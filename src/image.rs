//! Images referenced from a task's description.
//!
//! A description line may carry a markdown-style reference —
//! `![alt](~/shot.png)` — and mach draws the picture itself. Terminals
//! that speak kitty, iTerm2 or sixel graphics show the real image;
//! everywhere else it falls back to unicode half blocks.
//!
//! Animated GIFs play in the full-size preview (double-click / Enter).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

use image::codecs::gif::GifDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage};
use ratatui_image::FontSize;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;

const IMAGE_EXTENSIONS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];

/// Max long edge for stills in body / preview.
const MAX_STILL_EDGE: u32 = 1920;
/// Max long edge for GIF frames (encoded once per index).
const MAX_GIF_EDGE: u32 = 720;
/// Max frames decoded from one GIF.
const MAX_GIF_FRAMES: usize = 48;
/// Max decoded stills kept in the LRU cache.
const MAX_CACHE_ENTRIES: usize = 48;

/// Shrink so the longer side is at most `max_edge` (no-op when already smaller).
fn fit(img: DynamicImage, max_edge: u32) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    let edge = w.max(h);
    if edge <= max_edge {
        return img;
    }
    let scale = max_edge as f64 / edge as f64;
    let nw = ((w as f64) * scale).round().max(1.0) as u32;
    let nh = ((h as f64) * scale).round().max(1.0) as u32;
    img.resize(nw, nh, FilterType::Triangle)
}

/// Whether a string looks like an image path (extension check; no FS).
pub fn looks_like_image(text: &str) -> bool {
    let t = text.trim();
    if let Some((_, ext)) = t.rsplit_once('.') {
        let ext = ext.split(['/', '\\', '?', '#']).next().unwrap_or(ext);
        if IMAGE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
            return true;
        }
    }
    expand(t)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.iter().any(|x| e.eq_ignore_ascii_case(x)))
}

/// An existing image file named by `text`, if that is what it is.
pub fn path_if_image(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    if !looks_like_image(text) {
        return None;
    }
    let path = expand(text);
    path.is_file().then_some(path)
}

/// Resolves `~`, `file://` URLs and escaped spaces; the rest is left to
/// the filesystem.
pub fn expand(path: &str) -> PathBuf {
    let path = path.trim();
    let path = path.strip_prefix("file://").unwrap_or(path);
    let path = path.replace("%20", " ");
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// What the terminal says a cell is worth in pixels, when it will say.
///
/// Every size here is counted in cells and encoded as `cells × cell_size`
/// pixels, so this number has to be right or the terminal draws a picture
/// that does not fit the cells it was given and clips the overflow.
fn terminal_cell_size() -> Option<FontSize> {
    let ws = ratatui::crossterm::terminal::window_size().ok()?;
    // The pixel fields are optional in the ioctl; zero means "not told".
    if ws.width == 0 || ws.height == 0 || ws.columns == 0 || ws.rows == 0 {
        return None;
    }
    Some(FontSize::new(ws.width / ws.columns, ws.height / ws.rows))
}

fn tmux_without_passthrough() -> bool {
    if std::env::var_os("TMUX").is_none() {
        return false;
    }
    match std::process::Command::new("tmux")
        .args(["show", "-gv", "allow-passthrough"])
        .output()
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout).trim() != "on",
        Err(_) => true,
    }
}

/// Uppercase type label for a path (`GIF`, `PNG`, …).
pub fn type_label(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_uppercase())
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| "IMG".to_string())
}

pub fn is_gif(path: &Path) -> bool {
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("gif"))
    {
        return true;
    }
    // Sniff magic bytes — some temp clipboard paths omit a reliable suffix.
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::Read;
    let mut magic = [0u8; 6];
    matches!(f.read(&mut magic), Ok(6) if &magic == b"GIF87a" || &magic == b"GIF89a")
}

/// One decoded GIF ready to play in the full-size preview.
pub struct GifPlayback {
    frames: Vec<Arc<DynamicImage>>,
    delays: Vec<Duration>,
    index: usize,
    next_at: Instant,
    paused: bool,
}

