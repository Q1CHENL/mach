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
    high_contrast_selection: bool,
    colors_disabled: bool,
}

impl Theme {
    pub fn new(name: &str) -> Self {
        let colors_disabled = std::env::var_os("NO_COLOR").is_some()
            || std::env::var("TERM").is_ok_and(|term| term == "dumb");
        let light = terminal_background_is_light();
        Self::with_environment(name, colors_disabled, light)
    }

    pub fn with_environment(name: &str, colors_disabled: bool, light_background: bool) -> Self {
        let accent = if colors_disabled {
            Color::Reset
        } else if light_background {
            Color::Indexed(match name {
                "red" => 124,
                "yellow" => 136,
                "green" => 28,
                "cyan" => 30,
                "blue" => 25,
                "purple" => 91,
                "white" => 238,
                "black" => 16,
                _ => 25,
            })
        } else {
            color(name)
        };
        Self {
            accent,
            high_contrast_selection: colors_disabled || light_background,
            colors_disabled,
        }
    }

    pub fn muted_color(&self) -> Color {
        if self.colors_disabled {
            Color::Reset
        } else {
            GREY
        }
    }

    pub fn error_color(&self) -> Color {
        if self.colors_disabled {
            Color::Reset
        } else {
            RED
        }
    }

    pub fn success_color(&self) -> Color {
        if self.colors_disabled {
            Color::Reset
        } else {
            GREEN
        }
    }

    pub fn selection_wash(&self) -> Color {
        if self.colors_disabled {
            Color::Reset
        } else {
            tint(self.accent)
        }
    }

    pub fn dimmed_accent(&self) -> Color {
        if self.colors_disabled {
            Color::Reset
        } else {
            dimmed(self.accent)
        }
    }

    /// Style of the selected row: a wash of the accent behind it, with
    /// the text keeping whatever colour it already had.
    pub fn selection(&self) -> Style {
        if self.high_contrast_selection {
            Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::new()
                .bg(self.selection_wash())
                .add_modifier(Modifier::BOLD)
        }
    }

    /// Style of the selected row when its panel does not have focus.
    /// Keeps the accent wash so the active category stays obvious while
    /// the Tasks panel has keyboard focus; bold is reserved for focus.
    pub fn selection_unfocused(&self) -> Style {
        if self.high_contrast_selection {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(self.selection_wash())
        }
    }

    pub fn accent_text(&self) -> Style {
        Style::new().fg(self.accent)
    }

    pub fn plain(&self) -> Style {
        Style::new()
    }
}

fn terminal_background_is_light() -> bool {
    let Ok(value) = std::env::var("COLORFGBG") else {
        return false;
    };
    value
        .split([';', ':'])
        .next_back()
        .and_then(|value| value.parse::<u8>().ok())
        .is_some_and(|background| matches!(background, 7 | 10..=15))
}

pub fn reduced_motion() -> bool {
    std::env::var("MACH_REDUCED_MOTION").is_ok_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_color_and_light_terminals_use_contrast_instead_of_rgb_washes() {
        let no_color = Theme::with_environment("cyan", true, false);
        assert_eq!(no_color.accent, Color::Reset);
        assert!(
            no_color
                .selection()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert_eq!(no_color.muted_color(), Color::Reset);
        assert_eq!(no_color.error_color(), Color::Reset);
        assert_eq!(no_color.success_color(), Color::Reset);
        assert_eq!(no_color.selection_wash(), Color::Reset);
        assert_eq!(no_color.dimmed_accent(), Color::Reset);

        let light = Theme::with_environment("white", false, true);
        assert_eq!(light.accent, Color::Indexed(238));
        assert!(light.selection().add_modifier.contains(Modifier::REVERSED));
    }
}
