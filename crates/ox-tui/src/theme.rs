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
    #[must_use]
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

/// Diff colours, one per upstream `diff.*` token (`theme/src/tui/schema.ts`
/// `DiffDefinition`). `*_bg` styles carry a background only; the others a
/// foreground only, so a row composes them with `patch`.
#[derive(Debug, Clone, Copy)]
pub struct DiffStyles {
    /// `diff.text.added`.
    pub added: Style,
    /// `diff.text.removed`.
    pub removed: Style,
    /// `diff.text.context`.
    pub context: Style,
    /// `diff.text.hunkHeader`: the `@@` row between hunks.
    pub hunk: Style,
    /// `diff.highlight.added`: the `+` sign.
    pub sign_added: Style,
    /// `diff.highlight.removed`: the `-` sign.
    pub sign_removed: Style,
    /// `diff.background.added`.
    pub added_bg: Style,
    /// `diff.background.removed`.
    pub removed_bg: Style,
    /// `diff.background.context`: context rows and their gutter, pad rows.
    pub context_bg: Style,
    /// `diff.lineNumber.text`.
    pub line_number: Style,
    /// `diff.lineNumber.background.added`.
    pub line_number_added_bg: Style,
    /// `diff.lineNumber.background.removed`.
    pub line_number_removed_bg: Style,
}

impl DiffStyles {
    fn from_tokens(d: &tokens::Diff) -> Self {
        let fg = |color: Rgb| Style::default().fg(rgb(color));
        let bg = |color: Rgb| Style::default().bg(rgb(color));
        Self {
            added: fg(d.added),
            removed: fg(d.removed),
            context: fg(d.context),
            hunk: fg(d.hunk),
            sign_added: fg(d.highlight_added),
            sign_removed: fg(d.highlight_removed),
            added_bg: bg(d.added_bg),
            removed_bg: bg(d.removed_bg),
            context_bg: bg(d.context_bg),
            line_number: fg(d.line_number),
            line_number_added_bg: bg(d.line_number_added_bg),
            line_number_removed_bg: bg(d.line_number_removed_bg),
        }
    }

    /// Named hues on the text and signs; no backgrounds (the signs carry the
    /// meaning).
    fn ansi16() -> Self {
        let fg = |color: Color| Style::default().fg(color);
        let none = Style::default();
        Self {
            added: fg(Color::Green),
            removed: fg(Color::Red),
            context: fg(Color::DarkGray),
            hunk: fg(Color::DarkGray),
            sign_added: fg(Color::Green),
            sign_removed: fg(Color::Red),
            added_bg: none,
            removed_bg: none,
            context_bg: none,
            line_number: fg(Color::DarkGray),
            line_number_added_bg: none,
            line_number_removed_bg: none,
        }
    }

