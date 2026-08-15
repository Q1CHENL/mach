//! mach — a powerful yet easy-to-use todo TUI, built with ratatui.

pub mod app;
mod archive;
pub mod banner;
pub mod cli;
pub mod description;
pub mod due;
pub mod duepicker;
pub mod form;
pub mod fuzzy;
pub mod image;
pub mod input;
pub mod model;
pub mod open;
pub mod settings;
pub mod slash;
pub mod store;
pub mod text_input;
pub mod theme;
pub mod ui;
pub mod undo;
pub mod update;
mod update_state;

use std::io::{self, IsTerminal};
use std::time::{Duration, Instant};

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;

use crate::app::App;
use crate::store::Store;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const HOUSEKEEPING_INTERVAL: Duration = Duration::from_millis(500);
const GIF_WAIT: Duration = Duration::from_millis(30);
const IMAGE_WAIT: Duration = Duration::from_millis(16);
const BACKGROUND_WAIT: Duration = Duration::from_millis(100);

/// Entry point for the `mach` binary.
pub fn run() {
    cli::run();
}

pub(crate) fn require_interactive_terminal() -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "an interactive terminal is required on stdin and stdout; use a CLI subcommand for scripts",
        ));
    }
    Ok(())
}

pub fn run_tui(store: Store) -> io::Result<()> {
    require_interactive_terminal()?;

    // Load and validate persistent state before changing terminal modes. A bad
    // data file must never strand the user's shell in the alternate screen.
    let images_root = store.images_dir().to_path_buf();
    let mut app = App::with_store_and_update_state(
        VERSION,
        store,
        update_state::UpdateStateStore::open_default(),
    )
    .map_err(io::Error::other)?;

    // Probe graphics support before the event loop takes stdin.
    // (ratatui-image prefers the alternate screen; answers are the same either way.)
    let mut images = image::ImageStore::detect();
    images.set_root(images_root);
    images.set_attachments(&app.attachments);
    app.images = images;

    let (mut terminal, _session) = TerminalSession::enter()?;
    app.record_launch().map_err(io::Error::other)?;
    app.poll_automatic_update_schedule();
    let result = event_loop(&mut terminal, &mut app);
    app.shutdown_background_work();
    result
}

/// Owns every terminal mode enabled by mach. Keeping cleanup in `Drop` makes
/// normal errors and unwinding follow the same restoration path.
struct TerminalSession {
    enhanced_keyboard: bool,
}

impl TerminalSession {
    fn enter() -> io::Result<(DefaultTerminal, Self)> {
        let terminal = match ratatui::try_init() {
            Ok(terminal) => terminal,
            Err(error) => {
                // `try_init` can fail after raw mode or the alternate screen
                // was enabled, so unwind any partial initialization too.
                let _ = ratatui::try_restore();
                return Err(error);
            }
        };
        let mut session = Self {
            enhanced_keyboard: false,
        };
        let mut out = io::stdout();
        execute!(out, EnableMouseCapture, EnableBracketedPaste)?;
        // Disambiguate Ctrl/Alt+arrows. Do not enable
        // REPORT_ALL_KEYS_AS_ESCAPE_CODES (it breaks plain `/` in many
        // terminals).
        session.enhanced_keyboard = execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
        Ok((terminal, session))
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let mut out = io::stdout();
        if self.enhanced_keyboard {
            let _ = execute!(out, PopKeyboardEnhancementFlags);
        }
        let _ = execute!(out, DisableBracketedPaste, DisableMouseCapture);
        let _ = ratatui::try_restore();
    }
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    const MAX_EVENTS_PER_TICK: usize = 64;

    let mut last_clock = String::new();
    let mut next_housekeeping = Instant::now();
    loop {
        // Input first so keys are not blocked behind GIF encode on draw.
        // Bound each batch so a continuous mouse/key stream cannot starve
        // drawing, persistence polling, image work, or update completion.
        for _ in 0..MAX_EVENTS_PER_TICK {
            if !event::poll(Duration::ZERO)? {
                break;
            }
            if input::handle_event(app, event::read()?) {
                app.mark_dirty();
            }
            if app.should_quit {
                return Ok(());
            }
        }
        let _ = app.expire_message();
        if app.poll_update() {
            app.mark_dirty();
        }
        if app.poll_archive() {
            app.mark_dirty();
        }
        if app.should_quit {
            return Ok(());
        }
        let now = Instant::now();
        if housekeeping_due(&mut next_housekeeping, now) {
            if app.poll_automatic_update_schedule() {
                app.mark_dirty();
            }
            if app.poll_external_changes() {
                app.mark_dirty();
            }
            // Cell pixel size can change without a resize event (e.g. move display).
            if app.images.recheck_cell_size() {
                app.mark_dirty();
            }
            let clock = crate::due::now_string(&app.settings.date_format);
            if clock != last_clock {
                last_clock = clock;
                app.mark_dirty();
            }
        }
        if app.images.poll_pending() {
            app.mark_dirty();
        }

        let gif_advanced = app.form.as_mut().is_some_and(|f| f.tick_gif());
        if gif_advanced {
            app.mark_dirty();
        }
        let need_fast = app.form.as_ref().is_some_and(|f| f.gif_playing());

        if app.dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
            app.dirty = false;
        }

        let until_housekeeping = next_housekeeping.saturating_duration_since(Instant::now());
        let wait = loop_wait(
            need_fast,
            app.images.has_pending(),
            app.background_work_active(),
            until_housekeeping,
        );
        let _ = event::poll(wait)?;
    }
}

fn housekeeping_due(next: &mut Instant, now: Instant) -> bool {
    if now < *next {
        return false;
    }
    *next = now + HOUSEKEEPING_INTERVAL;
    true
}

fn loop_wait(
    need_fast: bool,
    images_pending: bool,
    background_active: bool,
    until_housekeeping: Duration,
) -> Duration {
    let activity_wait = if need_fast {
        GIF_WAIT
    } else if images_pending {
        IMAGE_WAIT
    } else if background_active {
        BACKGROUND_WAIT
    } else {
        HOUSEKEEPING_INTERVAL
    };
    activity_wait.min(until_housekeeping)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::{BACKGROUND_WAIT, HOUSEKEEPING_INTERVAL, housekeeping_due, loop_wait};

    #[test]
    fn housekeeping_runs_immediately_then_on_its_interval() {
        let start = Instant::now();
        let mut next = start;

        assert!(housekeeping_due(&mut next, start));
        assert_eq!(next.duration_since(start), HOUSEKEEPING_INTERVAL);
        assert!(!housekeeping_due(
            &mut next,
            start + HOUSEKEEPING_INTERVAL - Duration::from_millis(1),
        ));
        assert!(housekeeping_due(&mut next, start + HOUSEKEEPING_INTERVAL,));
    }

    #[test]
    fn housekeeping_deadline_caps_animation_and_idle_waits() {
        assert_eq!(
            loop_wait(true, false, false, Duration::from_millis(10)),
            Duration::from_millis(10),
        );
        assert_eq!(
            loop_wait(true, false, false, Duration::from_millis(200)),
            Duration::from_millis(30),
        );
        assert_eq!(
            loop_wait(false, false, false, Duration::from_millis(200)),
            Duration::from_millis(200),
        );
        assert_eq!(
            loop_wait(false, false, true, Duration::from_millis(500)),
            BACKGROUND_WAIT,
        );
    }
}
