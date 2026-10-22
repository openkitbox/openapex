//! The one place colour is decided. fx-style: terminal-default text, dim chrome,
//! a single accent for state. `NO_COLOR` and 16-colour terminals degrade the
//! same semantics to modifiers, so colour is never the only information
//! carrier.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deck {
    Truecolor,
    Ansi16,
    NoColor,
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Body text: whatever the terminal considers default.
    pub text: Style,
    /// Labels, separators, meta lines.
    pub dim: Style,
    /// The single accent: running dot, user marker, search hits.
    pub accent: Style,
    /// Failures and interrupts.
    pub warn: Style,
    /// A search hit, painted over a whole line.
    pub highlight: Style,
    /// The composer panel: one step lighter than the terminal background
    /// (upstream `promptBg`). Empty style on colourless decks.
    pub panel: Style,
    /// The composer panel's left border line (upstream `borderHighlight`).
    pub panel_border: Style,
    /// The agent chip in the composer metadata line (upstream violet).
    pub agent: Style,
}

impl Palette {
    pub fn for_deck(deck: Deck) -> Self {
        match deck {
            Deck::Truecolor => Self {
                text: Style::default(),
                dim: Style::default().fg(Color::Rgb(128, 128, 128)),
                accent: Style::default()
                    .fg(Color::Rgb(86, 182, 194))
                    .add_modifier(Modifier::BOLD),
                warn: Style::default().fg(Color::Rgb(224, 108, 117)),
                highlight: Style::default()
                    .fg(Color::Rgb(0, 0, 0))
                    .bg(Color::Rgb(86, 182, 194)),
                panel: Style::default().bg(Color::Rgb(28, 39, 63)),
                panel_border: Style::default()
                    .fg(Color::Rgb(122, 139, 200))
                    .bg(Color::Rgb(28, 39, 63)),
                agent: Style::default()
                    .fg(Color::Rgb(167, 139, 250))
                    .bg(Color::Rgb(28, 39, 63)),
            },
            Deck::Ansi16 => Self {
                text: Style::default(),
                dim: Style::default().fg(Color::DarkGray),
                accent: Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                warn: Style::default().fg(Color::Red),
                highlight: Style::default().fg(Color::Black).bg(Color::Cyan),
                panel: Style::default().bg(Color::DarkGray),
                panel_border: Style::default().fg(Color::White).bg(Color::DarkGray),
                agent: Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            },
            Deck::NoColor => Self {
                text: Style::default(),
                dim: Style::default().add_modifier(Modifier::DIM),
                accent: Style::default().add_modifier(Modifier::BOLD),
                warn: Style::default().add_modifier(Modifier::BOLD),
                highlight: Style::default().add_modifier(Modifier::REVERSED),
                panel: Style::default(),
                panel_border: Style::default().add_modifier(Modifier::DIM),
                agent: Style::default().add_modifier(Modifier::BOLD),
            },
        }
    }

    pub fn from_env() -> Self {
        let no_color = std::env::var_os("NO_COLOR")
            .map(|value| !value.is_empty())
            .unwrap_or(false);
        let truecolor = matches!(
            std::env::var("COLORTERM").as_deref(),
            Ok("truecolor") | Ok("24bit")
        );
        Self::for_deck(if no_color {
            Deck::NoColor
        } else if truecolor {
            Deck::Truecolor
        } else {
            Deck::Ansi16
        })
    }

    /// Paint `style` over `base`: the palette decides colour, the caller decides
    /// weight (bold labels keep their bold in `NO_COLOR` mode).
    pub fn paint(&self, base: Style, role: Style) -> Style {
        base.patch(role)
    }
}

/// Read once: the environment does not change under a running session and a
/// per-frame `env` lookup on every span would be waste.
pub fn palette() -> &'static Palette {
    static PALETTE: OnceLock<Palette> = OnceLock::new();
    PALETTE.get_or_init(Palette::from_env)
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::{Deck, Palette};

    #[test]
    fn no_color_degrades_to_modifiers() {
        let p = Palette::for_deck(Deck::NoColor);
        assert_eq!(p.text, ratatui::style::Style::default());
        assert!(p.accent.add_modifier.contains(Modifier::BOLD));
        assert_eq!(p.dim.fg, None, "no colour may leak through NO_COLOR");
        assert!(p.highlight.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn ansi16_uses_named_colours() {
        let p = Palette::for_deck(Deck::Ansi16);
        assert_eq!(p.accent.fg, Some(Color::Cyan));
        assert_eq!(p.dim.fg, Some(Color::DarkGray));
    }

    #[test]
    fn truecolor_uses_rgb() {
        let p = Palette::for_deck(Deck::Truecolor);
        assert!(matches!(p.accent.fg, Some(Color::Rgb(..))));
        assert!(matches!(p.highlight.bg, Some(Color::Rgb(..))));
    }

    #[test]
    fn panel_tokens_exist_per_deck() {
        let p = Palette::for_deck(Deck::Truecolor);
        assert!(matches!(p.panel.bg, Some(Color::Rgb(..))));
        assert!(matches!(p.panel_border.fg, Some(Color::Rgb(..))));
        assert!(matches!(p.agent.fg, Some(Color::Rgb(..))));
        let p = Palette::for_deck(Deck::NoColor);
        assert_eq!(p.panel.bg, None, "no colour may leak through NO_COLOR");
        assert!(p.agent.add_modifier.contains(Modifier::BOLD));
    }
}
