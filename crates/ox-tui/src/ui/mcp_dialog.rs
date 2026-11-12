//! The MCP servers dialog (upstream `component/dialog-mcp.tsx:15-177`, opened
//! by `mcp.list` / `/mcps`, `app.tsx:967-973`).
//!
//! [`McpDialog`] owns the server list as last fetched (sorted by name), the
//! servers with a connect/disconnect in flight, the [`Select`] that shows them
//! and the error details view when one is open. The server list stays the only
//! source of status; `loading` only overlays `Connecting …` until the owner
//! reports the request settled. Nothing here does I/O: `space` hands back
//! [`TOGGLE_KEY`] and the owner asks [`McpDialog::wants_connect`] which call to
//! make.
//!
//! Deviations from upstream:
//! - D1: no `needs_auth` state, `Sign in required →` footer or integration
//!   sign-in; ox has no MCP OAuth.
//! - D3: the search field reads `Search`, not `Search MCP servers`; upstream's
//!   `searchInTitle` / `titlePlaceholder` only apply under the off-by-default
//!   `new-designs` experiment (`ui/dialog-select.tsx:655,687`).
//! - Every failed server counts as having an error: one without a message
//!   still shows `view error` and opens the details with
//!   `Unknown MCP connection error` (upstream's fallback, `dialog-mcp.tsx:164`).
//!
//! The details view's own deviations (D4, D5) are in `ui/error_details.rs`.

use std::collections::BTreeSet;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ox_client::rest::{McpRef, McpStatus};
use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};

use super::dialog::{DialogOutcome, Hint, Item, Select, Tone, draw_select};
use super::error_details::{self, DetailsKey, ErrorDetails};
use crate::theme::Palette;

/// `Dialog::tag` of the dialog.
pub const TAG: &str = "mcp";
/// `Action` key of `space`: connect or disconnect the server in `value`.
pub const TOGGLE_KEY: char = ' ';
/// `Action` key of `c` in the details: copy the text in `value`.
pub const COPY_KEY: char = 'c';
/// `Action` key of `i` in the details: start a session drafted with `value`.
pub const INVESTIGATE_KEY: char = 'i';

const TITLE: &str = "MCP servers";
const UNKNOWN_ERROR: &str = "Unknown MCP connection error";

/// The MCP server list, or one server's error details over it.
#[derive(Debug, Clone, PartialEq)]
pub struct McpDialog {
    /// Sorted by name.
    servers: Vec<McpRef>,
    /// Servers whose connect/disconnect has not settled yet.
    loading: BTreeSet<String>,
    select: Select,
    detail: Option<ErrorDetails>,
}

impl McpDialog {
    #[must_use]
    pub fn new(mut servers: Vec<McpRef>) -> Self {
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        let loading = BTreeSet::new();
        let select = Select::new(TITLE, items(&servers, &loading));
        let mut dialog = Self {
            servers,
            loading,
            select,
            detail: None,
        };
        dialog.refresh_hints();
        dialog
    }

    /// The panel title: the details' while they are open.
    #[must_use]
    pub fn title(&self) -> &str {
        self.detail.as_ref().map_or(TITLE, |detail| detail.title())
    }

    /// Show a freshly fetched list; the cursor stays on the same server.
    pub fn update(&mut self, mut servers: Vec<McpRef>) {
        servers.sort_by(|a, b| a.name.cmp(&b.name));
        self.servers = servers;
        self.rebuild();
    }

    /// The connect/disconnect of `server` finished, either way.
    pub fn settle(&mut self, server: &str) {
        if self.loading.remove(server) {
            self.rebuild();
        }
    }

    /// Whether toggling `server` means connecting it: anything but a
    /// connected server connects (`dialog-mcp.tsx:124`).
    #[must_use]
    pub fn wants_connect(&self, server: &str) -> bool {
        !self
            .servers
            .iter()
            .any(|entry| entry.name == server && entry.status == McpStatus::Connected)
    }

    /// The details' copy was issued: show `✓ copied`.
    pub fn set_copied(&mut self) {
        if let Some(detail) = &mut self.detail {
            detail.set_copied();
        }
    }