impl GifPlayback {
    pub fn load(path: &Path) -> Result<Self, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let reader = std::io::BufReader::new(file);
        let decoder = GifDecoder::new(reader).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut frames = Vec::new();
        let mut delays = Vec::new();
        // Stream frames so a 500-frame meme does not decode entirely first.
        for (i, frame) in decoder.into_frames().enumerate() {
            if i >= MAX_GIF_FRAMES {
                break;
            }
            let frame = frame.map_err(|e| format!("{}: {e}", path.display()))?;
            // Delay is already a ratio of milliseconds — convert via Duration.
            let mut delay = Duration::from(frame.delay());
            // GIF delay of 0 is commonly treated as ~100ms.
            if delay.is_zero() {
                delay = Duration::from_millis(100);
            }
            // Floor so slow terminals can keep up; cap wild values.
            if delay < Duration::from_millis(40) {
                delay = Duration::from_millis(40);
            }
            if delay > Duration::from_secs(10) {
                delay = Duration::from_secs(10);
            }
            delays.push(delay);
            let rgba = DynamicImage::ImageRgba8(frame.into_buffer());
            frames.push(Arc::new(fit(rgba, MAX_GIF_EDGE)));
        }
        if frames.is_empty() {
            return Err(format!("{}: empty GIF", path.display()));
        }
        let delay0 = delays[0];
        Ok(Self {
            frames,
            delays,
            index: 0,
            next_at: Instant::now() + delay0,
            paused: false,
        })
    }

    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// 1-based index for the UI (`1/12`).
    pub fn frame_number(&self) -> usize {
        self.index + 1
    }

    pub fn is_animated(&self) -> bool {
        self.frames.len() > 1
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Toggle pause/resume. When resuming, the next frame is scheduled
    /// from now so playback does not jump.
    pub fn toggle_pause(&mut self) {
        if self.frames.len() <= 1 {
            return;
        }
        self.paused = !self.paused;
        if !self.paused {
            self.next_at = Instant::now() + self.delays[self.index];
        }
    }

    /// Advance when the current frame's delay has elapsed.
    pub fn tick(&mut self) -> bool {
        if self.paused || self.frames.len() <= 1 {
            return false;
        }
        let now = Instant::now();
        if now < self.next_at {
            return false;
        }
        self.index = (self.index + 1) % self.frames.len();
        // Schedule from *now* so a slow redraw does not skip many frames.
        self.next_at = now + self.delays[self.index];
        true
    }

    pub fn current(&self) -> &DynamicImage {
        &self.frames[self.index]
    }

    fn frame_arc(&self, idx: usize) -> Arc<DynamicImage> {
        Arc::clone(&self.frames[idx])
    }
}

/// One file: decoded pixels stay in RAM. Body and full-screen preview keep
/// separate protocols so closing the preview does not force a slow
/// re-encode the next time it opens (body is ~10 rows; preview is large).
struct CachedImage {
    image: Arc<DynamicImage>,
    protocol: Option<StatefulProtocol>,
    preview_protocol: Option<StatefulProtocol>,
}

/// Result of asking the store for a drawable protocol.
pub enum ImageReady<'a> {
    Ready(&'a mut StatefulProtocol),
    /// Decode still running on a worker thread.
    Loading,
    Failed(String),
}

/// Decoded images, kept so a redraw does not re-read the file.
#[derive(Default)]
pub struct ImageStore {
    picker: Option<Picker>,
    cache: HashMap<PathBuf, Result<CachedImage, String>>,
    /// LRU order of successful cache keys (front = oldest).
    lru: VecDeque<PathBuf>,
    /// In-flight background decodes.
    pending: HashMap<PathBuf, Receiver<Result<Arc<DynamicImage>, String>>>,
    /// Encoded GIF frames for the open preview (one encode per frame index).
    gif_protocols: Vec<Option<StatefulProtocol>>,
    gif_frame_count: usize,
}

/// [`FontSize`] carries no `PartialEq`.
fn same_cell(a: FontSize, b: FontSize) -> bool {
    a.width == b.width && a.height == b.height
}

impl ImageStore {
    /// Probe the terminal graphics protocol. Call before the alternate screen.
    /// Falls back to halfblocks if unsupported or under tmux without passthrough.
    pub fn detect() -> Self {
        let picker = if tmux_without_passthrough() {
            // Without `allow-passthrough on`, graphics escapes corrupt the screen.
            Picker::halfblocks()
        } else {
            Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
        };
        Self {
            picker: Some(picker),
            ..Self::default()
        }
    }

    fn decode(path: &Path) -> Result<Arc<DynamicImage>, String> {
        Ok(Arc::new(fit(load_dynamic(path)?, MAX_STILL_EDGE)))
    }