    /// `NO_COLOR`: no colour at all; the gutter and hunk rows recede.
    fn no_color() -> Self {
        let none = Style::default();
        let dim = none.add_modifier(Modifier::DIM);
        Self {
            added: none,
            removed: none,
            context: none,
            hunk: dim,
            sign_added: none,
            sign_removed: none,
            added_bg: none,
            removed_bg: none,
            context_bg: none,
            line_number: dim,
            line_number_added_bg: none,
            line_number_removed_bg: none,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    /// Body text (`text.base`; the terminal foreground in the system theme).
    pub text: Style,
    /// Labels, separators, meta lines (`text.muted`).
    pub dim: Style,
    /// Receded text such as cancelled todo items (upstream `faint`). The
    /// default theme defines no `faint`, so upstream falls back to
    /// `$text.muted` (`theme/src/tui/expand.ts:52`); the colour equals `dim`
    /// there but stays a separate role so themes that define it can differ.
    pub faint: Style,
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
    /// Upstream `border.base`: the menu's four-sided border, and the rail of a
    /// user prompt still pending delivery.
    pub menu_border: Style,
    /// The selected menu row: accent background, dark foreground.
    pub menu_selected: Style,
    /// The raised surface behind a user message (upstream `raised.base`).
    pub raised_base: Style,
    /// The fill of an attachment badge (`file` / `dir`) and of the `skill`
    /// badge: upstream `hue.accent` behind `background.raised.base` in bold
    /// (`routes/session/index.tsx:2447-2464`). The palette has no hue scale, so
    /// this is the accent the theme already gives images
    /// (`theme/tokens.rs` `markdown.image`).
    pub image: Style,
    /// An attachment's name on its chip: upstream `text.muted` on
    /// `decrease(raised.base)` (`theme/tokens.rs` `markdown.image_text`).
    pub image_text: Style,
    /// Markdown links and list markers (upstream `markdown.link` /
    /// `markdown.listItem`).
    pub link: Style,
    /// Fenced-code colours (upstream `syntax.*` tokens).
    pub syntax: SyntaxStyles,
    /// A finished reasoning header: `warning` tinted 60% over the background
    /// (upstream `message-parts.tsx` ReasoningHeader).
    pub thought: Style,
    /// Diff rows (upstream `diff.*` tokens).
    pub diff: DiffStyles,
}

impl Palette {
    /// Upstream's default `opencode` theme in `mode`. The mode only matters on
    /// a truecolor deck: the 16-colour and colourless decks name their colours
    /// by role, which reads the same on either polarity.
    #[must_use]
    pub fn opencode(deck: Deck, mode: Mode) -> Self {
        match deck {
            Deck::Truecolor => Self::from_tokens(&Tokens::opencode(mode)),
            Deck::Ansi16 => Self {
                text: Style::default(),
                dim: Style::default().fg(Color::DarkGray),
                faint: Style::default().fg(Color::DarkGray),
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
                image: Style::default()
                    .bg(Color::Cyan)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
                image_text: Style::default().fg(Color::Cyan).bg(Color::DarkGray),
                link: Style::default().fg(Color::Blue),
                syntax: SyntaxStyles::ansi16(),
                thought: Style::default().fg(Color::Yellow),
                diff: DiffStyles::ansi16(),
            },
            Deck::NoColor => Self {
                text: Style::default(),
                dim: Style::default().add_modifier(Modifier::DIM),
                faint: Style::default().add_modifier(Modifier::DIM),
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
                image: Style::default().add_modifier(Modifier::BOLD),
                image_text: Style::default().add_modifier(Modifier::DIM),
                link: Style::default(),
                syntax: SyntaxStyles::no_color(),
                thought: Style::default(),
                diff: DiffStyles::no_color(),
            },
        }
    }

    /// The static palette when nothing is known about the terminal: the
    /// default theme, dark.
    #[must_use]
    pub fn for_deck(deck: Deck) -> Self {
        Self::opencode(deck, Mode::Dark)
    }

    #[must_use]
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
            faint: Style::default().fg(rgb(t.text_muted)),
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
            image: Style::default()
                .bg(rgb(t.markdown.image))
                .fg(rgb(t.raised_base))
                .add_modifier(Modifier::BOLD),
            image_text: Style::default()
                .fg(rgb(t.markdown.image_text))
                .bg(rgb(t.raised_high)),
            link: Style::default().fg(rgb(t.markdown.link)),
            syntax: SyntaxStyles::from_tokens(&t.syntax),
            thought: thought(t.background, t.warning),
            diff: DiffStyles::from_tokens(&t.diff),
        }
    }

    /// opencode's `system` theme: the terminal's own palette, with the surfaces
    /// derived from its reported background. `None` when there is nothing to
    /// derive from (no probe, or `NO_COLOR` wins), so the caller keeps the
    /// static palette above.
    #[must_use]
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
            faint: Style::default().fg(Color::DarkGray),
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
            image: Style::default()
                .bg(Color::Cyan)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            image_text: Style::default().fg(Color::Cyan),
            link: Style::default().fg(Color::Blue),
            syntax: SyntaxStyles::ansi16(),
            thought: Style::default().fg(Color::Yellow),
            diff: DiffStyles::ansi16(),
        })
    }

    /// Paint `style` over `base`: the palette decides colour, the caller decides
    /// weight (bold labels keep their bold in `NO_COLOR` mode).
    #[must_use]
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
