use std::borrow::Cow;

use ratatui::text::{Line, Span};

use ox_client::store::CompletionSource;
use ox_client::store::MediaMeta;
use ox_client::store::ModelChoice;
use ox_client::store::Notice;
use ox_client::store::Switch;
use ox_client::store::ToolState;

use crate::theme;

use super::block::{Block, TurnFooter};
use super::text::{format_duration, indent_wrapped, titlecase};
use super::tool::{explored_block, text_tool_block, todo_block, tool_block};
use super::view::{SPINNER_FRAMES, ViewState, draws_block, tool_expandable};
use crate::ui::block_tool;
use crate::ui::media;
use crate::ui::patch_block::PatchCall;
use crate::ui::tool_row::{self, InlineSpec, RowView};
use crate::ui::tool_text;

/// The frame a block is laid out for: the columns it may use, and the rows one
/// attachment thumbnail is tall (which follows the terminal, not the block).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Metrics {
    pub(super) width: u16,
    pub(super) thumb_height: u16,
}

/// What one block draws: its rows, whether a click or `o` toggles it (a shell
/// block learns that from the same collapse its rows came from), the row of a
/// shell block's spinner lead, and the row band it reserves for attachment
/// thumbnails.
pub(super) struct Rendered {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) expandable: bool,
    pub(super) spinner_row: Option<usize>,
    /// Where the thumbnail strip's rows are, when the block has images and this
    /// terminal can draw them: the rows themselves are blank, since an image is
    /// pixels the graphics layer paints into the cells they leave.
    pub(super) thumbs: Option<media::Thumbs>,
}

/// Every block is one upstream row: `marginTop={1}` puts a blank line above it
/// (`routes/session/index.tsx:1670`). A block with nothing to show draws
/// nothing, not a stray blank line.
/// A block's rows, and whether a click or `o` toggles it: a shell block
/// learns that from the same collapse its rows came from.
/// A shell block also reports the row of its spinner lead.
pub(super) fn render_entry(block: &Block, metrics: Metrics, view: ViewState) -> Rendered {
    let width = metrics.width;
    let palette = theme::palette();
    let mut expandable = tool_expandable(block);
    let mut spinner_row = None;
    let mut thumbs = None;
    let body = match block {
        Block::Shell {
            state,
            input,
            output,
            omitted,
            shell,
            background,
            shell_running,
            ..
        } => {
            let workdir = input
                .get("workdir")
                .and_then(serde_json::Value::as_str)
                .map(|dir| tool_text::format_path(dir, tool_text::paths()))
                .unwrap_or_default();
            let spec = crate::ui::shell_block::ShellSpec {
                command: input
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default(),
                workdir: &workdir,
                state: *state,
                output,
                omitted: *omitted,
                has_shell: *shell,
                background: *background,
                shell_running: *shell_running,
                expanded: view.tool_expanded,
                hovered: view.hovered || view.tool_focused,
                spinner: SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
            };
            let rendered = crate::ui::shell_block::render(&spec, width);
            expandable = rendered.expandable;
            spinner_row = rendered.spinner_row;
            rendered.lines
        }
        Block::User { text, files } => {
            let (lines, strip) = user_block(text, files, view, metrics);
            thumbs = strip;
            lines
        }
        Block::Reasoning { text, done, ms, .. } => reasoning_block(text, *done, *ms, view, width),
        Block::Assistant { text, footer, .. } => {
            assistant_block(text, footer.as_ref(), view.retry_secs, width)
        }
        Block::Switch(switch) => vec![switch_line(switch, width)],
        Block::Notice(notice) => notice_lines(notice, width),
        Block::Error(error) => error_lines(&error.message, width),
        Block::CompactionQueued => vec![crate::ui::compaction::queued_line(width)],
        Block::Explored(explored) => explored_block(explored, view, width),
        Block::Tool {
            tool,
            state,
            summary,
            output,
            input,
            metadata,
            media: files,
            ..
        } => {
            let mut body = match own_block(
                tool,
                &PatchCall {
                    state: *state,
                    input,
                    metadata: metadata.as_deref(),
                    output,
                },
                width,
                palette,
            ) {
                Some(lines) => lines,
                None => {
                    match tool_text::text(
                        tool,
                        input,
                        metadata.as_deref(),
                        *state,
                        tool_text::paths(),
                    ) {
                        Some(text) => text_tool_block(block, &text, input, output, view, width),
                        None => tool_block(block, tool, *state, summary, output, view, width),
                    }
                }
            };
            // A result that carries images draws them under the block, as
            // upstream's `ToolImages` does (`routes/session/index.tsx:2645`):
            // one padding row, the image rows, one padding row.
            if !body.is_empty()
                && let Some(drawn) = thumbs_of(files, metrics, body.len() + 1)
            {
                body.push(Line::default());
                body.extend(
                    std::iter::repeat_with(Line::default).take(usize::from(drawn.strip.height)),
                );
                body.push(Line::default());
                thumbs = Some(drawn);
            }
            body
        }
        Block::Todo {
            state,
            todos,
            output,
            ..
        } => todo_block(*state, todos, output, view, width),
        Block::Compaction {
            status,
            text,
            cancelled,
            tokens,
            ..
        } => crate::ui::compaction::lines(
            &crate::ui::compaction::View {
                status: *status,
                text,
                cancelled: *cancelled,
                tokens: tokens.as_ref(),
                spinner: SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()],
            },
            width,
        ),
        Block::Completion {
            source,
            state,
            description,
            ..
        } => vec![completion_line(
            *source,
            state.as_deref(),
            description,
            usize::from(width),
            view.hovered,
            palette,
        )],
    };
    if body.is_empty() {
        return Rendered {
            lines: body,
            expandable,
            spinner_row: None,
            thumbs: None,
        };
    }
    let mut lines = Vec::with_capacity(body.len() + 1);
    lines.push(Line::default());
    lines.extend(body);
    // The margin row above shifts the block's own rows down by one.
    Rendered {
        lines,
        expandable,
        spinner_row: spinner_row.map(|row| row + 1),
        thumbs: thumbs.map(|thumbs| media::Thumbs {
            strip: media::Strip {
                at: thumbs.strip.at + 1,
                ..thumbs.strip
            },
            ..thumbs
        }),
    }
}

