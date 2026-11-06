//! Global key bindings: the command palette and the leader sequences.
//!
//! Pure decision logic (no I/O, no app state): [`Keymap::feed`] takes a key, an
//! injected clock reading and whether global bindings are active, and says what
//! the app should do. The app owns the wiring.
//!
//! Upstream defaults (`packages/tui/src/config/keybind.ts`):
//! `command.palette.show = ctrl+p`, `leader = ctrl+x` with a 2000ms timeout
//! (`context/keymap.tsx:143`), `session.new = <leader>n`,
//! `session.list = <leader>l`, `model.list = <leader>m`, `agent.list = <leader>a`,
//! `opencode.status = <leader>s`,
//! `session.queued_prompts = <leader>q`
//! (upstream's `app.exit` also lists `<leader>q`: the queue command takes
//! the key while prompts are queued, otherwise it declines and the app
//! exits; `App::run_command` makes that choice).
//! `session.child.first` is a bare `↓` (`config/keybind.ts:138`), not a leader
//! sequence: the same key runs `prompt.history.next`, which returns `false`
//! once the history is exhausted (`component/prompt/index.tsx:1086`), so
//! the picker takes the key after the prompt declines it
//! (`App::declined_key`). It stays out of [`LEADER_KEYS`] for that reason and
//! still reports a hint here, because the palette lists it beside the
//! leader-bound commands.
//! ox has no sidebar, so `<leader>b` (upstream `session.sidebar.toggle`)
//! opens the Shells list instead.
//! A key that does not continue a pending leader sequence cancels it and is
//! swallowed (upstream `registerEscapeClearsPendingSequence` plus the
//! timed-leader's `clearPendingSequence`).

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::ui::commands::CommandAction;

/// How the leader is written in key hints.
pub const LEADER_LABEL: &str = "ctrl+x";
/// How long a pressed leader waits for its follow-up key.
pub const LEADER_TIMEOUT: Duration = Duration::from_secs(2);

/// `<leader><key>` bindings. The palette's key hints derive from this table.
const LEADER_KEYS: &[(KeyCode, CommandAction)] = &[
    (KeyCode::Char('n'), CommandAction::New),
    (KeyCode::Char('l'), CommandAction::Sessions),
    (KeyCode::Char('m'), CommandAction::Models),
    (KeyCode::Char('a'), CommandAction::Agents),
    (KeyCode::Char('s'), CommandAction::Status),
    (KeyCode::Char('q'), CommandAction::QueuedPrompts),
    (KeyCode::Char('b'), CommandAction::Shells),
    (KeyCode::Char('i'), CommandAction::Images),
];

/// What a global binding asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    OpenPalette,
    Run(CommandAction),
}

/// What [`Keymap::feed`] did with a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// Not a global binding: the key goes on to whoever owns the keyboard.
    Pass,
    /// Swallowed: the leader was armed, or a pending sequence was cancelled.
    Consumed,
    Action(Action),
}

/// The leader state machine: idle, or armed since a clock reading.
#[derive(Debug, Default)]
pub struct Keymap {
    armed: Option<Instant>,
}

impl Keymap {
    /// Judge `key` at `now`. With `enabled` false (a modal is up, a text field
    /// owns the keyboard) nothing is bound and a pending leader is dropped.
    pub fn feed(&mut self, key: &KeyEvent, now: Instant, enabled: bool) -> Feed {
        if key.kind == KeyEventKind::Release {
            return Feed::Pass;
        }
        let armed_at = self.armed.take();
        if !enabled || is_cancel(key) {
            return Feed::Pass;
        }
        if let Some(at) = armed_at
            && now.saturating_duration_since(at) < LEADER_TIMEOUT
        {
            return match follow_up(key) {
                Some(action) => Feed::Action(Action::Run(action)),
                None => Feed::Consumed,
            };
        }
        if is_ctrl(key, 'x') {
            self.armed = Some(now);
            return Feed::Consumed;
        }
        if is_ctrl(key, 'p') {
            return Feed::Action(Action::OpenPalette);
        }
        Feed::Pass
    }

    /// Forget a pending leader (a modal opened over it).
    pub fn reset(&mut self) {
        self.armed = None;
    }

    /// Whether a leader is waiting for its follow-up key (ignoring the timeout).
    #[must_use]
    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }
}

/// The key hint of `action` (`ctrl+x l`), if it has a binding.
#[must_use]
pub fn hint(action: CommandAction) -> Option<String> {
    // `session.child.first` is a bare `↓`: no leader label, because the key
    // needs none upstream either (the palette lists it the same way).
    if action == CommandAction::Subagents {
        return Some(crate::ui::prompt::SUBAGENT_SHORTCUT.to_owned());
    }
    LEADER_KEYS
        .iter()
        .find(|(_, bound)| *bound == action)
        .map(|(code, _)| match code {
            KeyCode::Char(ch) => format!("{LEADER_LABEL} {ch}"),
            other => format!("{LEADER_LABEL} {other}"),
        })
}

/// `ctrl+c`: quit (upstream `app.exit`). Works in every mode.
#[must_use]
pub fn is_cancel(key: &KeyEvent) -> bool {
    has_ctrl(key, 'c')
}

/// `ctrl+f`: open the transcript search (an ox addition; upstream has none).
#[must_use]
pub fn is_search(key: &KeyEvent) -> bool {
    has_ctrl(key, 'f')
}

/// `ctrl+v`: paste from the system clipboard (upstream `prompt.paste`,
/// `config/keybind.ts`: `ctrl+v`). Only the prompt acts on it.
#[must_use]
pub fn is_paste(key: &KeyEvent) -> bool {
    key.kind != KeyEventKind::Release && is_ctrl(key, 'v')
}

fn follow_up(key: &KeyEvent) -> Option<CommandAction> {
    if !key.modifiers.is_empty() {
        return None;
    }
    LEADER_KEYS
        .iter()
        .find(|(bound, _)| *bound == key.code)
        .map(|(_, action)| *action)
}

/// `ctrl+<ch>` and nothing else held.
fn is_ctrl(key: &KeyEvent, ch: char) -> bool {
    key.code == KeyCode::Char(ch) && key.modifiers == KeyModifiers::CONTROL
}

/// `ctrl+<ch>`, other modifiers allowed: the pre-keymap `is_ctrl` semantics of
/// `ctrl+c` and `ctrl+f`, kept so their behaviour does not change.
fn has_ctrl(key: &KeyEvent, ch: char) -> bool {
    key.code == KeyCode::Char(ch) && key.modifiers.contains(KeyModifiers::CONTROL)
}
