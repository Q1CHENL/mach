//! mach — a powerful yet easy-to-use todo TUI, built with ratatui.

pub mod app;
pub mod banner;
pub mod body;
pub mod cli;
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

use std::io;
use std::time::Duration;

use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;

use crate::app::App;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Entry point for the `mach` binary.
pub fn run() {
    cli::run();
}

pub fn run_tui() -> io::Result<()> {
    // Probe graphics support before the event loop takes stdin.
    // (ratatui-image prefers the alternate screen; answers are the same either way.)
    let images = image::ImageStore::detect();

    let mut terminal = ratatui::init();
    let mut out = io::stdout();
    let _ = execute!(out, EnableMouseCapture, EnableBracketedPaste);
    // Disambiguate Ctrl/Alt+arrows. Do not enable REPORT_ALL_KEYS_AS_ESCAPE_CODES
    // (breaks plain `/` on many terminals).
    let enhanced = execute!(
        out,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .is_ok();

    let mut app = App::new(VERSION);
    app.images = images;
    let result = event_loop(&mut terminal, &mut app);

    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut DefaultTerminal, app: &mut App) -> io::Result<()> {
    let mut last_clock = String::new();
    loop {
        // Input first so keys are not blocked behind GIF encode on draw.
        while event::poll(Duration::ZERO)? {
            input::handle_event(app, event::read()?);
            app.mark_dirty();
            if app.should_quit {
                app.flush_saves_now();
                return Ok(());
            }
        }
        let _ = app.expire_message();
        if app.poll_update_check() {
            app.mark_dirty();
        }
        if app.flush_saves() {
            app.mark_dirty();
        }
        // Cell pixel size can change without a resize event (e.g. move display).
        if app.images.recheck_cell_size() {
            app.mark_dirty();
        }
        if app.images.poll_pending() {
            app.mark_dirty();
        }

        let gif_advanced = app.form.as_mut().is_some_and(|f| f.tick_gif());
        if gif_advanced {
            app.mark_dirty();
        }
        let need_fast = app.form.as_ref().is_some_and(|f| f.gif_playing());

        let clock = crate::due::now_string(&app.settings.date_format);
        if clock != last_clock {
            last_clock = clock;
            app.mark_dirty();
        }

        if app.dirty {
            terminal.draw(|frame| ui::draw(frame, app))?;
            app.dirty = false;
        }

        let wait = if need_fast {
            Duration::from_millis(30)
        } else if app.images.has_pending() {
            Duration::from_millis(16)
        } else if app.tasks_dirty_pending() {
            Duration::from_millis(50)
        } else {
            Duration::from_millis(500)
        };
        let _ = event::poll(wait)?;
    }
}
