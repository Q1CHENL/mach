//! 256-colour palette and the styles the UI is built from.

use ratatui::style::{Color, Modifier, Style};

pub fn color(name: &str) -> Color {
    Color::Indexed(match name {
        "red" => 160,
        "yellow" => 226,
        "green" => 41,
        "cyan" => 37,
        "blue" => 39,
        "purple" => 141,
        "white" => 231,
        "black" => 234,
        "grey" => 244,
        _ => 39,
    })
}

pub const RED: Color = Color::Indexed(196);
pub const GREEN: Color = Color::Indexed(41);
pub const GREY: Color = Color::Indexed(244);

/// Dim a 256-colour code by one step in each RGB component, mirroring the
/// dimming used for completed tasks that still carry a due date.
pub fn dimmed(c: Color) -> Color {
    let Color::Indexed(code) = c else { return c };
    let dim = match code {
        0..=15 => {
            if code >= 8 {
                code - 8
            } else {
                code
            }
        }
        232.. => code.saturating_sub(4).max(232),
        _ => {
            let base = code - 16;
            let (r, g, b) = (base / 36, (base % 36) / 6, base % 6);
            16 + 36 * r.saturating_sub(1) + 6 * g.saturating_sub(1) + b.saturating_sub(1)
        }
    };
    Color::Indexed(dim)
}

/// A wash of the accent: the same hue at about a quarter intensity,
/// lifted off black so text — including the muted grey of a finished
/// task — stays legible on top of it.
///
/// The 256-colour cube cannot express this: its darkest non-zero step is
/// already 95, which is bright enough to fight mid-grey text. So the
/// wash is given in RGB, which every terminal mach draws pictures in
/// supports anyway.
pub fn tint(color: Color) -> Color {
    let (r, g, b) = rgb_of(color);
    let wash = |c: u8| (c / 4).saturating_add(8);
    Color::Rgb(wash(r), wash(g), wash(b))
}

/// The RGB behind a palette index.
fn rgb_of(color: Color) -> (u8, u8, u8) {
    let code = match color {
        Color::Rgb(r, g, b) => return (r, g, b),
        Color::Indexed(code) => code,
        _ => return (0, 0, 0),
    };
    match code {
        // The 6x6x6 cube, whose steps are 0 then 95 and up by 40.
        16..232 => {
            let level = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
            let base = code - 16;
            (level(base / 36), level((base % 36) / 6), level(base % 6))
        }
        // The grayscale ramp.
        232.. => {
            let v = 8 + (code - 232) * 10;
            (v, v, v)
        }
        _ => (0, 0, 0),
    }
}

pub struct Theme {
    pub accent: Color,
}

impl Theme {
    pub fn new(name: &str) -> Self {
        Self {
            accent: color(name),
        }
    }

    /// Style of the selected row: a wash of the accent behind it, with
    /// the text keeping whatever colour it already had.
    pub fn selection(&self) -> Style {
        Style::new()
            .bg(tint(self.accent))
            .add_modifier(Modifier::BOLD)
    }

    /// Style of the selected row when its panel does not have focus.
    /// Keeps the accent wash so the active category stays obvious while
    /// the Tasks panel has keyboard focus; bold is reserved for focus.
    pub fn selection_unfocused(&self) -> Style {
        Style::new().bg(tint(self.accent))
    }

    pub fn accent_text(&self) -> Style {
        Style::new().fg(self.accent)
    }

    pub fn plain(&self) -> Style {
        Style::new()
    }
}
