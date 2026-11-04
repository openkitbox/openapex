//! `GET /ox/session/{sessionID}/media/{messageID}?index=N[&part=PARTID]` — an
//! ox-only read of one stored attachment's raw bytes.
//!
//! A transcript row carries its attachments inline as base64 (millions of
//! characters for one image), so a client that keeps only attachment metadata
//! fetches a payload when a viewer asks for it. There is no upstream path for
//! this: `/api/session/{id}/message` ships the whole JSON, payload included.
//!
//! Addressing: `part` selects a tool call of an assistant message, `index`
//! selects the slot.
//! - without `part` the row must be a user prompt; `index` addresses
//!   `files[index]`, whose `data` is plain base64 and whose `name` is optional;
//! - with `part` it addresses `content[index]` of that tool part's completed or
//!   error state, which must be a `file` content whose `uri` is a base64
//!   `data:` URL.
//!
//! Guarantees: the body is raw bytes, never base64, and no error body echoes a
//! payload. A missing message row or part is 404; a bad index, a non-`file`
//! content or an undecodable payload is 400. Rows are read through
//! [`load_message_row`], so the session message layer keeps owning that SQL.
//!
//! The served type is the magic-byte sniff of the decoded bytes
//! ([`ox_tools::read::sniff_media`]: PNG/JPEG/GIF/WebP/PDF), else
//! `application/octet-stream`; a stored `mime` is a client's declaration and is
//! never echoed, matching the write path ([`super::attachment`]). The response
//! also carries `X-Content-Type-Options: nosniff`, so a browser cannot
//! re-interpret the bytes as some other type.
//!
//! The tagger's drop overlay is not applied: it replaces text only, and the
//! stored bytes are what a viewer renders.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use salvo::http::HeaderName;
use salvo::http::HeaderValue;
use salvo::http::header::{CONTENT_LENGTH, CONTENT_TYPE};
use salvo::prelude::*;
use serde_json::Value;

use ox_schema::message::{AssistantContent, Message, ToolState};
use ox_schema::support::Content;

use super::attachment::parse_data_url;
use super::session::{invalid_request, load_message_row, message_not_found, unknown_error};
use super::state;

/// The attachment's file name, when the row carries one. A header rather than
/// part of the type: the client needs a name for display only, and building a
/// multipart body for it would cost the response a second copy.
const NAME_HEADER: &str = "x-ox-name";

#[handler]
pub(crate) async fn media(req: &mut Request, depot: &mut Depot, res: &mut Response) {
    let Some(app) = state(depot, res) else {
        return;
    };
    let Some(session_id) = req.param::<String>("sessionID") else {
        invalid_request(res, "Missing sessionID");
        return;
    };
    let Some(message_id) = req.param::<String>("messageID") else {
        invalid_request(res, "Missing messageID");
        return;
    };
    let Some(index) = req.query::<usize>("index") else {
        invalid_request(res, "Missing or invalid index");
        return;
    };

    // The read is scoped by both ids, so it needs no separate session
    // existence query: an unknown session and an unknown message are one 404.
    let row = match load_message_row(&app.db, &session_id, &message_id).await {
        Ok(Some(row)) => row,
        Ok(None) => {
            message_not_found(res, &session_id, &message_id);
            return;
        }
        Err(error) => {
            unknown_error(res, &error);
            return;
        }
    };

    let mut value: Value = match serde_json::from_str(&row.data) {
        Ok(value) => value,
        Err(error) => {
            unknown_error(res, &anyhow::Error::from(error));
            return;
        }
    };
    // `id` and `type` live in the columns; merge them back the same way
    // `message_get` does before decoding.
    Message::merge_row_columns(&message_id, &row.kind, &mut value);
    let message = match Message::from_merged_row(value) {
        Ok(message) => message,
        Err(error) => {
            unknown_error(res, &anyhow::Error::from(error));
            return;
        }
    };

    let part = req.query::<String>("part");
    let (data, name) = match lookup(&message, part.as_deref(), index) {
        Slot::Bytes { data, name } => (data, name),
        Slot::NoPart => {
            message_not_found(res, &session_id, &message_id);
            return;
        }
        Slot::Invalid(reason) => {
            invalid_request(res, reason);
            return;
        }
    };
    let Ok(bytes) = STANDARD.decode(data.as_bytes()) else {
        invalid_request(res, "Attachment payload is not valid base64");
        return;
    };

    // A stored type is a client's declaration, so the wire type comes from the
    // bytes themselves; `nosniff` keeps a browser from second-guessing it.
    let mime = ox_tools::read::sniff_media(&bytes).unwrap_or("application/octet-stream");
    res.status_code(StatusCode::OK);
    let headers = res.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(mime));
    headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    if let Some(name) = name {
        // A non-ASCII name cannot ride an HTTP header; it is display-only, so
        // the bytes still win over the label.
        if let Ok(value) = HeaderValue::from_str(name) {
            res.headers_mut()
                .insert(HeaderName::from_static(NAME_HEADER), value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(&bytes.len().to_string()) {
        res.headers_mut().insert(CONTENT_LENGTH, value);
    }
    res.body(bytes);
}

/// One addressed attachment slot of a stored message, borrowed from the row.
enum Slot<'a> {
    /// Base64 `data` without a `data:` prefix, and its optional name. The
    /// stored `mime` is deliberately not carried: the type served is sniffed
    /// from the decoded bytes.
    Bytes {
        data: &'a str,
        name: Option<&'a str>,
    },
    /// `part` names no tool part of this message: the row exists, the part
    /// does not.
    NoPart,
    /// The request names nothing readable: `index` is out of range, the
    /// content is not a `file`, or its payload is not a base64 data URL.
    Invalid(&'static str),
}

/// Resolve `(part, index)` against a decoded message. A user prompt addresses
/// `files`, an assistant message addresses one tool part's `content`.
fn lookup<'row>(message: &'row Message, part: Option<&str>, index: usize) -> Slot<'row> {
    match (part, message) {
        (None, Message::User { prompt, .. }) => {
            match prompt.files.as_deref().and_then(|files| files.get(index)) {
                Some(file) => Slot::Bytes {
                    data: &file.data,
                    name: file.name.as_deref(),
                },
                None => Slot::Invalid("No attachment at that index"),
            }
        }
        (None, _) => Slot::Invalid("Message has no attachments"),
        (Some(part), Message::Assistant(message)) => {
            let found = message.content.iter().find_map(|content| match content {
                AssistantContent::Tool { id, state, .. } if id == part => Some(state.as_ref()),
                _ => None,
            });
            let Some(state) = found else {
                return Slot::NoPart;
            };
            let Some(content) = tool_contents(state).get(index) else {
                return Slot::Invalid("No tool content at that index");
            };
            let Content::File { uri, name, .. } = content else {
                return Slot::Invalid("Tool content at that index is not a file");
            };
            let Some((_, payload)) = parse_data_url(uri) else {
                return Slot::Invalid("Attachment is not a base64 data URL");
            };
            Slot::Bytes {
                data: payload,
                name: name.as_deref(),
            }
        }
        (Some(_), _) => Slot::NoPart,
    }
}

/// The content blocks of a tool state; empty while the call has no result.
fn tool_contents(state: &ToolState) -> &[Content] {
    match state {
        ToolState::Completed { content, .. } => content,
        ToolState::Error { content, .. } => content.as_deref().unwrap_or_default(),
        ToolState::Streaming { .. } | ToolState::Running { .. } => &[],
    }
}
