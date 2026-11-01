use ox_llm::cardinal::wire::{ImageUrl, UserContent, UserPart};
use ox_schema::support::{FileAttachment, FileSource, Prompt};
/// A user prompt's wire text: the typed text plus attachment markers.
/// (Upstream `SessionPrompt.prepare` inlines resolved file content; 6a
/// stores attachments verbatim and marks them instead.)
///
/// This is the history/summary rendering: `build_chat_messages` uses
/// [`user_message_content`] instead, because resolved skill bodies go on the
/// wire but must stay out of the historian's serialised `message.text`.
pub(crate) fn user_prompt_text(prompt: &Prompt) -> String {
    let mut text = prompt.text.clone();
    for file in prompt.files.iter().flatten() {
        push_marker(&mut text, file);
    }
    text
}

/// The `[attached file: name (mime)]` marker line for an attachment that is
/// not rendered into model content (and every attachment in summaries).
fn push_marker(text: &mut String, file: &FileAttachment) {
    let name = file.name.as_deref().unwrap_or("file");
    text.push_str(&format!("\n[attached file: {name} ({})]", file.mime));
}

/// The wire content of a user message, in upstream `toLLMMessage` order
/// (skills → typed text → attachments). Upstream emits one content part per
/// skill, the typed text and each attachment piece, and its OpenAI-Chat
/// lowering concatenates text parts with an empty separator
/// (`packages/ai/src/protocols/openai-chat.ts:436-438`,
/// `content.map((part) => part.text).join("")`), so ox concatenates adjacent
/// text too rather than inserting separators: byte parity, and a skill body's
/// closing `</skill_content>` abuts the typed text exactly as it does
/// upstream. A skill whose `text` is missing contributes nothing.
///
/// Attachments follow upstream `attachmentContent`:
/// - `text/plain` and `application/x-directory` are inlined as text;
/// - `image/png|jpeg|gif|webp` become an `image_url` data-URL part (after an
///   `Attached file: <path>` line when the source is a `file:` URI), which
///   turns the content into a parts list;
/// - anything else keeps the `[attached file: …]` marker of
///   [`user_prompt_text`]. Deviation: upstream sends PDFs as media and drops
///   unknown types; ox has no PDF content part and keeps a marker so the
///   model knows a file existed.
///
/// With no image the result is a plain string, byte-identical to the
/// pre-attachment rendering for prompts without renderable attachments.
pub(crate) fn user_message_content(prompt: &Prompt) -> UserContent {
    let mut out = ContentBuilder::default();
    for text in prompt
        .skills
        .iter()
        .flatten()
        .filter_map(|skill| skill.text.as_deref())
    {
        out.text.push_str(text);
    }
    out.text.push_str(&prompt.text);
    for file in prompt.files.iter().flatten() {
        render_attachment(&mut out, file);
    }
    out.finish()
}

/// Accumulates user content: text runs merge, an image closes the current run.
#[derive(Default)]
struct ContentBuilder {
    parts: Vec<UserPart>,
    text: String,
}

impl ContentBuilder {
    /// Close the current text run as a part of its own.
    fn flush_text(&mut self) {
        if !self.text.is_empty() {
            self.parts.push(UserPart::Text {
                text: std::mem::take(&mut self.text),
            });
        }
    }

    fn image(&mut self, url: String) {
        self.flush_text();
        self.parts.push(UserPart::Image {
            image_url: ImageUrl { url },
        });
    }

    fn finish(mut self) -> UserContent {
        if self.parts.is_empty() {
            return UserContent::Text(self.text);
        }
        if !self.text.is_empty() {
            self.parts.push(UserPart::Text { text: self.text });
        }
        UserContent::Parts(self.parts)
    }
}

/// Nothing for the model to read: empty text, or no parts.
pub(super) fn content_is_blank(content: &UserContent) -> bool {
    match content {
        UserContent::Text(text) => text.is_empty(),
        UserContent::Parts(parts) => parts.is_empty(),
    }
}