    fn touch_lru(&mut self, path: &Path) {
        self.lru.retain(|p| p != path);
        self.lru.push_back(path.to_path_buf());
    }

    fn evict_if_needed(&mut self) {
        while self.lru.len() > MAX_CACHE_ENTRIES {
            let Some(old) = self.lru.pop_front() else {
                break;
            };
            self.cache.remove(&old);
        }
    }

    fn insert_decoded(&mut self, path: PathBuf, result: Result<Arc<DynamicImage>, String>) {
        match result {
            Ok(image) => {
                self.cache.insert(
                    path.clone(),
                    Ok(CachedImage {
                        image,
                        protocol: None,
                        preview_protocol: None,
                    }),
                );
                self.touch_lru(&path);
                self.evict_if_needed();
            }
            Err(err) => {
                self.cache.insert(path, Err(err));
            }
        }
    }

    /// Start decoding `paths` on worker threads. Safe to call repeatedly;
    /// already-cached or in-flight paths are skipped. The form can open
    /// immediately while this runs.
    pub fn prefetch(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for path in paths {
            if self.cache.contains_key(&path) || self.pending.contains_key(&path) {
                continue;
            }
            let (tx, rx) = mpsc::channel();
            let path_bg = path.clone();
            std::thread::spawn(move || {
                let _ = tx.send(Self::decode(&path_bg));
            });
            self.pending.insert(path, rx);
        }
    }

    /// Pull finished background decodes into the cache.
    /// Returns true when at least one image became ready (caller should redraw).
    pub fn poll_pending(&mut self) -> bool {
        let keys: Vec<PathBuf> = self.pending.keys().cloned().collect();
        let mut any = false;
        for key in keys {
            let Some(rx) = self.pending.get(&key) else {
                continue;
            };
            match rx.try_recv() {
                Ok(result) => {
                    self.pending.remove(&key);
                    self.insert_decoded(key, result);
                    any = true;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.pending.remove(&key);
                    self.insert_decoded(key, Err("image load failed".into()));
                    any = true;
                }
            }
        }
        any
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// If the terminal cell size changed, drop encodings and rebuild.
    ///
    /// Needed when moving between displays of different DPI: the grid size
    /// is unchanged so there is no resize event, but pixel-per-cell is.
    /// Decoded bitmaps stay cached; only protocols are invalidated.
    pub fn recheck_cell_size(&mut self) -> bool {
        if self.cache.is_empty() && self.gif_protocols.is_empty() {
            return false;
        }
        // Halfblocks are cell glyphs, not pixel protocols.
        if self
            .picker
            .as_ref()
            .is_none_or(|p| p.protocol_type() == ProtocolType::Halfblocks)
        {
            return false;
        }
        let Some(cell) = terminal_cell_size() else {
            return false;
        };
        self.adopt_cell_size(cell)
    }

    /// Apply a new cell size if it differs; keep the startup protocol type.
    fn adopt_cell_size(&mut self, cell: FontSize) -> bool {
        let Some(picker) = self.picker.as_ref() else {
            return false;
        };
        if same_cell(picker.font_size(), cell) {
            return false;
        }
        let protocol = picker.protocol_type();
        #[allow(deprecated, reason = "the only way to set a Picker's font size")]
        let mut picker = Picker::from_fontsize(cell);
        picker.set_protocol_type(protocol);
        self.picker = Some(picker);
        self.release(true);
        true
    }

    /// The protocol for `path`, without blocking on disk I/O.
    ///
    /// Missing files are fetched in the background; the first call returns
    /// [`ImageReady::Loading`] until a later [`poll_pending`] lands them.
    pub fn get(&mut self, path: &Path) -> ImageReady<'_> {
        self.protocol_for(path, false)
    }

