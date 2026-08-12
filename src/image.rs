//! Images referenced from a task's description.
//!
//! A description line may carry a markdown-style reference —
//! `![alt](~/shot.png)` — and mach draws the picture itself. Terminals
//! that speak kitty, iTerm2 or sixel graphics show the real image;
//! everywhere else it falls back to unicode half blocks.
//!
//! Animated GIFs play in the full-size preview (double-click / Enter).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use image::codecs::gif::GifDecoder;
use image::imageops::FilterType;
use image::{AnimationDecoder, DynamicImage, ImageDecoder, ImageFormat, Limits};
use ratatui_image::FontSize;
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;

const IMAGE_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "gif", "webp"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ManagedAttachmentFormat {
    pub extension: &'static str,
    pub media_type: &'static str,
}

const MANAGED_ATTACHMENT_FORMATS: [(ImageFormat, ManagedAttachmentFormat); 4] = [
    (
        ImageFormat::Png,
        ManagedAttachmentFormat {
            extension: "png",
            media_type: "image/png",
        },
    ),
    (
        ImageFormat::Jpeg,
        ManagedAttachmentFormat {
            extension: "jpg",
            media_type: "image/jpeg",
        },
    ),
    (
        ImageFormat::Gif,
        ManagedAttachmentFormat {
            extension: "gif",
            media_type: "image/gif",
        },
    ),
    (
        ImageFormat::WebP,
        ManagedAttachmentFormat {
            extension: "webp",
            media_type: "image/webp",
        },
    ),
];

pub(crate) fn managed_attachment_format(format: ImageFormat) -> Option<ManagedAttachmentFormat> {
    MANAGED_ATTACHMENT_FORMATS
        .iter()
        .find_map(|(candidate, metadata)| (*candidate == format).then_some(*metadata))
}

pub(crate) fn managed_attachment_format_for_media_type(
    media_type: &str,
) -> Option<ManagedAttachmentFormat> {
    MANAGED_ATTACHMENT_FORMATS
        .iter()
        .find_map(|(_, metadata)| (metadata.media_type == media_type).then_some(*metadata))
}

pub(crate) fn is_managed_attachment_extension(extension: &str) -> bool {
    MANAGED_ATTACHMENT_FORMATS
        .iter()
        .any(|(_, metadata)| metadata.extension == extension)
}

/// Max long edge for stills in description / preview.
const MAX_STILL_EDGE: u32 = 1920;
/// Max long edge for GIF frames (encoded once per index).
const MAX_GIF_EDGE: u32 = 720;
/// Max frames decoded from one GIF.
const MAX_GIF_FRAMES: usize = 48;
/// Max successful or failed decode outcomes kept in the LRU cache.
const MAX_CACHE_ENTRIES: usize = 48;
/// Aggregate decoded-pixel budget for still images. Protocol encodings are
/// released with the form; decoded pixels are the persistent cache cost.
const MAX_CACHE_BYTES: usize = 128 * 1024 * 1024;
/// Reject implausibly large canvases before a decoder allocates them.
const MAX_DECODE_DIMENSION: u32 = 8192;
const MAX_DECODE_ALLOC: u64 = 128 * 1024 * 1024;
/// Decode work is CPU and memory heavy; keep the UI responsive under a description
/// containing many images instead of spawning one thread per path.
const MAX_DECODE_WORKERS: usize = 2;

/// A private PNG staged from clipboard pixels for the lifetime of an open
/// form. Saving imports it into the content-addressed store; cancelling or
/// closing the form removes the source file.
#[derive(Debug)]
pub(crate) struct TemporaryImage {
    path: PathBuf,
}