/// Columns every session-level row is indented (`paddingLeft={3}`).
const COMPLETION_INDENT: usize = 3;

/// A call drawn as its tool's own block (upstream `Edit`, `ApplyPatch`,
/// `Write`), or
/// `None` to keep the inline row. Such blocks are not expandable
/// ([`draws_block`] keeps `tool_expandable` and `clickable` false).
fn own_block(
    tool: &str,
    call: &PatchCall<'_>,
    width: u16,
    palette: &theme::Palette,
) -> Option<Vec<Line<'static>>> {
    if !draws_block(tool, call.state) {
        return None;
    }
    match tool {
        "edit" => crate::ui::edit_block::render(call.metadata, width, palette),
        "patch" | "apply_patch" => crate::ui::patch_block::render(call, width, palette),
        "write" => Some(crate::ui::write_block::render(call.input, width, palette)),
        "question" => crate::ui::question_block::render(call.input, call.metadata, width, palette),
        _ => None,
    }
}

/// One completion notice row: `   ↳ Shell finished · description`.
///
/// The icon is `↳` for `completed` and `!` for every other state. Only the
/// title carries a status colour (`error` → error, `cancelled` → warning,
/// anything else → info, or the base text colour while `hovered`; error and
/// cancelled win over the hover, as upstream); the ` · description` suffix is always muted. The
/// suffix is cut by display width to fit `width` and ends with `…`; when even
/// ` · …` does not fit it is dropped, so the row never exceeds `width` unless
/// the title alone does.
fn completion_line(
    source: CompletionSource,
    state: Option<&str>,
    description: &str,
    width: usize,
    hovered: bool,
    palette: &theme::Palette,
) -> Line<'static> {
    use std::borrow::Cow;

    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

    let icon = if state == Some("completed") {
        '↳'
    } else {
        '!'
    };
    let actor = match source {
        CompletionSource::Shell => "Shell",
        CompletionSource::Subagent => "Subagent",
    };
    let word = match state {
        Some("completed") | None => "finished",
        Some("error") => "failed",
        Some(other) => other,
    };
    let title_style = match state {
        Some("error") => palette.warn,
        Some("cancelled") => palette.warning,
        _ if hovered => palette.text,
        _ => palette.info,
    };
    let title = format!("{:COMPLETION_INDENT$}{icon} {actor} {word}", "");
    let budget = width.saturating_sub(title.width());

    const SEPARATOR: &str = " · ";
    // Control characters (`\n`, `\t`, …) are dropped by the terminal draw and
    // would corrupt copied text, so each one renders as a single space.
    let description: Cow<'_, str> = if description.chars().any(char::is_control) {
        Cow::Owned(
            description
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect(),
        )
    } else {
        Cow::Borrowed(description)
    };
    let cell_width = |ch: char| ch.width().unwrap_or(0);
    let description_cells: usize = description.chars().map(cell_width).sum();
    let separator_cells = SEPARATOR.chars().map(cell_width).sum::<usize>();
    let suffix = if separator_cells + description_cells <= budget {
        Some(format!("{SEPARATOR}{description}"))
    } else if budget > separator_cells {
        // Leave one cell for the ellipsis.
        let mut room = budget - separator_cells - 1;
        let mut cut = String::from(SEPARATOR);
        for ch in description.chars() {
            let cells = cell_width(ch);
            if cells > room {
                break;
            }
            room -= cells;
            cut.push(ch);
        }
        cut.push('…');
        Some(cut)
    } else {
        None
    };

    let mut spans = vec![Span::styled(title, title_style)];
    spans.extend(suffix.map(|suffix| Span::styled(suffix, palette.dim)));
    Line::from(spans)
}