    /// Full-screen preview protocol — kept separate from the body thumb so
    /// open → close → open does not thrash encode size every time.
    pub fn get_preview(&mut self, path: &Path) -> ImageReady<'_> {
        self.protocol_for(path, true)
    }

    fn protocol_for(&mut self, path: &Path, preview: bool) -> ImageReady<'_> {
        // `poll_pending` is the event loop's job once per tick.
        if !self.cache.contains_key(path) {
            if !self.pending.contains_key(path) {
                self.prefetch(std::iter::once(path.to_path_buf()));
            }
            return ImageReady::Loading;
        }
        if self.cache.get(path).is_some_and(|e| e.is_err()) {
            let err = match self.cache.get(path) {
                Some(Err(e)) => e.clone(),
                _ => "image load failed".into(),
            };
            return ImageReady::Failed(err);
        }

        // Encode on first use, then keep it: body and preview hold their
        // own protocol so switching between them costs no re-encode.
        let need_encode = self.cache.get(path).is_some_and(|e| {
            e.as_ref().is_ok_and(|c| {
                if preview {
                    c.preview_protocol.is_none()
                } else {
                    c.protocol.is_none()
                }
            })
        });
        if need_encode {
            let image = match self.cache.get(path) {
                Some(Ok(c)) => Arc::clone(&c.image),
                _ => return ImageReady::Loading,
            };
            let Some(picker) = self.picker.as_mut() else {
                return ImageReady::Failed("no image support".into());
            };
            // new_resize_protocol needs an owned DynamicImage.
            let protocol = picker.new_resize_protocol((*image).clone());
            if let Some(Ok(cached)) = self.cache.get_mut(path) {
                if preview {
                    cached.preview_protocol = Some(protocol);
                } else {
                    cached.protocol = Some(protocol);
                }
            }
        }

        self.touch_lru(path);

        match self.cache.get_mut(path) {
            Some(Ok(cached)) => {
                let slot = if preview {
                    cached.preview_protocol.as_mut()
                } else {
                    cached.protocol.as_mut()
                };
                match slot {
                    Some(protocol) => ImageReady::Ready(protocol),
                    None => ImageReady::Failed("no image support".into()),
                }
            }
            Some(Err(err)) => ImageReady::Failed(err.clone()),
            None => ImageReady::Loading,
        }
    }

    /// Protocol for the current GIF frame (encode once per frame index).
    pub fn preview_frame(&mut self, gif: &GifPlayback) -> Result<&mut StatefulProtocol, String> {
        let idx = gif.index;
        let n = gif.frame_count();
        if self.gif_frame_count != n {
            self.gif_protocols = (0..n).map(|_| None).collect();
            self.gif_frame_count = n;
        }
        if self.gif_protocols[idx].is_none() {
            let picker = self.picker.as_mut().ok_or("no image support")?;
            let image = gif.frame_arc(idx);
            self.gif_protocols[idx] = Some(picker.new_resize_protocol((*image).clone()));
        }
        self.gif_protocols[idx]
            .as_mut()
            .ok_or_else(|| "no preview".into())
    }

    pub fn clear_preview(&mut self) {
        self.gif_protocols.clear();
        self.gif_frame_count = 0;
    }

    /// Drop the body protocols (the terminal deletes those pictures) but
    /// keep the decoded pixels. The next `get` rebuilds from RAM — no disk.
    ///
    /// Needed after a Clear over a graphics-protocol image, e.g. when the
    /// `/` menu closes.
    pub fn clear_cache(&mut self) {
        self.release(false);
    }

    /// Drop every placed protocol when leaving the task form.
    pub fn release_form_graphics(&mut self) {
        self.release(true);
    }

    fn release(&mut self, including_preview_protocols: bool) {
        for cached in self.cache.values_mut().flatten() {
            cached.protocol = None;
            if including_preview_protocols {
                cached.preview_protocol = None;
            }
        }
        self.clear_preview();
    }
}

