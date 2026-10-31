//! The one place colour is decided. Truecolor palettes are resolved from the
//! token system in [`tokens`] (upstream opencode's hue scale -> semantic
//! tokens); `NO_COLOR` and 16-colour terminals degrade the same semantics to
//! named colours and modifiers, so colour is never the only information
//! carrier.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};

pub mod system;
pub mod terminal;
pub mod tokens;

use terminal::{Rgb, TerminalColors};
use tokens::{Mode, Tokens};

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

/// Colours for code tokens, one per upstream `syntax.*` token
/// (`theme/src/tui/syntax.ts`). Italic/bold weights are the highlighter's
/// call; a deck only decides the colour (or, without colour, a modifier).
#[derive(Debug, Clone, Copy)]
pub struct SyntaxStyles {
    pub comment: Style,
    pub string: Style,
    pub number: Style,
    pub keyword: Style,
    pub function: Style,
    pub r#type: Style,
    pub operator: Style,
    pub variable: Style,
    pub punctuation: Style,
}

impl SyntaxStyles {
    fn from_tokens(syntax: &tokens::Syntax) -> Self {
        let fg = |color: Rgb| Style::default().fg(rgb(color));
        Self {
            comment: fg(syntax.comment),
            string: fg(syntax.string),
            number: fg(syntax.number),
            keyword: fg(syntax.keyword),
            function: fg(syntax.function),
            r#type: fg(syntax.r#type),
            operator: fg(syntax.operator),
            variable: fg(syntax.variable),
            punctuation: fg(syntax.punctuation),
        }
    }

    /// Named colours; identifiers and punctuation keep the terminal foreground.
    fn ansi16() -> Self {
        let fg = |color: Color| Style::default().fg(color);
        Self {
            comment: fg(Color::DarkGray),
            string: fg(Color::Green),
            number: fg(Color::Yellow),
            keyword: fg(Color::Magenta),
            function: fg(Color::Blue),
            r#type: fg(Color::Cyan),
            operator: fg(Color::Cyan),
            variable: Style::default(),
            punctuation: Style::default(),
        }
    }