impl TemporaryImage {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TemporaryImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Validate RGBA clipboard pixels and encode them into a private temporary
/// PNG. The returned owner keeps the source alive until a form is saved or
/// dismissed.
pub(crate) fn stage_clipboard_image(
    image: arboard::ImageData<'_>,
) -> Result<TemporaryImage, String> {
    use image::ImageEncoder;
    use std::io::Write;

    let width = u32::try_from(image.width)
        .map_err(|_| "clipboard image width exceeds the supported limit".to_string())?;
    let height = u32::try_from(image.height)
        .map_err(|_| "clipboard image height exceeds the supported limit".to_string())?;
    if width == 0 || height == 0 {
        return Err("clipboard image dimensions must be nonzero".into());
    }
    if width > MAX_DECODE_DIMENSION || height > MAX_DECODE_DIMENSION {
        return Err(format!(
            "clipboard image dimensions exceed the {MAX_DECODE_DIMENSION}-pixel safety limit"
        ));
    }
    let expected = image
        .width
        .checked_mul(image.height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "clipboard image dimensions overflow its pixel buffer".to_string())?;
    if expected != image.bytes.len() {
        return Err(format!(
            "clipboard image pixel buffer has {} bytes; expected {expected}",
            image.bytes.len()
        ));
    }
    if expected as u64 > MAX_DECODE_ALLOC {
        return Err(format!(
            "clipboard image pixel buffer exceeds the {} MiB safety limit",
            MAX_DECODE_ALLOC / 1024 / 1024
        ));
    }

    let path = std::env::temp_dir().join(format!("mach-clipboard-{}.png", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("could not create clipboard image staging file: {error}"))?;
    let encoded = (|| {
        image::codecs::png::PngEncoder::new(&mut file)
            .write_image(&image.bytes, width, height, image::ColorType::Rgba8.into())
            .map_err(|error| format!("could not encode clipboard image as PNG: {error}"))?;
        file.flush()
            .map_err(|error| format!("could not flush clipboard image: {error}"))?;
        let byte_len = file
            .metadata()
            .map_err(|error| format!("could not inspect clipboard image: {error}"))?
            .len();
        if byte_len > crate::store::MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "clipboard image exceeds the {} MiB attachment limit",
                crate::store::MAX_ATTACHMENT_BYTES / 1024 / 1024
            ));
        }
        Ok(())
    })();
    drop(file);
    if let Err(error) = encoded {
        let _ = std::fs::remove_file(&path);
        return Err(error);
    }
    Ok(TemporaryImage { path })
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AttachmentCatalog {
    files: HashMap<String, String>,
}

impl AttachmentCatalog {
    pub fn set(&mut self, attachments: &[crate::store::Attachment]) {
        self.files = attachments
            .iter()
            .map(|attachment| (attachment.id.clone(), attachment.storage_name.clone()))
            .collect();
    }

    pub fn resolve(&self, reference: &str, images_root: &Path) -> PathBuf {
        self.files
            .get(reference)
            .map(|storage_name| images_root.join(storage_name))
            .unwrap_or_else(|| expand_in(reference, images_root))
    }

    pub fn contains(&self, reference: &str) -> bool {
        self.files.contains_key(reference)
    }
}

fn decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECODE_DIMENSION);
    limits.max_image_height = Some(MAX_DECODE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    limits
}

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
    let Some(t) = reference_path(text) else {
        return false;
    };
    if let Some((_, ext)) = t.rsplit_once('.') {
        let ext = ext.split(['/', '\\', '?', '#']).next().unwrap_or(ext);
        if IMAGE_EXTENSIONS.iter().any(|e| ext.eq_ignore_ascii_case(e)) {
            return true;
        }
    }
    false
}

/// An existing image file named by `text`, if that is what it is.
pub fn path_if_image(text: &str) -> Option<PathBuf> {
    path_if_image_in(text, &default_images_root())
}

/// Resolve a raw or Markdown image reference against an explicit images root.
/// Relative paths never depend on the process working directory.
pub fn path_if_image_in(text: &str, images_root: &Path) -> Option<PathBuf> {
    let reference = reference_path(text)?;
    if !looks_like_image(reference) {
        return None;
    }
    let path = expand_in(reference, images_root);
    path.is_file().then_some(path)
}

/// Resolves `~`, `file://` URLs and escaped spaces; the rest is left to
/// the filesystem.
pub fn expand(path: &str) -> PathBuf {
    expand_in(path, &default_images_root())
}