/// Upstream `reasoningSummary` (`context/thinking.ts`): a leading `**Title**`
/// block is the header's title, the rest is the body.
fn reasoning_summary(text: &str) -> (Option<&str>, &str) {
    let content = text.trim();
    let Some(rest) = content.strip_prefix("**") else {
        return (None, content);
    };
    let Some(end) = rest.find("**") else {
        return (None, content);
    };
    let title = &rest[..end];
    let after = &rest[end + 2..];
    let body = if after.is_empty() {
        Some("")
    } else {
        after
            .strip_prefix("\n\n")
            .or_else(|| after.strip_prefix("\r\n\r\n"))
    };
    match body {
        Some(body) if !title.is_empty() && !title.contains('\n') => {
            (Some(title.trim()), body.trim_end())
        }
        _ => (None, content),
    }
}

/// Header colour: full `warning` when open, or when a finished header is
/// hovered (upstream `group-view.tsx:166-177`); the 60% `thought` tint
/// otherwise.
fn thought_header_style(
    palette: &theme::Palette,
    expanded: bool,
    done: bool,
    hovered: bool,
) -> ratatui::style::Style {
    if expanded || (done && hovered) {
        palette.warning
    } else {
        palette.thought
    }
}

/// Upstream `ReasoningPart` / `ReasoningHeader` (`message-parts.tsx`). The
/// header is `+ Thought` collapsed, `- Thought` open, and a spinner with
/// `Thinking` while the reasoning streams. A finished header appends
/// ` · 2.5s` (gold: `+ Thought · 2.5s`); a summary title sits before it as
/// `: title`. An open block shows its text on a `┃` rail one blank row below
/// the header. Click or `r` toggles it.
///
/// Observed in upstream 2.2.3 (`gold`): the open header is the plain warning
/// colour and the collapsed one is warning tinted 60% over the background.
/// The body is markdown in the muted tone (upstream `ReasoningPart` with
/// `generateThinkingSyntax`, `message-parts.tsx:43`).
fn reasoning_block(
    text: &str,
    done: bool,
    ms: Option<u64>,
    view: ViewState,
    width: u16,
) -> Vec<Line<'static>> {
    let palette = theme::palette();
    if text.trim().is_empty() {
        return Vec::new();
    }
    let expanded = view.reasoning_expanded;
    let (title, body) = reasoning_summary(text);
    let style = thought_header_style(palette, expanded, done, view.hovered);
    let header = if done {
        let mut header = format!("   {} Thought", if expanded { '-' } else { '+' });
        if let Some(title) = title {
            header.push_str(&format!(": {title}"));
        }
        if let Some(duration) = thought_duration(ms) {
            header.push_str(&format!(" \u{b7} {duration}"));
        }
        header
    } else {
        let glyph = SPINNER_FRAMES[view.spin % SPINNER_FRAMES.len()];
        match title {
            Some(title) => format!("   {glyph} Thinking: {title}"),
            None => format!("   {glyph} Thinking"),
        }
    };
    let mut lines = vec![Line::from(Span::styled(header, style))];
    if expanded && !body.is_empty() {
        let rail = palette
            .panel
            .bg
            .map_or(palette.dim, |bg| ratatui::style::Style::default().fg(bg));
        lines.push(Line::default());
        let rendered = crate::ui::markdown::render_toned(
            body,
            width.saturating_sub(5),
            0,
            crate::ui::markdown::Tone::Muted,
        );
        lines.extend(rendered.into_iter().map(|line| {
            let mut spans = Vec::with_capacity(line.spans.len() + 3);
            spans.push(Span::raw("   "));
            spans.push(Span::styled("\u{2503}", rail));
            spans.push(Span::raw(" "));
            spans.extend(line.spans);
            Line::from(spans)
        }));
    }
    lines
}