    /// A key while the dialog is open: `space` toggles the focused server,
    /// enter opens a failed server's details, the rest drives the list; with
    /// the details open they take every key.
    pub fn on_key(&mut self, key: KeyEvent) -> DialogOutcome {
        if let Some(detail) = &mut self.detail {
            return match detail.on_key(key) {
                DetailsKey::Back => {
                    self.detail = None;
                    DialogOutcome::Changed
                }
                DetailsKey::Copy => DialogOutcome::Action {
                    key: COPY_KEY,
                    value: detail.text(),
                },
                DetailsKey::Investigate => DialogOutcome::Action {
                    key: INVESTIGATE_KEY,
                    value: detail.prompt(),
                },
                DetailsKey::Changed => DialogOutcome::Changed,
                DetailsKey::Idle => DialogOutcome::Idle,
            };
        }
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            // The toggle binding takes `space` before the search field does.
            KeyCode::Char(TOGGLE_KEY) if plain => self.toggle(),
            KeyCode::Enter => self.open_detail(),
            _ => {
                let outcome = self.select.on_key(key);
                self.refresh_hints();
                outcome
            }
        }
    }

    /// Paste into the search field; the details have none.
    pub fn on_paste(&mut self, text: &str) -> DialogOutcome {
        if self.detail.is_some() {
            return DialogOutcome::Idle;
        }
        let outcome = self.select.on_paste(text);
        self.refresh_hints();
        outcome
    }

    /// `space` (`toggle`, `dialog-mcp.tsx:113-132`): ignored while the server
    /// is connecting either way.
    fn toggle(&mut self) -> DialogOutcome {
        let Some(name) = self.select.selected_value() else {
            return DialogOutcome::Idle;
        };
        let busy = self.loading.contains(name)
            || self
                .servers
                .iter()
                .find(|server| server.name == name)
                .is_none_or(|server| server.status == McpStatus::Pending);
        if busy {
            return DialogOutcome::Idle;
        }
        let name = name.to_owned();
        self.loading.insert(name.clone());
        self.rebuild();
        DialogOutcome::Action {
            key: TOGGLE_KEY,
            value: name,
        }
    }

    /// Enter (`select`, `dialog-mcp.tsx:100-109`): the details of a failed
    /// server; any other row stays put.
    fn open_detail(&mut self) -> DialogOutcome {
        let Some(name) = self.select.selected_value() else {
            return DialogOutcome::Idle;
        };
        let Some(McpStatus::Failed { error }) = self
            .servers
            .iter()
            .find(|server| server.name == name)
            .map(|server| &server.status)
        else {
            return DialogOutcome::Idle;
        };
        let error = if error.is_empty() {
            UNKNOWN_ERROR
        } else {
            error.as_str()
        };
        self.detail = Some(ErrorDetails::new(
            format!("MCP server: {name}"),
            error.to_owned(),
            Some(format!("Status: failed\nConfiguration: mcp.servers.{name}")),
        ));
        DialogOutcome::Changed
    }

    fn rebuild(&mut self) {
        self.select
            .replace_items(items(&self.servers, &self.loading));
        self.refresh_hints();
    }

    /// The footer for the focused row (`toggleTitle` + `footerHints`,
    /// `dialog-mcp.tsx:85-98, :147-157`): the toggle first, then `view error`.
    /// With no row focused there is nothing to toggle, so no hint.
    fn refresh_hints(&mut self) {
        let Some(name) = self.select.selected_value() else {
            self.select.set_hints(Vec::new());
            return;
        };
        let status = self
            .servers
            .iter()
            .find(|server| server.name == name)
            .map(|server| &server.status);
        let toggle = match status {
            Some(McpStatus::Connected) => "disconnect (all sessions)",
            Some(McpStatus::Failed { .. }) => "retry (all sessions)",
            _ => "connect (all sessions)",
        };
        let mut hints = vec![Hint::new(toggle, "space")];
        if matches!(status, Some(McpStatus::Failed { .. })) {
            hints.push(Hint::new("view error", "enter"));
        }
        self.select.set_hints(hints);
    }
}

/// One row per server, its status as the footer (`Status`,
/// `dialog-mcp.tsx:20-34`, coloured by `statusColor`, :57-62).
#[must_use]
fn items(servers: &[McpRef], loading: &BTreeSet<String>) -> Vec<Item> {
    servers
        .iter()
        .map(|server| {
            let (footer, tone) = match &server.status {
                _ if loading.contains(&server.name) => ("Connecting \u{2026}", Tone::Dim),
                McpStatus::Pending => ("Connecting \u{2026}", Tone::Dim),
                McpStatus::Connected => ("Connected \u{2713}", Tone::Success),
                McpStatus::Failed { .. } => ("Failed !", Tone::Warn),
                McpStatus::Disabled => ("Disabled \u{25cb}", Tone::Dim),
            };
            Item::new(&server.name, &server.name)
                .footer(footer)
                .footer_tone(tone)
        })
        .collect()
}

/// Draw the list, or the details while they are open. Returns where the
/// search cursor goes; the details have no cursor.
pub fn draw(buf: &mut Buffer, area: Rect, dialog: &McpDialog, pal: &Palette) -> Option<Position> {
    match &dialog.detail {
        Some(detail) => {
            error_details::draw(buf, area, detail, pal);
            None
        }
        None => Some(draw_select(buf, area, &dialog.select, pal)),
    }
}