/// Open and decode an image file with path-labeled errors.
pub fn load_dynamic(path: &Path) -> Result<DynamicImage, String> {
    image::ImageReader::open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .with_guessed_format()
        .map_err(|e| format!("{}: {e}", path.display()))?
        .decode()
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The `~`-relative form of a path, so bodies stay readable.
pub fn short(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_picture_extensions() {
        assert!(looks_like_image("/tmp/a.png"));
        assert!(
            looks_like_image("shot.JPEG"),
            "extension is case-insensitive"
        );
        assert!(!looks_like_image("notes.txt"));
        assert!(!looks_like_image("remember to send the png to Dana"));
    }

    #[test]
    fn expands_a_file_url_with_escaped_spaces() {
        assert_eq!(
            expand("file:///tmp/my%20shot.PNG"),
            PathBuf::from("/tmp/my shot.PNG")
        );
    }

    #[test]
    fn expands_a_home_relative_path() {
        let path = expand("~/pic.png");
        assert!(path.is_absolute() || dirs::home_dir().is_none());
        assert!(path.ends_with("pic.png"));
    }

    #[test]
    fn only_an_existing_file_counts_as_a_picture() {
        let real = concat!(env!("CARGO_MANIFEST_DIR"), "/assets/screenshot.png");
        assert!(path_if_image(real).is_some());
        assert!(path_if_image("/tmp/definitely-not-here.png").is_none());
        assert!(path_if_image(real.trim_end_matches(".png")).is_none());
    }

    /// A store holding one decoded image, so there is an encoding to lose.
    fn store_with_an_image() -> ImageStore {
        let mut store = ImageStore {
            picker: Some(Picker::halfblocks()),
            ..Default::default()
        };
        store.cache.insert(
            PathBuf::from("/tmp/x.png"),
            Ok(CachedImage {
                image: Arc::new(DynamicImage::new_rgba8(4, 4)),
                protocol: None,
                preview_protocol: None,
            }),
        );
        store
    }

    #[test]
    fn the_first_changed_reading_rebuilds_the_picker() {
        // The window is already showing a clipped picture by now, so this
        // must not wait to be sure.
        let mut store = store_with_an_image();
        let was = store.picker.as_ref().unwrap().font_size();
        let moved = FontSize::new(was.width * 2, was.height * 2);

        assert!(store.adopt_cell_size(moved));
        let now = store.picker.as_ref().unwrap().font_size();
        assert!(same_cell(now, moved), "the picker measures in the new size");
    }

    #[test]
    fn a_steady_cell_size_is_left_alone() {
        let mut store = store_with_an_image();
        let same = store.picker.as_ref().unwrap().font_size();
        for _ in 0..5 {
            assert!(
                !store.adopt_cell_size(same),
                "nothing moved, so nothing to re-encode"
            );
        }
    }

    #[test]
    fn one_move_costs_one_rebuild_however_often_it_is_polled() {
        let mut store = store_with_an_image();
        let was = store.picker.as_ref().unwrap().font_size();
        let moved = FontSize::new(was.width * 2, was.height * 2);

        let rebuilds = (0..20).filter(|_| store.adopt_cell_size(moved)).count();
        assert_eq!(rebuilds, 1);
    }

    #[test]
    fn nothing_cached_means_nothing_to_check() {
        let mut store = ImageStore {
            picker: Some(Picker::halfblocks()),
            ..Default::default()
        };
        let was = store.picker.as_ref().unwrap().font_size();
        assert!(!store.recheck_cell_size());
        assert!(
            same_cell(store.picker.as_ref().unwrap().font_size(), was),
            "the picker was never touched"
        );
    }

    #[test]
    fn half_blocks_are_never_re_measured() {
        // Their font size is a stand-in, not a measurement, so acting on a
        // real one would drop every encoding and change nothing on screen.
        let mut store = store_with_an_image();
        assert_eq!(
            store.picker.as_ref().unwrap().protocol_type(),
            ProtocolType::Halfblocks
        );
        let was = store.picker.as_ref().unwrap().font_size();
        assert!(!store.recheck_cell_size());
        assert!(same_cell(store.picker.as_ref().unwrap().font_size(), was));
    }
}

#[cfg(test)]
mod gif_tests {
    use super::*;
    use image::codecs::gif::GifEncoder;
    use image::{Delay, Frame, Rgba, RgbaImage};
    use std::fs::File;

    fn write_test_gif(path: &Path, n: u32) {
        let file = File::create(path).unwrap();
        let mut enc = GifEncoder::new(file);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite)
            .unwrap();
        for i in 0..n {
            let mut img = RgbaImage::new(8, 8);
            for p in img.pixels_mut() {
                *p = Rgba([((i * 80) % 255) as u8, 0, 255, 255]);
            }
            let delay = Delay::from_numer_denom_ms(50, 1);
            let frame = Frame::from_parts(img, 0, 0, delay);
            enc.encode_frame(frame).unwrap();
        }
    }

    #[test]
    fn loads_and_advances_multiple_gif_frames() {
        let dir = std::env::temp_dir().join("mach-gif-test");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("anim.gif");
        write_test_gif(&path, 4);
        assert!(is_gif(&path));
        let mut gif = GifPlayback::load(&path).expect("load gif");
        assert!(gif.frame_count() >= 2, "got {} frames", gif.frame_count());
        assert!(gif.is_animated());
        let first = gif.index;
        std::thread::sleep(Duration::from_millis(120));
        assert!(gif.tick(), "should advance after delay");
        assert_ne!(gif.index, first);
    }
}