/// The ` · 2.5s` suffix of a finished Thought header: tenths of a second, and
/// no suffix until the reasoning reported a duration (`ms == 0` means none).
///
/// `format_duration` is upstream's `Locale.duration`, whose sub-second shape is
/// `400ms`; the header always shows seconds (`0.4s`), so this is the smallest
/// formatter that fits it instead of a mode inside the shared one.
fn thought_duration(ms: Option<u64>) -> Option<String> {
    let ms = ms.filter(|ms| *ms > 0)?;
    Some(format!("{:.1}s", ms as f64 / 1_000.0))
}

/// The answer text, then the turn footer (own blank row above it).
fn assistant_block(
    text: &str,
    footer: Option<&TurnFooter>,
    retry_secs: Option<i64>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines = crate::ui::markdown::render(text.trim(), width, 3);
    if let Some(footer) = footer {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(footer_lines(footer, retry_secs, width));
    }
    lines
}

/// The step error the engine records for a cancelled step (upstream
/// `AssistantFooter` `interrupted`).
const STEP_INTERRUPTED: &str = "Step interrupted";

/// Upstream `AssistantFooter` (`routes/session/index.tsx:2022-2058`): the
/// error (unless interrupted or retrying), the retry countdown
/// (`AssistantRetry`, :2528), a blank row when either showed, then the stats
/// line. `retry_secs` is the countdown `Transcript` derived from the wall clock.
fn footer_lines(footer: &TurnFooter, retry_secs: Option<i64>, width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let interrupted = footer
        .error
        .as_ref()
        .is_some_and(|error| &*error.message == STEP_INTERRUPTED);
    let mut lines = Vec::new();
    if let Some(error) = footer.error.as_ref()
        && !interrupted
        && footer.retry.is_none()
    {
        lines.extend(error_lines(&error.message, width));
    }
    if let Some(retry) = &footer.retry {
        let seconds = retry_secs.unwrap_or(0);
        let when = if seconds > 0 {
            format!("Retrying in {seconds}s")
        } else {
            "Retry due".to_owned()
        };
        let text = format!(
            "\u{26a0} {when} \u{b7} attempt {} \u{b7} {}",
            retry.attempt, retry.message
        );
        lines.extend(
            indent_wrapped(&text, width, COMPLETION_INDENT)
                .into_iter()
                .map(|line| line.style(palette.warning)),
        );
    }
    if footer.retry.is_some() || (footer.error.is_some() && !interrupted) {
        lines.push(Line::default());
    }
    let mut stats = footer_line(footer, width);
    if interrupted {
        stats
            .spans
            .push(Span::styled(" \u{b7} interrupted", palette.dim));
    }
    lines.push(stats);
    lines
}

/// `Error: message` in the error colour, indented three columns (upstream
/// `AssistantFooter` fallback and the session-level error row).
fn error_lines(message: &str, width: u16) -> Vec<Line<'static>> {
    let palette = theme::palette();
    indent_wrapped(&format!("Error: {message}"), width, COMPLETION_INDENT)
        .into_iter()
        .map(|line| line.style(palette.warn))
        .collect()
}

/// Upstream `AssistantRetry`: `max(0, ceil((at − now) / 1000))`.
pub(super) fn retry_secs(at: i64, now_ms: i64) -> i64 {
    let left = at.saturating_sub(now_ms);
    if left <= 0 {
        0
    } else {
        left.saturating_add(999) / 1000
    }
}