/// Put `prefix` at the very start of the content: on the first text, or as a
/// text part of its own when the content opens with an image.
pub(super) fn prefix_content(content: UserContent, prefix: String) -> UserContent {
    match content {
        UserContent::Text(mut text) => {
            text.insert_str(0, &prefix);
            UserContent::Text(text)
        }
        UserContent::Parts(mut parts) => {
            match parts.first_mut() {
                Some(UserPart::Text { text }) => text.insert_str(0, &prefix),
                _ => parts.insert(0, UserPart::Text { text: prefix }),
            }
            UserContent::Parts(parts)
        }
    }
}

/// Render one attachment into `out` (upstream `attachmentContent`).
fn render_attachment(out: &mut ContentBuilder, file: &FileAttachment) {
    let location = match &file.source {
        FileSource::Uri { uri } => Some(uri.as_str()),
        FileSource::Inline => None,
    };
    match file.mime.as_str() {
        "text/plain" => {
            let label = file
                .name
                .as_deref()
                .or(location)
                .unwrap_or("inline attachment");
            let mark = out.text.len();
            out.text.push_str("\n\nAttached file: ");
            out.text.push_str(label);
            push_description(&mut out.text, file);
            out.text.push_str("\n\n");
            if !push_decoded_text(&mut out.text, &file.data) {
                out.text.truncate(mark);
                push_marker(&mut out.text, file);
            }
        }
        "application/x-directory" => {
            let label = location
                .and_then(file_uri_path)
                .or_else(|| file.name.clone())
                .or_else(|| location.map(str::to_owned));
            let mark = out.text.len();
            out.text.push_str("\n\nAttached directory: ");
            out.text.push_str(label.as_deref().unwrap_or("directory"));
            push_description(&mut out.text, file);
            let body_at = out.text.len();
            out.text.push_str("\n\n");
            if !push_decoded_text(&mut out.text, &file.data) {
                out.text.truncate(mark);
                push_marker(&mut out.text, file);
            } else if out.text.len() == body_at + 2 {
                out.text.truncate(body_at);
            }
        }
        "image/png" | "image/jpeg" | "image/gif" | "image/webp" => {
            if let Some(path) = location.and_then(file_uri_path) {
                // Upstream emits the location as a content part of its own.
                out.flush_text();
                out.parts.push(UserPart::Text {
                    text: format!("Attached file: {path}"),
                });
            }
            // The single unavoidable copy: the wire carries the payload inside
            // a `data:` URL (same boundary as tool-result images).
            out.image(format!("data:{};base64,{}", file.mime, file.data));
        }
        _ => push_marker(&mut out.text, file),
    }
}

fn push_description(text: &mut String, file: &FileAttachment) {
    if let Some(description) = &file.description {
        text.push_str("\nDescription: ");
        text.push_str(description);
    }
}

/// Append a base64 text payload to `out` as UTF-8 (lossy), decoded once and
/// copied once into `out`. `false` (with `out` untouched) when the stored
/// payload is not valid base64; the caller then falls back to a marker.
fn push_decoded_text(out: &mut String, data: &str) -> bool {
    use base64::Engine as _;
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return false;
    };
    out.push_str(&String::from_utf8_lossy(&bytes));
    true
}

/// The local path of a `file:` URI (upstream `attachmentLocation` /
/// `fileURLToPath`): query and fragment dropped, percent-decoded. `None` for
/// other schemes, a remote host, or a path that is not valid UTF-8. A local
/// helper rather than the `url` crate, which ox-engine does not otherwise
/// depend on.
fn file_uri_path(uri: &str) -> Option<String> {
    let scheme_end = uri.find(':')?;
    if !uri[..scheme_end].eq_ignore_ascii_case("file") {
        return None;
    }
    let rest = uri[scheme_end + 1..].strip_prefix("//")?;
    let rest = rest.split(['?', '#']).next()?;
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(slash) if rest[..slash].eq_ignore_ascii_case("localhost") => &rest[slash..],
        _ => return None,
    };
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_digit(*bytes.get(i + 1)?)?;
            let lo = hex_digit(*bytes.get(i + 2)?)?;
            decoded.push(hi << 4 | lo);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

/// Value of an ASCII hex digit; unlike `from_str_radix`, no sign is accepted.
fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).map(|digit| digit as u8)
}