pub fn expand_in(path: &str, images_root: &Path) -> PathBuf {
    let path = reference_path(path).unwrap_or(path).trim();
    let path = path.strip_prefix("file://").unwrap_or(path);
    let path = path.replace("%20", " ");
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    let path = PathBuf::from(path);
    if path.is_absolute() {
        path
    } else {
        images_root.join(path)
    }
}

/// Extract the path from either `path.png` or `![alt](path.png)`.
pub(crate) fn reference_path(text: &str) -> Option<&str> {
    let text = text.trim();
    if !text.starts_with("![") {
        return (!text.is_empty()).then_some(text);
    }
    let close = text.find("](")?;
    if !text.ends_with(')') || close + 2 >= text.len() - 1 {
        return None;
    }
    Some(text[close + 2..text.len() - 1].trim())
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
        let mut decoder =
            GifDecoder::new(reader).map_err(|e| format!("{}: {e}", path.display()))?;
        decoder
            .set_limits(decode_limits())
            .map_err(|e| format!("{}: {e}", path.display()))?;
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

struct GifJob {
    path: PathBuf,
    result: Sender<Result<GifPlayback, String>>,
}

fn gif_worker() -> &'static SyncSender<GifJob> {
    static WORKER: OnceLock<SyncSender<GifJob>> = OnceLock::new();
    WORKER.get_or_init(|| {
        // One active decode and at most one queued request. Forms can be
        // opened/closed faster than a large GIF decodes; an unbounded queue
        // would otherwise keep obsolete work alive long after the UI moved on.
        let (jobs, receiver) = mpsc::sync_channel::<GifJob>(1);
        let _ = std::thread::Builder::new()
            .name("mach-gif-decode".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    let _ = job.result.send(GifPlayback::load(&job.path));
                }
            });
        jobs
    })
}

/// One asynchronous GIF decode owned by the open form. Jobs share one process
/// worker, so rapidly changing previews cannot create unbounded decode threads.
pub struct GifLoad {
    path: PathBuf,
    receiver: Receiver<Result<GifPlayback, String>>,
}

impl GifLoad {
    pub fn start(path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel();
        let job = GifJob {
            path: path.clone(),
            result: sender,
        };
        match gif_worker().try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => {
                let _ = job.result.send(Err(
                    "GIF decoder is busy; try opening the image again".into()
                ));
            }
            Err(TrySendError::Disconnected(job)) => {
                let _ = job
                    .result
                    .send(Err("GIF decode worker stopped".to_string()));
            }
        }
        Self { path, receiver }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn poll(&self) -> Option<Result<GifPlayback, String>> {
        match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(Err("GIF load failed".to_string())),
        }
    }
}

/// One file: decoded pixels stay in RAM. Description and full-screen preview keep
/// separate protocols so closing the preview does not force a slow
/// re-encode the next time it opens (description is ~10 rows; preview is large).
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
pub struct ImageStore {
    images_root: PathBuf,
    attachments: AttachmentCatalog,
    picker: Option<Picker>,
    cache: HashMap<PathBuf, Result<CachedImage, String>>,
    cache_bytes: usize,
    cache_budget: usize,
    /// LRU order of cache keys (front = oldest).
    lru: VecDeque<PathBuf>,
    /// In-flight background decodes.
    pending: HashMap<PathBuf, Receiver<Result<Arc<DynamicImage>, String>>>,
    /// FIFO work waiting for one of the bounded decode slots.
    queued: VecDeque<PathBuf>,
    queued_paths: HashSet<PathBuf>,
    /// Encoded GIF frames for the open preview (one encode per frame index).
    gif_protocols: Vec<Option<StatefulProtocol>>,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self {
            images_root: default_images_root(),
            attachments: AttachmentCatalog::default(),
            picker: None,
            cache: HashMap::new(),
            cache_bytes: 0,
            cache_budget: MAX_CACHE_BYTES,
            lru: VecDeque::new(),
            pending: HashMap::new(),
            queued: VecDeque::new(),
            queued_paths: HashSet::new(),
            gif_protocols: Vec::new(),
        }
    }
}

/// [`FontSize`] carries no `PartialEq`.
fn same_cell(a: FontSize, b: FontSize) -> bool {
    a.width == b.width && a.height == b.height
}

