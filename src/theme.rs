//! 256-colour palette and the styles the UI is built from.

use ratatui::style::{Color, Modifier, Style};

use crate::model::LabelColor;

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

/// A lower-emphasis accent surface for pointer hover. On dark terminals it
/// softens the selection wash toward a neutral dark surface; on light
/// terminals it produces a pale accent tint.
fn hover_tint(color: Color, light_background: bool) -> Color {
    let (r, g, b) = rgb_of(color);
    if light_background {
        let pale = |channel: u8| ((u16::from(channel) + 5 * 255) / 6) as u8;
        Color::Rgb(pale(r), pale(g), pale(b))
    } else {
        let (r, g, b) = rgb_of(tint(color));
        let soften = |channel: u8| ((u16::from(channel) + 32) / 2) as u8;
        Color::Rgb(soften(r), soften(g), soften(b))
    }
}

fn contrasting_text(background: Color) -> Color {
    let (r, g, b) = rgb_of(background);
    let luminance = (u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114) / 1_000;
    if luminance >= 128 {
        Color::Indexed(16)
    } else {
        Color::Indexed(231)
    }
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
    colors_disabled: bool,
    light_background: bool,
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
            colors_disabled,
            light_background,
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

    fn high_contrast_selection(&self) -> bool {
        self.colors_disabled || self.light_background
    }

    /// Style of the selected row: a wash of the accent behind it, with
    /// the text keeping whatever colour it already had.
    pub fn selection(&self) -> Style {
        if self.high_contrast_selection() {
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
        if self.high_contrast_selection() {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::new().bg(self.selection_wash())
        }
    }

    pub fn accent_text(&self) -> Style {
        Style::new().fg(self.accent)
    }

    fn label_color(&self, color: LabelColor) -> Color {
        Color::Indexed(if self.light_background {
            match color {
                LabelColor::Red => 124,
                LabelColor::Orange => 166,
                LabelColor::Yellow => 136,
                LabelColor::Lime => 64,
                LabelColor::Green => 28,
                LabelColor::Teal => 29,
                LabelColor::Cyan => 30,
                LabelColor::Blue => 25,
                LabelColor::Indigo => 55,
                LabelColor::Purple => 91,
                LabelColor::Pink => 125,
                LabelColor::Brown => 94,
            }
        } else {
            match color {
                LabelColor::Red => 196,
                LabelColor::Orange => 208,
                LabelColor::Yellow => 226,
                LabelColor::Lime => 118,
                LabelColor::Green => 41,
                LabelColor::Teal => 36,
                LabelColor::Cyan => 45,
                LabelColor::Blue => 39,
                LabelColor::Indigo => 63,
                LabelColor::Purple => 141,
                LabelColor::Pink => 205,
                LabelColor::Brown => 130,
            }
        })
    }

    fn completed_label_color(&self) -> Color {
        Color::Indexed(if self.light_background { 238 } else { 244 })
    }

    /// Active labels keep their identity color under row focus; labels on a
    /// completed task share a neutral gray completion style.
    pub fn label_badge(&self, color: LabelColor, done: bool) -> Style {
        if self.colors_disabled {
            return Style::new().add_modifier(Modifier::REVERSED);
        }
        let background = if done {
            self.completed_label_color()
        } else {
            self.label_color(color)
        };
        Style::new()
            .fg(contrasting_text(background))
            .bg(background)
            .remove_modifier(Modifier::REVERSED)
    }

    /// A picker swatch is an inset glyph rather than a cell background, so
    /// the surrounding selection brackets can visibly enclose it.
    pub fn label_swatch(&self, color: LabelColor) -> Style {
        if self.colors_disabled {
            Style::new()
        } else {
            Style::new()
                .fg(self.label_color(color))
                .remove_modifier(Modifier::REVERSED)
        }
    }

    /// Manager focus is neutral so it cannot be mistaken for label color.
    pub fn label_focus(&self) -> Style {
        if self.colors_disabled {
            Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)
        } else {
            Style::new()
                .bg(Color::Indexed(if self.light_background {
                    250
                } else {
                    240
                }))
                .add_modifier(Modifier::BOLD)
        }
    }

    /// Lower-emphasis background for an unselected row. NO_COLOR cannot emit
    /// RGB, so reverse video supplies the background and DIM keeps it below
    /// the selected/focused state.
    pub fn hover(&self) -> Style {
        if self.colors_disabled {
            Style::new().add_modifier(Modifier::DIM | Modifier::REVERSED)
        } else {
            Style::new().bg(hover_tint(self.accent, self.light_background))
        }
    }

    /// Label-manager badges already own their background, so their hover
    /// state also supplies a readable foreground.
    pub fn label_hover(&self) -> Style {
        if self.colors_disabled {
            self.hover()
        } else {
            let background = hover_tint(self.accent, self.light_background);
            Style::new()
                .fg(contrasting_text(background))
                .bg(background)
                .remove_modifier(Modifier::REVERSED)
        }
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
        assert!(
            no_color
                .label_badge(LabelColor::Blue, false)
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(
            no_color
                .hover()
                .add_modifier
                .contains(Modifier::DIM | Modifier::REVERSED),
            "NO_COLOR still needs a background-like hover state"
        );
        assert!(!no_color.hover().add_modifier.contains(Modifier::BOLD));
        assert!(!no_color.hover().add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(no_color.hover().bg, None);

        let light = Theme::with_environment("white", false, true);
        assert_eq!(light.accent, Color::Indexed(238));
        assert!(light.selection().add_modifier.contains(Modifier::REVERSED));
        assert_eq!(
            light.label_badge(LabelColor::Brown, false).bg,
            Some(Color::Indexed(94))
        );
        assert_eq!(
            light.label_badge(LabelColor::Brown, false).fg,
            Some(Color::Indexed(231))
        );
        assert_eq!(
            light.label_badge(LabelColor::Red, true).bg,
            Some(Color::Indexed(238))
        );
        assert!(light.hover().bg.is_some());
        assert!(!light.hover().add_modifier.contains(Modifier::UNDERLINED));
        let dark_yellow = Theme::with_environment("yellow", false, false);
        assert!(dark_yellow.hover().bg.is_some());
        assert!(
            !dark_yellow
                .hover()
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert_eq!(
            dark_yellow.label_badge(LabelColor::Yellow, false).bg,
            Some(Color::Indexed(226))
        );
        assert_eq!(
            dark_yellow.label_badge(LabelColor::Yellow, false).fg,
            Some(Color::Indexed(16))
        );
        assert_eq!(
            dark_yellow.label_badge(LabelColor::Red, true).bg,
            Some(Color::Indexed(244))
        );
        assert_eq!(
            dark_yellow.label_badge(LabelColor::Blue, true).bg,
            Some(Color::Indexed(244)),
            "completed task labels share one neutral background"
        );
        assert_eq!(dark_yellow.label_focus().bg, Some(Color::Indexed(240)));
    }
}