/// Milliseconds since the Unix epoch; 0 if the clock is before it.
pub(super) fn wall_ms_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// Upstream `SessionSwitchMessageV2` (`routes/session/index.tsx:2060-2088`).
fn switch_line(switch: &Switch, width: u16) -> Line<'static> {
    let palette = theme::palette();
    let pad = " ".repeat(COMPLETION_INDENT);
    let text = match switch {
        Switch::Location { directory } => {
            return Line::from(vec![
                Span::raw(pad),
                Span::styled("\u{21b3} Moved to ", palette.dim),
                Span::styled(directory.to_string(), palette.info),
            ]);
        }
        Switch::Agent { agent, previous } => match previous {
            Some(previous) if previous != agent => format!(
                "Switched agent from {} to {}",
                titlecase(previous),
                titlecase(agent)
            ),
            _ => format!("Switched agent to {}", titlecase(agent)),
        },
        Switch::Model { model, previous } => switch_label(model, previous.as_ref()),
    };
    let line = indent_wrapped(&text, width, COMPLETION_INDENT)
        .into_iter()
        .next()
        .unwrap_or_default();
    line.style(palette.dim)
}

/// Upstream `switchLabel` (`util/model.ts:10`) without the catalogue's
/// display names: the same provider and id is a variant switch, anything else
/// names the model by `formatRef` (`provider/id[/variant]`).
fn switch_label(model: &ModelChoice, previous: Option<&ModelChoice>) -> String {
    if let Some(previous) = previous
        && previous.provider_id == model.provider_id
        && previous.id == model.id
    {
        return format!(
            "Switched variant to {}",
            model.variant.as_deref().unwrap_or("default")
        );
    }
    match &model.variant {
        Some(variant) => format!(
            "Switched model to {}/{}/{variant}",
            model.provider_id, model.id
        ),
        None => format!("Switched model to {}/{}", model.provider_id, model.id),
    }
}

/// Upstream `SessionNoticeMessageV2` (`◈ description`, default
/// `Instructions updated`) and `SessionSkillMessage` (`→ Skill name`): muted
/// complete inline tool rows.
fn notice_lines(notice: &Notice, width: u16) -> Vec<Line<'static>> {
    let (icon, pending, text) = match notice {
        Notice::System { description } => (
            "\u{25c8}",
            "Notice",
            Cow::Borrowed(description.as_deref().unwrap_or("Instructions updated")),
        ),
        Notice::Skill { name } => ("\u{2192}", "Skill", Cow::Owned(format!("Skill {name}"))),
    };
    let spec = InlineSpec {
        icon,
        pending,
        complete: Some(&text),
        error: None,
    };
    let view = RowView {
        state: ToolState::Success,
        spinner: SPINNER_FRAMES[0],
        hovered: false,
        error_expanded: false,
    };
    tool_row::lines(&spec, view, width)
}

/// Upstream `AssistantFooter` (`routes/session/index.tsx:2000`): the agent in
/// its colour, then muted ` · model`, ` · duration`, ` · N.N tok/s`. The model
/// is the bare id (`General · k3-256k · 5m 3s · 32.1 tok/s` in the gold
/// capture); `TurnFooter::model` carries upstream's `formatRef`
/// (`providerID/id`), so the provider prefix is dropped here, as the composer's
/// metadata row already draws it separately. The width gates are upstream's;
/// `width` here is the timeline's, a few columns short of the terminal's, which
/// only matters at the 28/36 boundaries.
fn footer_line(footer: &TurnFooter, width: u16) -> Line<'static> {
    let palette = theme::palette();
    // A failed message's agent is muted (`AssistantFooter`).
    let mut agent_style = if footer.error.is_some() {
        palette.dim
    } else {
        palette.agent
    };
    agent_style.bg = None;
    let mut spans = vec![Span::raw("   ")];
    if let Some(agent) = &footer.agent {
        spans.push(Span::styled(titlecase(agent), agent_style));
    }
    if width >= 28
        && let Some(model) = &footer.model
    {
        let id = crate::ui::models::decode(model).map_or(model.as_str(), |(_, id)| id);
        spans.push(Span::styled(format!(" \u{b7} {id}"), palette.dim));
    }
    if let Some(ms) = footer.duration_ms.filter(|ms| *ms > 0)
        && !(28..36).contains(&width)
    {
        spans.push(Span::styled(
            format!(" \u{b7} {}", format_duration(ms)),
            palette.dim,
        ));
    }
    if let Some(rate) = footer.tokens_per_second {
        spans.push(Span::styled(
            format!(" \u{b7} {rate:.1} tok/s"),
            palette.dim,
        ));
    }
    Line::from(spans)
}