impl ImageStore {
    pub fn with_root(images_root: PathBuf) -> Self {
        Self {
            images_root,
            ..Self::default()
        }
    }

    pub fn set_root(&mut self, images_root: PathBuf) {
        if self.images_root != images_root {
            self.images_root = images_root;
            self.cache.clear();
            self.cache_bytes = 0;
            self.lru.clear();
            self.pending.clear();
            self.queued.clear();
            self.queued_paths.clear();
            self.release(true);
        }
    }

    pub fn root(&self) -> &Path {
        &self.images_root
    }

    pub fn set_attachments(&mut self, attachments: &[crate::store::Attachment]) {
        self.attachments.set(attachments);
    }

    pub fn resolve(&self, reference: &str) -> PathBuf {
        self.attachments.resolve(reference, &self.images_root)
    }

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
        while self.lru.len() > MAX_CACHE_ENTRIES || self.cache_bytes > self.cache_budget {
            let Some(old) = self.lru.pop_front() else {
                break;
            };
            if let Some(cached) = self.cache.remove(&old)
                && let Ok(cached) = cached
            {
                self.cache_bytes = self
                    .cache_bytes
                    .saturating_sub(cached.image.as_bytes().len());
            }
        }
    }

    fn insert_decoded(&mut self, path: PathBuf, result: Result<Arc<DynamicImage>, String>) {
        self.lru.retain(|cached| cached != &path);
        if let Some(Ok(cached)) = self.cache.remove(&path) {
            self.cache_bytes = self
                .cache_bytes
                .saturating_sub(cached.image.as_bytes().len());
        }
        let cached = match result {
            Ok(image) => {
                let bytes = image.as_bytes().len();
                if bytes > self.cache_budget {
                    Err(format!(
                        "decoded image is {bytes} bytes; cache limit is {} bytes",
                        self.cache_budget
                    ))
                } else {
                    self.cache_bytes = self.cache_bytes.saturating_add(bytes);
                    Ok(CachedImage {
                        image,
                        protocol: None,
                        preview_protocol: None,
                    })
                }
            }
            Err(error) => Err(error),
        };
        self.cache.insert(path.clone(), cached);
        self.touch_lru(&path);
        self.evict_if_needed();
    }

    /// Start decoding `paths` on worker threads. Safe to call repeatedly;
    /// already-cached or in-flight paths are skipped. The form can open
    /// immediately while this runs.
    pub fn prefetch(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        for path in paths {
            if self.cache.contains_key(&path) || self.pending.contains_key(&path) {
                continue;
            }
            if self.queued_paths.insert(path.clone()) {
                self.queued.push_back(path);
            }
        }
        self.start_queued();
    }

    fn start_queued(&mut self) {
        while self.pending.len() < MAX_DECODE_WORKERS {
            let Some(path) = self.queued.pop_front() else {
                break;
            };
            self.queued_paths.remove(&path);
            let (tx, rx) = mpsc::channel();
            let path_bg = path.clone();
            match std::thread::Builder::new()
                .name("mach-image-decode".into())
                .spawn(move || {
                    let _ = tx.send(Self::decode(&path_bg));
                }) {
                Ok(_) => {
                    self.pending.insert(path, rx);
                }
                Err(error) => self
                    .insert_decoded(path, Err(format!("could not start image decoder: {error}"))),
            }
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
        self.start_queued();
        any
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty() || !self.queued.is_empty()
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
    /// [`ImageReady::Loading`] until a later [`Self::poll_pending`] lands them.
    pub fn get(&mut self, path: &Path) -> ImageReady<'_> {
        self.protocol_for(path, false)
    }

    /// Full-screen preview protocol — kept separate from the description thumb so
    /// open → close → open does not thrash encode size every time.
    pub fn get_preview(&mut self, path: &Path) -> ImageReady<'_> {
        self.protocol_for(path, true)
    }

    fn protocol_for(&mut self, path: &Path, preview: bool) -> ImageReady<'_> {
        // `poll_pending` is the event loop's job once per tick.
        match self.cache.get(path) {
            None => {
                if !self.pending.contains_key(path) {
                    self.prefetch(std::iter::once(path.to_path_buf()));
                }
                return ImageReady::Loading;
            }
            Some(Err(error)) => return ImageReady::Failed(error.clone()),
            Some(Ok(_)) => {}
        }
        self.touch_lru(path);

        let Self { cache, picker, .. } = self;
        let Some(Ok(CachedImage {
            image,
            protocol,
            preview_protocol,
        })) = cache.get_mut(path)
        else {
            return ImageReady::Loading;
        };

        // Description and preview hold separate protocols so switching between them
        // does not re-encode the decoded image.
        let slot = if preview { preview_protocol } else { protocol };
        let protocol = match slot {
            Some(protocol) => protocol,
            empty @ None => {
                let Some(picker) = picker.as_mut() else {
                    return ImageReady::Failed("no image support".into());
                };
                empty.insert(picker.new_resize_protocol((**image).clone()))
            }
        };
        ImageReady::Ready(protocol)
    }

    /// Protocol for the current GIF frame (encode once per frame index).
    pub fn preview_frame(&mut self, gif: &GifPlayback) -> Result<&mut StatefulProtocol, String> {
        let idx = gif.index;
        let n = gif.frame_count();
        if self.gif_protocols.len() != n {
            self.gif_protocols = (0..n).map(|_| None).collect();
        }
        match &mut self.gif_protocols[idx] {
            Some(protocol) => Ok(protocol),
            slot @ None => {
                let picker = self.picker.as_mut().ok_or("no image support")?;
                let image = gif.frame_arc(idx);
                Ok(slot.insert(picker.new_resize_protocol((*image).clone())))
            }
        }
    }

    pub fn clear_preview(&mut self) {
        self.gif_protocols.clear();
    }

    /// Drop the description protocols (the terminal deletes those pictures) but
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
    let mut reader = image::ImageReader::open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .with_guessed_format()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    reader.limits(decode_limits());
    reader
        .decode()
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The `~`-relative form of a path, so bodies stay readable.
pub fn short(path: &Path) -> String {
    short_in(path, &default_images_root())
}

