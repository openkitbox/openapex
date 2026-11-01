//! Syntax colouring for fenced code. Pure: `(language, code, palette)` in,
//! styled spans out.
//!
//! The lexing is syntect's (pure-Rust regex engine, bundled syntaxes only,
//! loaded on first use). The colours are not: every TextMate scope maps to the
//! upstream `syntax.*` token that opencode's `theme/src/tui/syntax.ts` gives
//! the equivalent tree-sitter capture, and the colour comes from the
//! [`Palette`], so the colourless decks degrade like the rest of the UI.
//!
//! Deviations from upstream (tree-sitter), all forced by TextMate scopes:
//! - `let`/`const` and primitive types such as `u32` share `storage.type`, so
//!   both take the keyword colour (upstream paints `u32` as `type.builtin`).
//! - There is no TypeScript syntax in syntect's bundle; `ts`/`tsx`/`jsx` use
//!   the JavaScript grammar. TOML has none and renders plain.

use std::sync::OnceLock;

use ratatui::style::{Modifier, Style};
use ratatui::text::Span;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};

use crate::theme::Palette;

/// Which palette colour a scope takes.
#[derive(Clone, Copy)]
enum Role {
    Comment,
    String,
    Number,
    Keyword,
    Function,
    Type,
    Operator,
    Variable,
    Punctuation,
    /// Upstream paints builtins and tags in the error colour.
    Warn,
    Link,
    /// Not a token of its own upstream (a quote or `$` inside its string or
    /// variable): look at the enclosing scope instead.
    Inherit,
}

const ITALIC: Modifier = Modifier::ITALIC;
const BOLD: Modifier = Modifier::BOLD;
const NONE: Modifier = Modifier::empty();

/// `scope prefix -> role`, most specific first; the first prefix match wins.
const RULES: &[(&str, Role, Modifier)] = &[
    ("comment", Role::Comment, ITALIC),
    ("punctuation.definition.comment", Role::Inherit, NONE),
    ("punctuation.definition.string", Role::Inherit, NONE),
    ("punctuation.definition.variable", Role::Inherit, NONE),
    ("punctuation.definition.parameter", Role::Inherit, NONE),
    ("punctuation.definition.heading", Role::Inherit, NONE),
    ("punctuation.definition.bold", Role::Inherit, NONE),
    ("punctuation.definition.italic", Role::Inherit, NONE),
    ("punctuation.definition.raw", Role::Inherit, NONE),
    ("punctuation.separator", Role::Operator, NONE),
    ("punctuation.terminator", Role::Operator, NONE),
    ("punctuation.accessor", Role::Operator, NONE),
    ("punctuation", Role::Punctuation, NONE),
    ("string.regexp", Role::Keyword, NONE),
    ("constant.character.escape", Role::Keyword, NONE),
    ("string", Role::String, NONE),
    ("constant", Role::Number, NONE),
    ("storage.type.function", Role::Function, NONE),
    ("storage", Role::Keyword, ITALIC),
    ("keyword.control.import", Role::Keyword, NONE),
    ("keyword.control", Role::Keyword, ITALIC),
    ("keyword.operator", Role::Operator, NONE),
    ("keyword", Role::Keyword, ITALIC),
    ("entity.name.function", Role::Function, NONE),
    ("entity.name.tag", Role::Warn, NONE),
    ("entity.other.attribute-name", Role::Keyword, NONE),
    ("entity.name", Role::Type, NONE),
    ("entity.other.inherited-class", Role::Type, NONE),
    ("support.function", Role::Function, NONE),
    ("support.macro", Role::Function, NONE),
    ("support.class", Role::Type, NONE),
    ("support.type", Role::Type, NONE),
    ("variable.function", Role::Function, NONE),
    ("variable.language", Role::Warn, NONE),
    ("variable", Role::Variable, NONE),
    ("markup.heading", Role::Keyword, BOLD),
    ("markup.bold", Role::Variable, BOLD),
    ("markup.italic", Role::Variable, ITALIC),
    ("markup.raw", Role::String, NONE),
    ("markup.quote", Role::Comment, ITALIC),
    ("markup.underline.link", Role::Link, Modifier::UNDERLINED),
];

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
static SCOPES: OnceLock<Vec<(Scope, Role, Modifier)>> = OnceLock::new();

fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(SyntaxSet::load_defaults_newlines)
}

fn rules() -> &'static [(Scope, Role, Modifier)] {
    SCOPES.get_or_init(|| {
        RULES
            .iter()
            .filter_map(|&(name, role, mods)| Some((Scope::new(name).ok()?, role, mods)))
            .collect()
    })
}

