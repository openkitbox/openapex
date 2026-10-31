//! Theme tokens: hue scale -> semantic tokens, resolved to plain RGB.
//!
//! Pure data and arithmetic, no I/O. Two sources produce the same [`Tokens`]:
//!
//! - [`Tokens::opencode`]: upstream's default `opencode` theme, v2 document
//!   `packages/tui/src/theme/assets/v2/opencode.json`, dark and light.
//! - [`Tokens::system`]: upstream's `system` theme (`generateSystem`,
//!   `packages/tui/src/theme/system.ts:11-99`), built from the terminal's own
//!   colours and the gray ramp in [`super::system`].
//!
//! Deliberate deviations from upstream:
//!
//! 1. Only the tokens ox renders are kept. Action/formfield states and
//!    scrollbar tokens are not resolved; the state-expansion rules in
//!    `packages/theme/src/tui/expand.ts` are therefore not ported.
//! 2. `opencode.json` has no `secondary` token. `secondary` is `blue.200`, the
//!    value v1's `darkSecondary` (`assets/opencode.json` defs) has in dark.
//! 3. The mode comes from background luma `> 127.5` on the 0-255 scale, like
//!    `system.rs`. Upstream `terminalMode` (`system.ts:6`) compares the 0-1
//!    scaled luma with `0.5` on a 0-255 value, so everything but pure black
//!    counts as light there; that is a unit bug, not a design.
//! 4. `agent` is `categorical[0].200` (`context/local.tsx:75-82`); ox has one
//!    agent colour, not a rotating list.

use super::system::System;
use super::terminal::{Rgb, TerminalColors};

/// One hue, steps 100..=900 (`schema.ts` `HueStep`). Index `i` is step `(i + 1) * 100`.
type Scale = [Rgb; 9];