/// Stable fallback used by standalone editor tests. Production injects the
/// active store's images directory into [`ImageStore`] and
/// [`crate::description::DescriptionEditor`].
pub fn default_images_root() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".mach"))
        .unwrap_or_else(|| std::env::temp_dir().join("mach"))
        .join("images")
}

pub fn short_in(path: &Path, images_root: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(images_root)
        && !relative.as_os_str().is_empty()
    {
        return relative.display().to_string();
    }
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
        assert!(looks_like_image("![diagram](architecture.png)"));
        assert!(
            !looks_like_image("legacy.bmp"),
            "BMP is not compiled into the decoder"
        );
    }

    #[test]
    fn resolves_relative_references_against_an_injected_images_root() {
        let root = std::env::temp_dir().join(format!("mach-image-root-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let path = root.join("diagram.png");
        std::fs::write(&path, b"not decoded in this test").unwrap();

        assert_eq!(
            path_if_image_in("![diagram](diagram.png)", &root),
            Some(path)
        );
    }

    #[test]
    fn clipboard_pixels_are_staged_as_a_private_temporary_png() {
        use std::borrow::Cow;

        let staged = stage_clipboard_image(arboard::ImageData {
            width: 2,
            height: 1,
            bytes: Cow::Owned(vec![255, 0, 0, 255, 0, 255, 0, 255]),
        })
        .expect("stage clipboard image");
        let path = staged.path().to_path_buf();
        let decoded = load_dynamic(&path).expect("decode staged PNG");
        assert_eq!((decoded.width(), decoded.height()), (2, 1));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        drop(staged);
        assert!(!path.exists(), "form-owned clipboard staging must clean up");
    }

    #[test]
    fn malformed_clipboard_pixel_buffers_are_rejected_before_encoding() {
        use std::borrow::Cow;

        let error = stage_clipboard_image(arboard::ImageData {
            width: 2,
            height: 2,
            bytes: Cow::Owned(vec![0; 4]),
        })
        .expect_err("RGBA buffer is shorter than its dimensions");
        assert!(error.contains("pixel buffer"), "{error}");
    }

    #[test]
    fn resolves_attachment_ids_through_the_managed_catalog() {
        let root = PathBuf::from("/tmp/mach-managed-images");
        let id = "a".repeat(64);
        let attachment = crate::store::Attachment {
            id: id.clone(),
            sha256: id.clone(),
            media_type: "image/png".into(),
            byte_len: 12,
            storage_name: format!("{id}.png"),
        };
        let mut store = ImageStore::with_root(root.clone());
        store.set_attachments(&[attachment]);

        assert_eq!(store.resolve(&id), root.join(format!("{id}.png")));
        assert_eq!(store.resolve("draft.png"), root.join("draft.png"));
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
    fn prefetch_uses_a_bounded_number_of_decode_workers() {
        let mut store = ImageStore::default();
        let paths = (0..8).map(|index| PathBuf::from(format!("/missing/{index}.png")));
        store.prefetch(paths);
        assert!(store.pending.len() <= MAX_DECODE_WORKERS);
        assert_eq!(store.pending.len() + store.queued.len(), 8);
    }

    #[test]
    fn decoded_cache_evicts_by_bytes_before_entry_count() {
        let mut store = ImageStore {
            cache_budget: 32,
            ..ImageStore::default()
        };
        let image = || Arc::new(DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2)));
        let paths: Vec<_> = (0..3)
            .map(|index| PathBuf::from(format!("small-{index}.png")))
            .collect();

        for path in &paths {
            store.insert_decoded(path.clone(), Ok(image()));
        }

        assert_eq!(store.cache_bytes, 32);
        assert!(!store.cache.contains_key(&paths[0]));
        assert!(store.cache.contains_key(&paths[1]));
        assert!(store.cache.contains_key(&paths[2]));
    }

    #[test]
    fn one_image_larger_than_the_cache_budget_is_reported_not_cached() {
        let mut store = ImageStore {
            cache_budget: 15,
            ..ImageStore::default()
        };
        let path = PathBuf::from("too-large.png");
        let image = Arc::new(DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2)));

        store.insert_decoded(path.clone(), Ok(image));

        assert_eq!(store.cache_bytes, 0);
        assert!(matches!(
            store.cache.get(&path),
            Some(Err(error)) if error.contains("cache limit")
        ));
    }

    #[test]
    fn failed_decodes_share_the_cache_entry_limit() {
        let mut store = ImageStore::default();
        for index in 0..(MAX_CACHE_ENTRIES + 12) {
            store.insert_decoded(
                PathBuf::from(format!("missing-{index}.png")),
                Err("missing".into()),
            );
        }

        assert_eq!(store.cache.len(), MAX_CACHE_ENTRIES);
        assert_eq!(store.lru.len(), MAX_CACHE_ENTRIES);
    }

    #[test]
    fn oversized_canvas_is_rejected_from_still_and_gif_decoders() {
        let path =
            std::env::temp_dir().join(format!("mach-oversized-{}.gif", uuid::Uuid::new_v4()));
        // Valid one-frame GIF with its logical canvas patched to 8193×1. The
        // strict dimension check runs before a canvas buffer can be allocated.
        std::fs::write(
            &path,
            [
                b'G', b'I', b'F', b'8', b'9', b'a', 0x01, 0x20, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00,
                0x00, 0x00, 0xff, 0xff, 0xff, 0x21, 0xf9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00, 0x2c,
                0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00,
                0x3b,
            ],
        )
        .unwrap();

        let still = load_dynamic(&path).expect_err("still decoder must enforce dimensions");
        let gif = match GifPlayback::load(&path) {
            Err(error) => error,
            Ok(_) => panic!("GIF decoder must enforce dimensions"),
        };
        assert!(
            still.to_lowercase().contains("limit") || still.to_lowercase().contains("dimension"),
            "{still}"
        );
        assert!(
            gif.to_lowercase().contains("limit") || gif.to_lowercase().contains("dimension"),
            "{gif}"
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