/// The innermost scope on `stack` that has a rule decides; none means plain.
fn style_of(stack: &ScopeStack, palette: &Palette) -> Style {
    for scope in stack.as_slice().iter().rev() {
        let Some(&(_, role, mods)) = rules().iter().find(|(rule, ..)| rule.is_prefix_of(*scope))
        else {
            continue;
        };
        let syntax = &palette.syntax;
        let token = match role {
            Role::Inherit => continue,
            Role::Comment => syntax.comment,
            Role::String => syntax.string,
            Role::Number => syntax.number,
            Role::Keyword => syntax.keyword,
            Role::Function => syntax.function,
            Role::Type => syntax.r#type,
            Role::Operator => syntax.operator,
            Role::Variable => syntax.variable,
            Role::Punctuation => syntax.punctuation,
            Role::Warn => palette.warn,
            Role::Link => palette.link,
        };
        return palette.text.patch(token).add_modifier(mods);
    }
    palette.text
}

/// The grammar for a fence info string (`rust`, `Rust,ignore`, `ts title="x"`).
fn syntax_for(info: &str) -> Option<&'static SyntaxReference> {
    let token = info
        .split(|c: char| c.is_whitespace() || matches!(c, ',' | '{' | ':'))
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let token = match token.as_str() {
        "" | "text" | "plain" | "plaintext" | "txt" => return None,
        "ts" | "tsx" | "typescript" | "jsx" | "javascript" | "mjs" | "cjs" => "js",
        "shell" | "zsh" | "console" => "sh",
        "rs" => "rust",
        other => other,
    };
    let set = syntaxes();
    set.find_syntax_by_token(token)
        .filter(|syntax| syntax.name != "Plain Text")
}

/// The fence token for a file path, from its extension (upstream
/// `util/filetype.ts` `filetype`): `ts`/`tsx`/`js`/`jsx` all read as
/// TypeScript there, which here is the JavaScript grammar. Extensions the
/// table does not name are passed through for the grammar lookup to try.
#[must_use]
pub fn lang_for_path(path: &str) -> Option<&str> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    if name.eq_ignore_ascii_case("makefile") {
        return Some("makefile");
    }
    // `path.extname`: a leading dot alone (`.bashrc`) is no extension.
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() {
        return None;
    }
    let lang = match ext.to_ascii_lowercase().as_str() {
        "ts" | "tsx" | "mts" | "cts" | "mtsx" | "ctsx" | "ets" | "js" | "jsx" | "mjs" | "cjs" => {
            "ts"
        }
        "rs" => "rust",
        "py" => "python",
        "sh" | "bash" | "zsh" | "ksh" => "bash",
        "md" | "markdown" => "markdown",
        "yml" | "yaml" => "yaml",
        "htm" | "html" => "html",
        "rb" | "rake" | "gemspec" | "ru" => "ruby",
        "patch" | "diff" => "diff",
        _ => return Some(ext),
    };
    Some(lang)
}

/// `code` as one span list per line. An unknown language, or a line the
/// engine cannot lex, comes back as plain text in the palette's text colour.
#[must_use]
pub fn highlight(lang: &str, code: &str, palette: &Palette) -> Vec<Vec<Span<'static>>> {
    let Some(syntax) = syntax_for(lang) else {
        return code.lines().map(|line| plain(line, palette)).collect();
    };
    let set = syntaxes();
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut failed = false;
    let mut buffer = String::new();
    let mut out = Vec::new();
    for line in code.lines() {
        if failed {
            out.push(plain(line, palette));
            continue;
        }
        // The bundled grammars are `newlines` grammars: every line ends in \n.
        buffer.clear();
        buffer.push_str(line);
        buffer.push('\n');
        match lex_line(&mut state, &mut stack, &buffer, set, palette) {
            Some(spans) => out.push(spans),
            None => {
                failed = true;
                out.push(plain(line, palette));
            }
        }
    }
    out
}

fn plain(line: &str, palette: &Palette) -> Vec<Span<'static>> {
    if line.is_empty() {
        Vec::new()
    } else {
        vec![Span::styled(line.to_owned(), palette.text)]
    }
}

/// Spans for one `\n`-terminated line; `None` when the regex engine gives up.
fn lex_line(
    state: &mut ParseState,
    stack: &mut ScopeStack,
    line: &str,
    set: &SyntaxSet,
    palette: &Palette,
) -> Option<Vec<Span<'static>>> {
    let ops = state.parse_line(line, set).ok()?;
    let text = line.strip_suffix('\n').unwrap_or(line);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut last = 0;
    for (at, op) in ops {
        let at = at.min(text.len());
        if at > last {
            push(&mut spans, &text[last..at], style_of(stack, palette));
            last = at;
        }
        stack.apply(&op).ok()?;
    }
    if last < text.len() {
        push(&mut spans, &text[last..], style_of(stack, palette));
    }
    Some(spans)
}

/// Append `text`, merging into the previous span when the style is the same.
fn push(spans: &mut Vec<Span<'static>>, text: &str, style: Style) {
    match spans.last_mut() {
        Some(prev) if prev.style == style => prev.content.to_mut().push_str(text),
        _ => spans.push(Span::styled(text.to_owned(), style)),
    }
}