    /// `NO_COLOR`: comments dim, definitions bold, everything else plain.
    fn no_color() -> Self {
        let plain = Style::default();
        Self {
            comment: plain.add_modifier(Modifier::DIM),
            function: plain.add_modifier(Modifier::BOLD),
            string: plain,
            number: plain,
            keyword: plain,
            r#type: plain,
            operator: plain,
            variable: plain,
            punctuation: plain,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Body text (`text.base`; the terminal foreground in the system theme).
    pub text: Style,
    /// Labels, separators, meta lines (`text.muted`).
    pub dim: Style,
    /// The interactive accent: running dot, search hits (`primary`).
    pub accent: Style,
    /// Failures and interrupts (`error`).
    pub warn: Style,
    /// A healthy status dot (`text.feedback.success`).
    pub success: Style,
    /// `text.feedback.warning`: the warning toast rail (`ui/toast.tsx:54`).
    pub warning: Style,
    /// `text.feedback.info`: the info toast rail (`ui/toast.tsx:54`).
    pub info: Style,
    /// The colour the terminal background resolves to, when it is known as RGB.
    /// Upstream tints the logo shadow from `background.base`
    /// (`component/logo.tsx:11`); the system theme's background is transparent,
    /// so there it is the terminal's own reported colour.
    pub backdrop: Option<Rgb>,
    /// A search hit, painted over a whole line.
    pub highlight: Style,
    /// The composer panel: one step lighter than the terminal background
    /// (upstream `promptBg`). Empty style on colourless decks.
    pub panel: Style,
    /// The composer panel's left border line (upstream `borderHighlight`).
    ///
    /// No background: upstream draws the border box *outside* the panel
    /// surface, so the rule and its `╹` foot both sit on the terminal
    /// background instead of carrying the panel colour.
    pub panel_border: Style,
    /// The agent chip in the composer metadata line (upstream violet).
    pub agent: Style,
    /// The slash-command menu surface (upstream `background.raised.high`).
    pub menu: Style,
    /// The menu's four-sided border (upstream `border.base`).
    pub menu_border: Style,
    /// The selected menu row: accent background, dark foreground.
    pub menu_selected: Style,
    /// The raised surface behind a user message (upstream `raised.base`).
    pub raised_base: Style,
    /// Markdown links and list markers (upstream `markdown.link` /
    /// `markdown.listItem`).
    pub link: Style,
    /// Fenced-code colours (upstream `syntax.*` tokens).
    pub syntax: SyntaxStyles,
    /// A finished reasoning header: `warning` tinted 60% over the background
    /// (upstream `message-parts.tsx` ReasoningHeader).
    pub thought: Style,
}

impl Palette {
    /// Upstream's default `opencode` theme in `mode`. The mode only matters on
    /// a truecolor deck: the 16-colour and colourless decks name their colours
    /// by role, which reads the same on either polarity.
    pub fn opencode(deck: Deck, mode: Mode) -> Self {
        match deck {
            Deck::Truecolor => Self::from_tokens(&Tokens::opencode(mode)),
            Deck::Ansi16 => Self {
                text: Style::default(),
                dim: Style::default().fg(Color::DarkGray),
                accent: Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
                warn: Style::default().fg(Color::Red),
                success: Style::default().fg(Color::Green),
                warning: Style::default().fg(Color::Yellow),
                info: Style::default().fg(Color::Cyan),
                backdrop: None,
                highlight: Style::default().fg(Color::Black).bg(Color::Cyan),
                panel: Style::default().bg(Color::DarkGray),
                panel_border: Style::default().fg(Color::White),
                agent: Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
                menu: Style::default().bg(Color::DarkGray),
                menu_border: Style::default().fg(Color::Gray),
                menu_selected: Style::default().fg(Color::Black).bg(Color::Cyan),
                raised_base: Style::default().bg(Color::DarkGray),
                link: Style::default().fg(Color::Blue),
                syntax: SyntaxStyles::ansi16(),
                thought: Style::default().fg(Color::Yellow),
            },
            Deck::NoColor => Self {
                text: Style::default(),
                dim: Style::default().add_modifier(Modifier::DIM),
                accent: Style::default().add_modifier(Modifier::BOLD),
                warn: Style::default().add_modifier(Modifier::BOLD),
                success: Style::default(),
                warning: Style::default().add_modifier(Modifier::BOLD),
                info: Style::default(),
                backdrop: None,
                highlight: Style::default().add_modifier(Modifier::REVERSED),
                panel: Style::default(),
                panel_border: Style::default().add_modifier(Modifier::DIM),
                agent: Style::default().add_modifier(Modifier::BOLD),
                menu: Style::default(),
                menu_border: Style::default().add_modifier(Modifier::DIM),
                menu_selected: Style::default().add_modifier(Modifier::REVERSED),
                raised_base: Style::default(),
                link: Style::default(),
                syntax: SyntaxStyles::no_color(),
                thought: Style::default(),
            },
        }
    }

    /// The static palette when nothing is known about the terminal: the
    /// default theme, dark.
    pub fn for_deck(deck: Deck) -> Self {
        Self::opencode(deck, Mode::Dark)
    }

    pub fn from_env() -> Self {
        Self::for_deck(Deck::from_env())
    }

    /// Truecolor roles from resolved tokens. Surfaces: the prompt panel and
    /// slash menu are `raised.high` (upstream `decrease(raised.base)`,
    /// `component/prompt/index.tsx:1678`, and `autocomplete.tsx:897`); the
    /// rail and menu borders are `border.base`; selected rows fill with the
    /// interactive colour.
    fn from_tokens(t: &Tokens) -> Self {
        let selected = Style::default().fg(rgb(t.on_primary)).bg(rgb(t.primary));
        let panel = rgb(t.raised_high);
        Self {
            text: Style::default().fg(rgb(t.text)),
            dim: Style::default().fg(rgb(t.text_muted)),
            accent: Style::default()
                .fg(rgb(t.primary))
                .add_modifier(Modifier::BOLD),
            warn: Style::default().fg(rgb(t.error)),
            success: Style::default().fg(rgb(t.success)),
            warning: Style::default().fg(rgb(t.warning)),
            info: Style::default().fg(rgb(t.info)),
            backdrop: t.background,
            highlight: selected,
            panel: Style::default().bg(panel),
            panel_border: Style::default().fg(rgb(t.border)),
            agent: Style::default().fg(rgb(t.agent)).bg(panel),
            menu: Style::default().bg(panel),
            menu_border: Style::default().fg(rgb(t.border)),
            menu_selected: selected,
            raised_base: Style::default().bg(rgb(t.raised_base)),
            link: Style::default().fg(rgb(t.markdown.link)),
            syntax: SyntaxStyles::from_tokens(&t.syntax),
            thought: thought(t.background, t.warning),
        }
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

        if deck == Deck::Truecolor {
            // Upstream's system background is transparent: the terminal's own
            // colour shows through, so that is what the logo shadow tints from.
            let tokens = Tokens::system(&system, colors);
            return Some(Self {
                backdrop: Some(system.background),
                thought: thought(Some(system.background), tokens.warning),
                ..Self::from_tokens(&tokens)
            });
        }
        // The derived ramp cannot be said in 16 colours, so the panel and menu
        // stay transparent and the chrome falls back to the two grays the ANSI
        // palette actually has.
        Some(Self {
            text: Style::default(),
            dim: Style::default().fg(Color::DarkGray),
            accent: Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            warn: Style::default().fg(Color::Red),
            success: Style::default().fg(Color::Green),
            warning: Style::default().fg(Color::Yellow),
            info: Style::default().fg(Color::Cyan),
            backdrop: None,
            highlight: Style::default().fg(Color::Black).bg(Color::Cyan),
            panel: Style::default(),
            panel_border: Style::default().fg(Color::Gray),
            agent: Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
            menu: Style::default(),
            menu_border: Style::default().fg(Color::DarkGray),
            menu_selected: Style::default().fg(Color::Black).bg(Color::Cyan),
            raised_base: Style::default(),
            link: Style::default().fg(Color::Blue),
            syntax: SyntaxStyles::ansi16(),
            thought: Style::default().fg(Color::Yellow),
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

/// `warning` tinted 60% over `background` (upstream `ReasoningHeader`); a
/// theme with no known background keeps the plain warning colour.
fn thought(background: Option<Rgb>, warning: Rgb) -> Style {
    let color = background.map_or(warning, |base| Rgb::tint(base, warning, 0.6));
    Style::default().fg(rgb(color))
}

fn rgb(color: Rgb) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::terminal::Rgb;
    use super::tokens::Mode;
    use super::{Deck, Palette, terminal};

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
        assert!(matches!(p.text.fg, Some(Color::Rgb(..))));
        assert!(matches!(p.accent.fg, Some(Color::Rgb(..))));
        assert!(matches!(p.highlight.bg, Some(Color::Rgb(..))));
    }

    #[test]
    fn default_palette_is_the_dark_opencode_theme() {
        let p = Palette::for_deck(Deck::Truecolor);
        // tokens.rs pins these against assets/v2/opencode.json.
        assert_eq!(p.text.fg, Some(Color::Rgb(0xee, 0xee, 0xee)));
        assert_eq!(p.dim.fg, Some(Color::Rgb(0x80, 0x80, 0x80)));
        assert_eq!(p.panel.bg, Some(Color::Rgb(0x1e, 0x1e, 0x1e)));
        assert_eq!(p.menu.bg, p.panel.bg);
        assert_eq!(p.accent.fg, Some(Color::Rgb(0xfa, 0xb2, 0x83)));
        assert_eq!(p.warn.fg, Some(Color::Rgb(0xe0, 0x6c, 0x75)));
        assert_eq!(p.agent.fg, Some(Color::Rgb(0x5c, 0x9c, 0xf5)));
        assert_eq!(p.panel_border.fg, Some(Color::Rgb(0x48, 0x48, 0x48)));
        assert_eq!(p.menu_border.fg, p.panel_border.fg);
        // Selection: dark page colour on the interactive fill.
        assert_eq!(p.menu_selected.fg, Some(Color::Rgb(0x0a, 0x0a, 0x0a)));
        assert_eq!(p.menu_selected.bg, Some(Color::Rgb(0xfa, 0xb2, 0x83)));
        assert_eq!(p.highlight, p.menu_selected);
    }

    #[test]
    fn light_opencode_theme_is_selected_by_background_luminance() {
        assert_eq!(Mode::of_background(Rgb::new(0xfd, 0xf6, 0xe3)), Mode::Light);
        let p = Palette::opencode(Deck::Truecolor, Mode::Light);
        assert_eq!(p.text.fg, Some(Color::Rgb(0x1a, 0x1a, 0x1a)));
        assert_eq!(p.panel.bg, Some(Color::Rgb(0xf5, 0xf5, 0xf5)));
        // The 16-colour and colourless decks do not vary with polarity.
        for deck in [Deck::Ansi16, Deck::NoColor] {
            assert_eq!(
                format!("{:?}", Palette::opencode(deck, Mode::Light)),
                format!("{:?}", Palette::opencode(deck, Mode::Dark))
            );
        }
    }

    #[test]
    fn menu_tokens_exist_per_deck() {
        let p = Palette::for_deck(Deck::Truecolor);
        assert!(matches!(p.menu.bg, Some(Color::Rgb(..))));
        assert!(matches!(p.menu_border.fg, Some(Color::Rgb(..))));
        assert!(matches!(p.menu_selected.bg, Some(Color::Rgb(..))));
        let p = Palette::for_deck(Deck::NoColor);
        assert_eq!(p.menu.bg, None, "no colour may leak through NO_COLOR");
        assert!(p.menu_selected.add_modifier.contains(Modifier::REVERSED));
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

    fn probed(background: Rgb) -> terminal::TerminalColors {
        terminal::TerminalColors {
            background: Some(background),
            ..terminal::TerminalColors::default()
        }
    }

    #[test]
    fn system_theme_derives_the_surfaces_and_takes_hues_from_the_palette() {
        let mut colors = probed(Rgb::new(16, 16, 16));
        colors.foreground = Some(Rgb::new(0xd0, 0xd0, 0xd0));
        colors.palette[1] = Some(Rgb::new(0xcc, 0x66, 0x66));
        colors.palette[6] = Some(Rgb::new(0x8a, 0xbe, 0xb7));
        let p = Palette::system(&colors, Deck::Truecolor).expect("background was reported");
        // Body text is the terminal foreground, hues are its ANSI slots.
        assert_eq!(p.text.fg, Some(Color::Rgb(0xd0, 0xd0, 0xd0)));
        assert_eq!(p.warn.fg, Some(Color::Rgb(0xcc, 0x66, 0x66)));
        assert_eq!(p.accent.fg, Some(Color::Rgb(0x8a, 0xbe, 0xb7)));
        // Surfaces come from the derived ramp: panel and menu grays[3]
        // (`backgroundMenu`), border grays[7], muted text.
        assert_eq!(p.panel.bg, Some(Color::Rgb(39, 39, 39)));
        assert_eq!(p.menu.bg, Some(Color::Rgb(39, 39, 39)));
        assert_eq!(p.panel_border.fg, Some(Color::Rgb(71, 71, 71)));
        assert_eq!(p.menu_border.fg, Some(Color::Rgb(71, 71, 71)));
        assert_eq!(p.dim.fg, Some(Color::Rgb(164, 164, 164)));
        // Selection paints the terminal background over the accent.
        assert_eq!(p.menu_selected.bg, Some(Color::Rgb(0x8a, 0xbe, 0xb7)));
        assert_eq!(p.menu_selected.fg, Some(Color::Rgb(16, 16, 16)));
        assert_eq!(p.highlight.fg, Some(Color::Rgb(16, 16, 16)));
    }

    /// The terminal the parity gold grids were captured under
    /// (`~/.local/state/opencode/latest/tui/system-theme.json`).
    #[test]
    fn system_theme_reproduces_the_gold_text_dim_and_panel() {
        let ansi = [
            0x1d1f21u32,
            0xcc6666,
            0xb5bd68,
            0xf0c674,
            0x81a2be,
            0xb294bb,
            0x8abeb7,
            0xc5c8c6,
            0x666666,
            0xd54e53,
            0xb9ca4a,
            0xe7c547,
            0x7aa6da,
            0xc397d8,
            0x70c0b1,
            0xeaeaea,
        ];
        let rgb = |v: u32| Rgb::new((v >> 16) as u8, (v >> 8) as u8, v as u8);
        let mut colors = probed(rgb(0x1b2738));
        colors.foreground = Some(rgb(0xa6b2c0));
        for (slot, value) in colors.palette.iter_mut().zip(ansi) {
            *slot = Some(rgb(value));
        }
        let p = Palette::system(&colors, Deck::Truecolor).expect("background was reported");
        // Gold: body a6b2c0, muted ababab, prompt panel 2a3d58, agent b294bb.
        assert_eq!(p.text.fg, Some(Color::Rgb(0xa6, 0xb2, 0xc0)));
        assert_eq!(p.dim.fg, Some(Color::Rgb(0xab, 0xab, 0xab)));
        assert_eq!(p.panel.bg, Some(Color::Rgb(0x2a, 0x3d, 0x58)));
        assert_eq!(p.agent.fg, Some(Color::Rgb(0xb2, 0x94, 0xbb)));
    }

    #[test]
    fn system_theme_without_truecolor_drops_the_derived_surfaces() {
        let colors = probed(Rgb::new(16, 16, 16));
        let p = Palette::system(&colors, Deck::Ansi16).expect("background was reported");
        assert_eq!(p.accent.fg, Some(Color::Cyan));
        assert_eq!(p.panel.bg, None, "no RGB surface on a 16-colour deck");
        assert_eq!(p.menu.bg, None);
        assert_eq!(p.dim.fg, Some(Color::DarkGray));
    }

    #[test]
    fn system_theme_yields_to_no_color_and_to_a_failed_probe() {
        let colors = probed(Rgb::new(16, 16, 16));
        assert!(Palette::system(&colors, Deck::NoColor).is_none());
        assert!(Palette::system(&terminal::TerminalColors::default(), Deck::Truecolor).is_none());
    }
}