/// The strip a block with `files` reserves rows for, when this terminal draws
/// pixels and the attachment list has images: `at` is the row the first image
/// starts on, so drawing and hit-testing read one number.
///
/// A surface then adds its own padding rows around it: the user block has the
/// frame's margin above and a `gap` plus the text box's padding below, a tool
/// block has upstream's `paddingTop`/`paddingBottom` of one row each
/// (`SessionImages`, `routes/session/index.tsx:2670-2712`).
fn thumbs_of(files: &[MediaMeta], metrics: Metrics, at: usize) -> Option<media::Thumbs> {
    if !media::supported() {
        return None;
    }
    let thumbs = media::Thumbs::new(at, files, metrics.thumb_height)?;
    // A band too narrow for a cell and for `+N more` would reserve rows nothing
    // draws into, so the block keeps its text and shows no strip.
    let band = u16::try_from(block_tool::content_width(metrics.width)).unwrap_or(u16::MAX);
    thumbs.strip.draws_in(band).then_some(thumbs)
}

/// Upstream user message (`routes/session/index.tsx` ~2370): the block-tool
/// frame with an agent-coloured rail, on `raised.base` (`raised.high` under
/// the pointer, `block_tool::surface`).
///
/// Above the text the block reserves a strip of thumbnail rows when the prompt
/// carried images and this terminal draws them (`SessionImages`, `:2656-2712`);
/// under it a `file`/`dir` badge row names every attachment (`:2447-2480`).
fn user_block(
    text: &str,
    files: &[MediaMeta],
    view: ViewState,
    metrics: Metrics,
) -> (Vec<Line<'static>>, Option<media::Thumbs>) {
    let palette = theme::palette();
    let band = block_tool::surface(palette, view.hovered);
    let rail = Span::styled(block_tool::RAIL, palette.agent.patch(band));
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut thumbs = None;
    if let Some(drawn) = thumbs_of(files, metrics, 1) {
        // The frame's leading blank row is the strip's `paddingTop`, so the
        // rows added here are the image rows, their `paddingBottom` and the
        // text box's `paddingTop` (`gap={1}` between the two boxes).
        rows.extend(std::iter::repeat_with(Line::default).take(usize::from(drawn.strip.height)));
        rows.push(Line::default());
        rows.push(Line::default());
        thumbs = Some(drawn);
    }
    rows.extend(
        block_tool::wrap(text, block_tool::content_width(metrics.width))
            .into_iter()
            .map(|row| Line::from(Span::styled(row, palette.text))),
    );
    if !files.is_empty() {
        rows.push(Line::default());
        rows.extend(attachment_labels(
            files,
            block_tool::content_width(metrics.width),
        ));
    }
    (block_tool::frame(rows, &rail, band, metrics.width), thumbs)
}

/// One row per attachment chip, wrapped at `width`: ` file name` on the badge
/// fill, the name on the receded chip behind it (`:2447-2480`). A directory is
/// labelled `dir`, every other type `file`.
fn attachment_labels(files: &[MediaMeta], width: usize) -> Vec<Line<'static>> {
    let palette = theme::palette();
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for file in files {
        let badge = if file.is_dir() { " dir " } else { " file " };
        let name = format!(" {} ", file.name.as_deref().unwrap_or("attachment"));
        let chip = Span::styled(badge, palette.image)
            .width()
            .saturating_add(Span::styled(name.clone(), palette.image_text).width());
        // One column between chips, then wrap like upstream's `flexWrap`.
        if used > 0 && used.saturating_add(1).saturating_add(chip) > width {
            lines.push(Line::from(std::mem::take(&mut spans)));
            used = 0;
        }
        if used > 0 {
            spans.push(Span::raw(" "));
            used += 1;
        }
        spans.push(Span::styled(badge, palette.image));
        spans.push(Span::styled(name, palette.image_text));
        used = used.saturating_add(chip);
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}