const fn h(value: u32) -> Rgb {
    Rgb::new((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

// Indices of the steps the tokens reference.
const S200: usize = 1;
const S400: usize = 3;
const S600: usize = 5;
const S700: usize = 6;
const S800: usize = 7;
const S900: usize = 8;

struct Hues {
    gray: Scale,
    red: Scale,
    orange: Scale,
    green: Scale,
    cyan: Scale,
    blue: Scale,
    purple: Scale,
}

/// `assets/v2/opencode.json:303-386`.
const DARK: Hues = Hues {
    gray: [
        h(0xffffff),
        h(0xeeeeee),
        h(0xb5b5b5),
        h(0x808080),
        h(0x4c4c4c),
        h(0x1e1e1e),
        h(0x141414),
        h(0x0a0a0a),
        h(0x030303),
    ],
    red: [
        h(0xfd7e87),
        h(0xe06c75),
        h(0xc35a63),
        h(0xa74952),
        h(0x8c3941),
        h(0x722931),
        h(0x591921),
        h(0x410a13),
        h(0x280207),
    ],
    orange: [
        h(0xfddac5),
        h(0xfab283),
        h(0xd8976c),
        h(0xb67c56),
        h(0x966341),
        h(0x774a2c),
        h(0x593319),
        h(0x3d1d06),
        h(0x1f0b01),
    ],
    green: [
        h(0x96f7a7),
        h(0x7fd88f),
        h(0x68b977),
        h(0x539c61),
        h(0x3d7f4b),
        h(0x296336),
        h(0x144922),
        h(0x00300f),
        h(0x011705),
    ],
    cyan: [
        h(0x68d0dd),
        h(0x56b6c2),
        h(0x449da7),
        h(0x33848e),
        h(0x226c75),
        h(0x0f555d),
        h(0x093e44),
        h(0x02292e),
        h(0x001518),
    ],
    blue: [
        h(0x82b4fb),
        h(0x5c9cf5),
        h(0x4c86d6),
        h(0x3c70b8),
        h(0x2c5b9b),
        h(0x1d477f),
        h(0x0f3364),
        h(0x02214a),
        h(0x01112b),
    ],
    purple: [
        h(0xb38ff4),
        h(0x9d7cd8),
        h(0x8869bd),
        h(0x7357a2),
        h(0x5f4688),
        h(0x4b356f),
        h(0x392557),
        h(0x271640),
        h(0x17072b),
    ],
};

/// `assets/v2/opencode.json:160-302`.
const LIGHT: Hues = Hues {
    gray: [
        h(0x000000),
        h(0x1a1a1a),
        h(0x4e4e4e),
        h(0x8a8a8a),
        h(0xbebebe),
        h(0xf5f5f5),
        h(0xfafafa),
        h(0xffffff),
        h(0xffffff),
    ],
    red: [
        h(0xc20f26),
        h(0xd1383d),
        h(0xe05352),
        h(0xee6b68),
        h(0xfc837d),
        h(0xfda19b),
        h(0xfcbeb9),
        h(0xfed8d5),
        h(0xfff2f0),
    ],
    orange: [
        h(0xc7811e),
        h(0xd68c27),
        h(0xdf9a44),
        h(0xe8a85c),
        h(0xf1b671),
        h(0xfac386),
        h(0xfcd4a8),
        h(0xfee3c7),
        h(0xfef3e7),
    ],
    green: [
        h(0x1f8c44),
        h(0x3d9a57),
        h(0x55a96a),
        h(0x6cb77c),
        h(0x81c68f),
        h(0x96d5a3),
        h(0xace4b6),
        h(0xc1f3ca),
        h(0xe1fee6),
    ],
    cyan: [
        h(0x077786),
        h(0x318795),
        h(0x4c97a4),
        h(0x64a8b4),
        h(0x7bb8c4),
        h(0x92c9d4),
        h(0xa8dbe4),
        h(0xbfecf4),
        h(0xe3fafe),
    ],
    blue: [
        h(0x226bcc),
        h(0x3b7dd8),
        h(0x518ee4),
        h(0x67a0f0),
        h(0x7db1fc),
        h(0x9cc3fa),
        h(0xb7d4fd),
        h(0xd4e5fd),
        h(0xf0f6fe),
    ],
    purple: [
        h(0x6b47a7),
        h(0x7b5bb6),
        h(0x8c6fc5),
        h(0x9d83d4),
        h(0xaf98e3),
        h(0xc1acf2),
        h(0xd3c3fc),
        h(0xe4dbfd),
        h(0xf6f3ff),
    ],
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}

impl Mode {
    pub fn of_background(background: Rgb) -> Self {
        if background.luminance() > 127.5 {
            Self::Light
        } else {
            Self::Dark
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Syntax {
    pub comment: Rgb,
    pub keyword: Rgb,
    pub function: Rgb,
    pub variable: Rgb,
    pub string: Rgb,
    pub number: Rgb,
    pub r#type: Rgb,
    pub operator: Rgb,
    pub punctuation: Rgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Markdown {
    pub text: Rgb,
    pub heading: Rgb,
    pub link: Rgb,
    pub link_text: Rgb,
    pub code: Rgb,
    pub block_quote: Rgb,
    pub emphasis: Rgb,
    pub strong: Rgb,
    pub horizontal_rule: Rgb,
    pub list_item: Rgb,
    pub list_enumeration: Rgb,
    pub image: Rgb,
    pub image_text: Rgb,
    pub code_block: Rgb,
}

/// The `diff.*` group (`theme/src/tui/schema.ts` `DiffDefinition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Diff {
    /// `diff.text.added`.
    pub added: Rgb,
    /// `diff.text.removed`.
    pub removed: Rgb,
    /// `diff.text.context`.
    pub context: Rgb,
    /// `diff.text.hunkHeader`.
    pub hunk: Rgb,
    /// `diff.highlight.added`: the `+` sign (`addedSignColor`, `index.tsx:3475`).
    pub highlight_added: Rgb,
    /// `diff.highlight.removed`: the `-` sign.
    pub highlight_removed: Rgb,
    /// `diff.background.added`.
    pub added_bg: Rgb,
    /// `diff.background.removed`.
    pub removed_bg: Rgb,
    /// `diff.background.context`, also the context gutter.
    pub context_bg: Rgb,
    /// `diff.lineNumber.text`.
    pub line_number: Rgb,
    /// `diff.lineNumber.background.added`.
    pub line_number_added_bg: Rgb,
    /// `diff.lineNumber.background.removed`.
    pub line_number_removed_bg: Rgb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tokens {
    /// `text.base`.
    pub text: Rgb,
    /// `text.muted`.
    pub text_muted: Rgb,
    /// `background.base`; `None` is the terminal's own background (the system
    /// theme's transparent background).
    pub background: Option<Rgb>,
    /// `background.raised.base`.
    pub raised_base: Rgb,
    /// `background.raised.high`; also `decrease(raised.base)`, the prompt
    /// panel (`component/prompt/index.tsx:1678`).
    pub raised_high: Rgb,
    /// `border.base`.
    pub border: Rgb,
    /// The interactive colour: selected rows (`text.action.primary.$selected`).
    pub primary: Rgb,
    pub secondary: Rgb,
    pub accent: Rgb,
    pub error: Rgb,
    pub warning: Rgb,
    pub success: Rgb,
    pub info: Rgb,
    /// Foreground drawn on a `primary` fill (`text.action.primary.$focused`).
    pub on_primary: Rgb,
    /// The agent chip and the prompt's left rule.
    pub agent: Rgb,
    pub syntax: Syntax,
    pub markdown: Markdown,
    pub diff: Diff,
}

impl Tokens {
    /// Upstream's default theme in `mode`, resolved from the v2 document.
    pub fn opencode(mode: Mode) -> Self {
        let dark = mode == Mode::Dark;
        let hues = if dark { &DARK } else { &LIGHT };
        // `hue.accent` / `hue.interactive` aliases (:301-302 dark, :228+ light).
        let (accent, interactive) = if dark {
            (hues.purple, hues.orange)
        } else {
            (hues.orange, hues.blue)
        };
        let gray = &hues.gray;
        // Mode overrides: warning, syntax.number/type and markdown
        // blockQuote/emphasis/strong differ between the two modes.
        let warning = if dark { h(0xf5a742) } else { accent[S200] };
        let gold = if dark { h(0xe5c07b) } else { h(0xb0851f) };
        let number = warning;
        let text = gray[S200];
        let muted = gray[S400];
        let blue = hues.blue[S200];
        let cyan = hues.cyan[S200];
        // `base.categorical` is blue-first in dark and purple-first in light.
        let agent = if dark { blue } else { hues.purple[S200] };
        Self {
            text,
            text_muted: muted,
            background: Some(gray[S800]),
            raised_base: gray[S700],
            raised_high: gray[S600],
            border: if dark { h(0x484848) } else { h(0xb8b8b8) },
            primary: interactive[S200],
            secondary: blue,
            accent: accent[S200],
            error: hues.red[S200],
            warning,
            success: hues.green[S200],
            info: cyan,
            on_primary: if dark { gray[S800] } else { gray[S900] },
            agent,
            syntax: Syntax {
                comment: muted,
                keyword: accent[S200],
                function: interactive[S200],
                variable: hues.red[S200],
                string: hues.green[S200],
                number,
                r#type: gold,
                operator: cyan,
                punctuation: text,
            },
            markdown: Markdown {
                text,
                heading: accent[S200],
                link: interactive[S200],
                link_text: cyan,
                code: hues.green[S200],
                block_quote: gold,
                emphasis: gold,
                strong: number,
                horizontal_rule: muted,
                list_item: interactive[S200],
                list_enumeration: cyan,
                image: interactive[S200],
                image_text: cyan,
                code_block: text,
            },
            // `assets/v2/opencode.json` diff blocks: dark :98-120, light
            // :271-292. Light omits `text.removed` and `background.context`;
            // v1 (`assets/opencode.json`) gives `#c53b53`, and the context
            // surface is `neutral.700` in both modes.
            diff: if dark {
                Diff {
                    added: h(0x4fd6be),
                    removed: h(0xc53b53),
                    context: h(0x828bb8),
                    hunk: h(0x828bb8),
                    highlight_added: h(0xb8db87),
                    highlight_removed: h(0xe26a75),
                    added_bg: h(0x20303b),
                    removed_bg: h(0x37222c),
                    context_bg: gray[S700],
                    line_number: h(0x8f8f8f),
                    line_number_added_bg: h(0x1b2b34),
                    line_number_removed_bg: h(0x2d1f26),
                }
            } else {
                Diff {
                    added: h(0x1e725c),
                    removed: h(0xc53b53),
                    context: h(0x7086b5),
                    hunk: h(0x7086b5),
                    highlight_added: h(0x4db380),
                    highlight_removed: h(0xf52a65),
                    added_bg: h(0xd5e5d5),
                    removed_bg: h(0xf7d8db),
                    context_bg: gray[S700],
                    line_number: h(0x595959),
                    line_number_added_bg: h(0xc5d5c5),
                    line_number_removed_bg: h(0xe7c8cb),
                }
            },
        }
    }

    /// Upstream's `system` theme: the terminal's own hues by ANSI slot, the
    /// body text as the terminal foreground, surfaces from the derived gray
    /// ramp (`system.ts:46-99`, then v1 migration puts `grays[2]`/`grays[3]`
    /// at `raised.base`/`raised.high`, `v1-migrate.ts:73-76`).
    pub fn system(system: &System, colors: &TerminalColors) -> Self {
        let fg = colors.foreground.unwrap_or_else(|| colors.slot(7));
        let (red, green, yellow, blue, magenta, cyan) = (
            colors.slot(1),
            colors.slot(2),
            colors.slot(3),
            colors.slot(4),
            colors.slot(5),
            colors.slot(6),
        );
        let rule = system.grays[7];
        // system.ts:36-41: diff surfaces tinted from the background.
        let alpha = if system.is_light { 0.14 } else { 0.22 };
        let context_bg = system.grays[2];
        Self {
            text: fg,
            text_muted: system.muted,
            background: None,
            raised_base: system.grays[2],
            raised_high: system.grays[3],
            border: rule,
            primary: cyan,
            secondary: magenta,
            accent: cyan,
            error: red,
            warning: yellow,
            success: green,
            info: cyan,
            on_primary: system.background,
            agent: magenta,
            syntax: Syntax {
                comment: system.muted,
                keyword: magenta,
                function: blue,
                variable: fg,
                string: green,
                number: yellow,
                r#type: cyan,
                operator: cyan,
                punctuation: fg,
            },
            markdown: Markdown {
                text: fg,
                heading: fg,
                link: blue,
                link_text: cyan,
                code: green,
                block_quote: yellow,
                emphasis: yellow,
                strong: fg,
                horizontal_rule: rule,
                list_item: blue,
                list_enumeration: cyan,
                image: blue,
                image_text: cyan,
                code_block: fg,
            },
            // system.ts:62-73.
            diff: Diff {
                added: green,
                removed: red,
                context: rule,
                hunk: rule,
                highlight_added: colors.slot(10),
                highlight_removed: colors.slot(9),
                added_bg: Rgb::tint(system.background, green, alpha),
                removed_bg: Rgb::tint(system.background, red, alpha),
                context_bg,
                line_number: system.muted,
                line_number_added_bg: Rgb::tint(context_bg, green, alpha),
                line_number_removed_bg: Rgb::tint(context_bg, red, alpha),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Mode, Tokens};
    use crate::theme::system::System;
    use crate::theme::terminal::{Rgb, TerminalColors};

    fn hex(value: u32) -> Rgb {
        Rgb::new((value >> 16) as u8, (value >> 8) as u8, value as u8)
    }

    #[test]
    fn dark_opencode_surfaces_follow_the_neutral_scale() {
        let t = Tokens::opencode(Mode::Dark);
        // assets/v2/opencode.json:12-13 text.base = neutral.200, muted = 400;
        // :54-58 background.base = 800, raised base/high = 700/600;
        // dark gray scale at :304-314.
        assert_eq!(t.text, hex(0xeeeeee));
        assert_eq!(t.text_muted, hex(0x808080));
        assert_eq!(t.background, Some(hex(0x0a0a0a)));
        assert_eq!(t.raised_base, hex(0x141414));
        assert_eq!(t.raised_high, hex(0x1e1e1e));
        // :93 border.base.
        assert_eq!(t.border, hex(0x484848));
        // :26 / :16-17 on-primary is neutral.800, the page background.
        assert_eq!(t.on_primary, hex(0x0a0a0a));
    }

    #[test]
    fn dark_opencode_status_and_hue_aliases() {
        let t = Tokens::opencode(Mode::Dark);
        // :40,:43,:46,:49 feedback bases; red/green/cyan 200 at :316,:338,:349.
        assert_eq!(t.error, hex(0xe06c75));
        assert_eq!(t.warning, hex(0xf5a742));
        assert_eq!(t.success, hex(0x7fd88f));
        assert_eq!(t.info, hex(0x56b6c2));
        // dark: hue.accent = purple, hue.interactive = orange (end of dark hue).
        assert_eq!(t.accent, hex(0x9d7cd8));
        assert_eq!(t.primary, hex(0xfab283));
        // deviation 2: blue.200.
        assert_eq!(t.secondary, hex(0x5c9cf5));
        // base.categorical[0] = blue (:4-10); deviation 4.
        assert_eq!(t.agent, hex(0x5c9cf5));
    }

    #[test]
    fn dark_opencode_syntax_and_markdown_groups() {
        let t = Tokens::opencode(Mode::Dark);
        // assets/v2/opencode.json:122-147.
        assert_eq!(t.syntax.comment, hex(0x808080));
        assert_eq!(t.syntax.keyword, hex(0x9d7cd8));
        assert_eq!(t.syntax.function, hex(0xfab283));
        assert_eq!(t.syntax.variable, hex(0xe06c75));
        assert_eq!(t.syntax.string, hex(0x7fd88f));
        assert_eq!(t.syntax.number, hex(0xf5a742));
        assert_eq!(t.syntax.r#type, hex(0xe5c07b));
        assert_eq!(t.syntax.operator, hex(0x56b6c2));
        assert_eq!(t.syntax.punctuation, hex(0xeeeeee));
        assert_eq!(t.markdown.text, hex(0xeeeeee));
        assert_eq!(t.markdown.heading, hex(0x9d7cd8));
        assert_eq!(t.markdown.link, hex(0xfab283));
        assert_eq!(t.markdown.link_text, hex(0x56b6c2));
        assert_eq!(t.markdown.code, hex(0x7fd88f));
        assert_eq!(t.markdown.block_quote, hex(0xe5c07b));
        assert_eq!(t.markdown.emphasis, hex(0xe5c07b));
        assert_eq!(t.markdown.strong, hex(0xf5a742));
        assert_eq!(t.markdown.horizontal_rule, hex(0x808080));
        assert_eq!(t.markdown.list_item, hex(0xfab283));
        assert_eq!(t.markdown.list_enumeration, hex(0x56b6c2));
        assert_eq!(t.markdown.image, hex(0xfab283));
        assert_eq!(t.markdown.image_text, hex(0x56b6c2));
        assert_eq!(t.markdown.code_block, hex(0xeeeeee));
    }

    #[test]
    fn light_opencode_applies_the_mode_overrides() {
        let t = Tokens::opencode(Mode::Light);
        // light gray scale at :161-171; text.base 200, muted 400.
        assert_eq!(t.text, hex(0x1a1a1a));
        assert_eq!(t.text_muted, hex(0x8a8a8a));
        assert_eq!(t.background, Some(hex(0xffffff)));
        assert_eq!(t.raised_base, hex(0xfafafa));
        assert_eq!(t.raised_high, hex(0xf5f5f5));
        // light override: border.base.
        assert_eq!(t.border, hex(0xb8b8b8));
        // light: text.action.primary.$focused = neutral.900.
        assert_eq!(t.on_primary, hex(0xffffff));
        // light: hue.accent = orange, hue.interactive = blue.
        assert_eq!(t.accent, hex(0xd68c27));
        assert_eq!(t.primary, hex(0x3b7dd8));
        // light: warning = accent.200; syntax.number = accent.200; strong = accent.200.
        assert_eq!(t.warning, hex(0xd68c27));
        assert_eq!(t.syntax.number, hex(0xd68c27));
        assert_eq!(t.markdown.strong, hex(0xd68c27));
        // light: syntax.type, markdown blockQuote/emphasis.
        assert_eq!(t.syntax.r#type, hex(0xb0851f));
        assert_eq!(t.markdown.block_quote, hex(0xb0851f));
        assert_eq!(t.markdown.emphasis, hex(0xb0851f));
        // light: categorical[0] = purple.
        assert_eq!(t.agent, hex(0x7b5bb6));
    }

    #[test]
    fn mode_follows_background_luminance() {
        assert_eq!(Mode::of_background(Rgb::new(0, 0, 0)), Mode::Dark);
        assert_eq!(Mode::of_background(Rgb::new(0x1b, 0x27, 0x38)), Mode::Dark);
        assert_eq!(Mode::of_background(Rgb::new(0xfd, 0xf6, 0xe3)), Mode::Light);
        // luma 128: the first light value (`> 127.5`, same as system.rs).
        assert_eq!(Mode::of_background(Rgb::new(128, 128, 128)), Mode::Light);
        assert_eq!(Mode::of_background(Rgb::new(127, 127, 127)), Mode::Dark);
    }

    /// The terminal palette the parity gold grids were captured under
    /// (`~/.local/state/opencode/latest/tui/system-theme.json`).
    fn gold_terminal() -> TerminalColors {
        let ansi = [
            0x1d1f21, 0xcc6666, 0xb5bd68, 0xf0c674, 0x81a2be, 0xb294bb, 0x8abeb7, 0xc5c8c6,
            0x666666, 0xd54e53, 0xb9ca4a, 0xe7c547, 0x7aa6da, 0xc397d8, 0x70c0b1, 0xeaeaea,
        ];
        let mut palette = [None; 16];
        for (slot, value) in palette.iter_mut().zip(ansi) {
            *slot = Some(hex(value));
        }
        TerminalColors {
            foreground: Some(hex(0xa6b2c0)),
            background: Some(hex(0x1b2738)),
            palette,
        }
    }

    #[test]
    fn system_tokens_match_the_gold_capture() {
        let colors = gold_terminal();
        let system = System::derive(&colors).expect("background reported");
        let t = Tokens::system(&system, &colors);
        // system.ts:54-55 text = defaultForeground; :56 textMuted.
        assert_eq!(t.text, hex(0xa6b2c0));
        assert_eq!(t.text_muted, hex(0xababab));
        // :57 background is transparent -> the terminal's own.
        assert_eq!(t.background, None);
        // :58 backgroundPanel = grays[2] (user block); :59 backgroundElement /
        // :60 backgroundMenu = grays[3] (prompt panel); :62 border = grays[7].
        assert_eq!(t.raised_base, hex(0x25364d));
        assert_eq!(t.raised_high, hex(0x2a3d58));
        assert_eq!(t.border, hex(0x3f5c84));
        // :48-54 hues by ANSI slot: cyan=6, magenta=5, red=1, yellow=3, green=2.
        assert_eq!(t.primary, hex(0x8abeb7));
        assert_eq!(t.accent, hex(0x8abeb7));
        assert_eq!(t.secondary, hex(0xb294bb));
        assert_eq!(t.agent, hex(0xb294bb));
        assert_eq!(t.error, hex(0xcc6666));
        assert_eq!(t.warning, hex(0xf0c674));
        assert_eq!(t.success, hex(0xb5bd68));
        assert_eq!(t.info, hex(0x8abeb7));
        // :56 selectedListItemText = background.
        assert_eq!(t.on_primary, hex(0x1b2738));
        // :80-99 syntax and markdown.
        assert_eq!(t.syntax.keyword, hex(0xb294bb));
        assert_eq!(t.syntax.function, hex(0x81a2be));
        assert_eq!(t.syntax.comment, hex(0xababab));
        assert_eq!(t.markdown.code, hex(0xb5bd68));
        assert_eq!(t.markdown.horizontal_rule, hex(0x3f5c84));
        assert_eq!(t.markdown.text, hex(0xa6b2c0));
    }

    #[test]
    fn opencode_diff_tokens_match_the_v2_document() {
        let d = Tokens::opencode(Mode::Dark).diff;
        assert_eq!(d.added, hex(0x4fd6be));
        assert_eq!(d.removed, hex(0xc53b53));
        assert_eq!(d.context, hex(0x828bb8));
        assert_eq!(d.hunk, hex(0x828bb8));
        assert_eq!(d.highlight_added, hex(0xb8db87));
        assert_eq!(d.highlight_removed, hex(0xe26a75));
        assert_eq!(d.added_bg, hex(0x20303b));
        assert_eq!(d.removed_bg, hex(0x37222c));
        assert_eq!(d.context_bg, hex(0x141414));
        assert_eq!(d.line_number, hex(0x8f8f8f));
        assert_eq!(d.line_number_added_bg, hex(0x1b2b34));
        assert_eq!(d.line_number_removed_bg, hex(0x2d1f26));
        let l = Tokens::opencode(Mode::Light).diff;
        assert_eq!(l.added, hex(0x1e725c));
        assert_eq!(l.removed, hex(0xc53b53), "v1 fallback");
        assert_eq!(l.context, hex(0x7086b5));
        assert_eq!(l.highlight_added, hex(0x4db380));
        assert_eq!(l.highlight_removed, hex(0xf52a65));
        assert_eq!(l.added_bg, hex(0xd5e5d5));
        assert_eq!(l.removed_bg, hex(0xf7d8db));
        assert_eq!(l.context_bg, hex(0xfafafa));
        assert_eq!(l.line_number, hex(0x595959));
        assert_eq!(l.line_number_added_bg, hex(0xc5d5c5));
        assert_eq!(l.line_number_removed_bg, hex(0xe7c8cb));
    }

    /// R10: system.ts:36-41, alpha 0.22 dark / 0.14 light.
    #[test]
    fn system_diff_backgrounds_tint_the_background_by_mode() {
        let colors = gold_terminal();
        let system = System::derive(&colors).expect("background reported");
        let d = Tokens::system(&system, &colors).diff;
        let bg = hex(0x1b2738);
        assert_eq!(d.added_bg, Rgb::tint(bg, hex(0xb5bd68), 0.22));
        assert_eq!(d.removed_bg, Rgb::tint(bg, hex(0xcc6666), 0.22));
        assert_eq!(d.context_bg, system.grays[2]);
        assert_eq!(
            d.line_number_added_bg,
            Rgb::tint(system.grays[2], hex(0xb5bd68), 0.22)
        );
        assert_eq!(
            d.line_number_removed_bg,
            Rgb::tint(system.grays[2], hex(0xcc6666), 0.22)
        );
        assert_eq!(d.line_number, system.muted);
        assert_eq!(d.added, hex(0xb5bd68));
        assert_eq!(d.removed, hex(0xcc6666));
        assert_eq!(d.highlight_added, hex(0xb9ca4a));
        assert_eq!(d.highlight_removed, hex(0xd54e53));
        assert_eq!(d.hunk, system.grays[7]);

        let mut light = colors;
        light.background = Some(hex(0xfdf6e3));
        let system = System::derive(&light).expect("background reported");
        assert!(system.is_light, "a cream background is light");
        let d = Tokens::system(&system, &light).diff;
        assert_eq!(d.added_bg, Rgb::tint(hex(0xfdf6e3), hex(0xb5bd68), 0.14));
        assert_eq!(d.removed_bg, Rgb::tint(hex(0xfdf6e3), hex(0xcc6666), 0.14));
    }

    #[test]
    fn system_text_falls_back_to_ansi_slot_seven() {
        let colors = TerminalColors {
            background: Some(Rgb::new(0, 0, 0)),
            ..TerminalColors::default()
        };
        let system = System::derive(&colors).expect("background reported");
        let t = Tokens::system(&system, &colors);
        // system.ts:14 defaultForeground ?? palette[7]; xterm default slot 7.
        assert_eq!(t.text, Rgb::new(0xc0, 0xc0, 0xc0));
    }
}
