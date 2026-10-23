//! The one place colour is decided. fx-style: terminal-default text, dim chrome,
//! a single accent for state. `NO_COLOR` and 16-colour terminals degrade the
//! same semantics to modifiers, so colour is never the only information
//! carrier.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

pub mod system;
pub mod terminal;

use terminal::{Rgb, TerminalColors};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deck {
    Truecolor,
    Ansi16,
    NoColor,
}

impl Deck {
    /// How much colour the terminal can take. `NO_COLOR` wins over everything;
    /// truecolor is claimed only when `COLORTERM` says so (the conservative
    /// reading — a terminal that supports 256 colours but not RGB lands on
    /// `Ansi16`, which the system theme expresses with named hues only).
    pub fn from_env() -> Self {
        let no_color = std::env::var_os("NO_COLOR")
            .map(|value| !value.is_empty())
            .unwrap_or(false);
        let truecolor = matches!(
            std::env::var("COLORTERM").as_deref(),
            Ok("truecolor") | Ok("24bit")
        );
        if no_color {
            Self::NoColor
        } else if truecolor {
            Self::Truecolor
        } else {
            Self::Ansi16
        }
    }
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
    /// The slash-command menu surface (upstream `background.raised.high`).
    pub menu: Style,
    /// The menu's four-sided border (upstream `border.base`).
    pub menu_border: Style,
    /// The selected menu row: accent background, dark foreground.
    pub menu_selected: Style,
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
                menu: Style::default().bg(Color::Rgb(36, 50, 78)),
                menu_border: Style::default().fg(Color::Rgb(72, 72, 72)),
                menu_selected: Style::default()
                    .fg(Color::Rgb(18, 26, 40))
                    .bg(Color::Rgb(86, 182, 194)),
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
                menu: Style::default().bg(Color::DarkGray),
                menu_border: Style::default().fg(Color::Gray),
                menu_selected: Style::default().fg(Color::Black).bg(Color::Cyan),
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
                menu: Style::default(),
                menu_border: Style::default().add_modifier(Modifier::DIM),
                menu_selected: Style::default().add_modifier(Modifier::REVERSED),
            },
        }
    }

    pub fn from_env() -> Self {
        Self::for_deck(Deck::from_env())
    }

    /// opencode's `system` theme: the terminal's own palette, with the surfaces
    /// derived from its reported background. `None` when there is nothing to
    /// derive from (no probe, or `NO_COLOR` wins), so the caller keeps the
    /// static palette above.
    pub fn system(colors: &TerminalColors, deck: Deck) -> Option<Self> {
        if deck == Deck::NoColor {
            return None;
        }
        let system = system::System::derive(colors)?;
        let accent = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let warn = Style::default().fg(Color::Red);

        if deck != Deck::Truecolor {
            // The derived ramp cannot be said in 16 colours, so the panel and
            // menu stay transparent and the chrome falls back to the two grays
            // the ANSI palette actually has.
            return Some(Self {
                text: Style::default(),
                dim: Style::default().fg(Color::DarkGray),
                accent,
                warn,
                highlight: Style::default().fg(Color::Black).bg(Color::Cyan),
                panel: Style::default(),
                panel_border: Style::default().fg(Color::Gray),
                agent: Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
                menu: Style::default(),
                menu_border: Style::default().fg(Color::DarkGray),
                menu_selected: Style::default().fg(Color::Black).bg(Color::Cyan),
            });
        }

        let background = rgb(system.background);
        let panel = rgb(system.grays[2]);
        Some(Self {
            text: Style::default(),
            dim: Style::default().fg(rgb(system.muted)),
            accent,
            warn,
            highlight: Style::default().fg(background).bg(Color::Cyan),
            panel: Style::default().bg(panel),
            panel_border: Style::default().fg(rgb(system.grays[8])).bg(panel),
            agent: Style::default().fg(Color::Magenta).bg(panel),
            menu: Style::default().bg(rgb(system.grays[3])),
            menu_border: Style::default().fg(rgb(system.grays[7])),
            menu_selected: Style::default().fg(background).bg(Color::Cyan),
        })
    }

    /// Paint `style` over `base`: the palette decides colour, the caller decides
    /// weight (bold labels keep their bold in `NO_COLOR` mode).
    pub fn paint(&self, base: Style, role: Style) -> Style {
        base.patch(role)
    }
}

static PALETTE: OnceLock<Palette> = OnceLock::new();

/// Read once: the environment does not change under a running session and a
/// per-frame `env` lookup on every span would be waste.
pub fn palette() -> &'static Palette {
    PALETTE.get_or_init(Palette::from_env)
}

/// Install the palette once, at startup, when the terminal probe has run.
///
/// Without a probe result (piped output, a terminal that ignores OSC) this is
/// the same static palette `palette()` would have built lazily.
pub fn init(colors: Option<TerminalColors>) {
    let deck = Deck::from_env();
    let palette = colors
        .as_ref()
        .and_then(|colors| Palette::system(colors, deck))
        .unwrap_or_else(|| Palette::for_deck(deck));
    let _ = PALETTE.set(palette);
}

fn rgb(color: Rgb) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}
