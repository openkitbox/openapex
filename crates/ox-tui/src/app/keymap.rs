//! Global key bindings: the command palette and the leader sequences.
//!
//! Pure decision logic (no I/O, no app state): [`Keymap::feed`] takes a key, an
//! injected clock reading and whether global bindings are active, and says what
//! the app should do. The app owns the wiring.
//!
//! Upstream defaults (`packages/tui/src/config/keybind.ts`):
//! `command.palette.show = ctrl+p`, `leader = ctrl+x` with a 2000ms timeout
//! (`context/keymap.tsx:143`), `session.new = <leader>n`,
//! `session.list = <leader>l`, `session.child.first = <leader>down`, `model.list = <leader>m`, `agent.list = <leader>a`,
//! `opencode.status = <leader>s`, `session.sidebar.toggle = <leader>b`,
//! `session.queued_prompts = <leader>q`
//! (upstream's `app.exit` also lists `<leader>q`: the queue command takes
//! the key while prompts are queued, otherwise it declines and the app
//! exits; `App::run_command` makes that choice).
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
    (KeyCode::Down, CommandAction::Subagents),
    (KeyCode::Char('b'), CommandAction::ToggleSidebar),
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
    pub fn is_armed(&self) -> bool {
        self.armed.is_some()
    }
}

/// The key hint of `action` (`ctrl+x l`), if it has a binding.
pub fn hint(action: CommandAction) -> Option<String> {
    LEADER_KEYS
        .iter()
        .find(|(_, bound)| *bound == action)
        .map(|(code, _)| match code {
            KeyCode::Down => format!("{LEADER_LABEL} ↓"),
            KeyCode::Char(ch) => format!("{LEADER_LABEL} {ch}"),
            other => format!("{LEADER_LABEL} {other}"),
        })
}

/// `ctrl+c`: interrupt a running turn, otherwise quit. Works in every mode.
pub fn is_cancel(key: &KeyEvent) -> bool {
    has_ctrl(key, 'c')
}

/// `ctrl+f`: open the transcript search (an ox addition; upstream has none).
pub fn is_search(key: &KeyEvent) -> bool {
    has_ctrl(key, 'f')
}

/// `ctrl+v`: paste from the system clipboard (upstream `prompt.paste`,
/// `config/keybind.ts`: `ctrl+v`). Only the prompt acts on it.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn ctrl(ch: char) -> KeyEvent {
        key(KeyCode::Char(ch), KeyModifiers::CONTROL)
    }

    fn plain(ch: char) -> KeyEvent {
        key(KeyCode::Char(ch), KeyModifiers::NONE)
    }

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn ctrl_p_opens_the_palette() {
        let mut map = Keymap::default();
        assert_eq!(
            map.feed(&ctrl('p'), Instant::now(), true),
            Feed::Action(Action::OpenPalette)
        );
    }

    #[test]
    fn unbound_keys_pass_through() {
        let mut map = Keymap::default();
        let now = Instant::now();
        for event in [
            plain('n'),
            ctrl('c'),
            ctrl('d'),
            key(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            assert_eq!(map.feed(&event, now, true), Feed::Pass, "{event:?}");
        }
        assert!(!map.is_armed());
    }

    #[test]
    fn each_leader_follow_up_runs_its_command() {
        let cases = [
            ('n', CommandAction::New),
            ('l', CommandAction::Sessions),
            ('m', CommandAction::Models),
            ('a', CommandAction::Agents),
            ('s', CommandAction::Status),
            ('q', CommandAction::QueuedPrompts),
        ];
        for (ch, action) in cases {
            let mut map = Keymap::default();
            let t0 = Instant::now();
            assert_eq!(map.feed(&ctrl('x'), t0, true), Feed::Consumed);
            assert!(map.is_armed());
            assert_eq!(
                map.feed(&plain(ch), at(t0, 100), true),
                Feed::Action(Action::Run(action)),
                "leader {ch}"
            );
            assert!(!map.is_armed(), "the follow-up disarms the leader");
        }
    }

    #[test]
    fn leader_b_toggles_the_sidebar() {
        let mut map = Keymap::default();
        let t0 = Instant::now();
        map.feed(&ctrl('x'), t0, true);
        assert_eq!(
            map.feed(&plain('b'), at(t0, 100), true),
            Feed::Action(Action::Run(CommandAction::ToggleSidebar))
        );
    }

    #[test]
    fn leader_down_arrow_opens_the_subagent_picker() {
        let mut map = Keymap::default();
        let t0 = Instant::now();
        map.feed(&ctrl('x'), t0, true);
        assert_eq!(
            map.feed(&key(KeyCode::Down, KeyModifiers::NONE), at(t0, 100), true),
            Feed::Action(Action::Run(CommandAction::Subagents))
        );
        assert!(!map.is_armed());
        // With a modifier held it is not the binding.
        map.feed(&ctrl('x'), t0, true);
        assert_eq!(
            map.feed(&key(KeyCode::Down, KeyModifiers::ALT), at(t0, 100), true),
            Feed::Consumed
        );
    }

    #[test]
    fn the_leader_expires_after_two_seconds() {
        let mut map = Keymap::default();
        let t0 = Instant::now();
        map.feed(&ctrl('x'), t0, true);
        // Just inside the window still counts.
        let mut inside = Keymap::default();
        inside.feed(&ctrl('x'), t0, true);
        assert_eq!(
            inside.feed(&plain('n'), at(t0, 1999), true),
            Feed::Action(Action::Run(CommandAction::New))
        );
        // At the deadline the leader is gone and the key is an ordinary one.
        assert_eq!(map.feed(&plain('n'), at(t0, 2000), true), Feed::Pass);
        assert!(!map.is_armed());
    }

    #[test]
    fn an_expired_leader_does_not_swallow_a_new_binding() {
        let mut map = Keymap::default();
        let t0 = Instant::now();
        map.feed(&ctrl('x'), t0, true);
        assert_eq!(
            map.feed(&ctrl('p'), at(t0, 5000), true),
            Feed::Action(Action::OpenPalette)
        );
        map.feed(&ctrl('x'), at(t0, 6000), true);
        assert!(map.is_armed(), "a fresh leader arms again");
    }

    #[test]
    fn an_invalid_follow_up_cancels_the_leader_and_is_swallowed() {
        for invalid in [
            plain('z'),
            plain('N'),
            key(KeyCode::Char('n'), KeyModifiers::ALT),
            ctrl('n'),
            ctrl('x'),
            ctrl('p'),
            key(KeyCode::Esc, KeyModifiers::NONE),
            key(KeyCode::Enter, KeyModifiers::NONE),
        ] {
            let mut map = Keymap::default();
            let t0 = Instant::now();
            map.feed(&ctrl('x'), t0, true);
            assert_eq!(
                map.feed(&invalid, at(t0, 10), true),
                Feed::Consumed,
                "{invalid:?}"
            );
            assert!(!map.is_armed(), "{invalid:?}");
            // The sequence is over: the next key is judged on its own.
            assert_eq!(map.feed(&plain('n'), at(t0, 20), true), Feed::Pass);
        }
    }

    #[test]
    fn ctrl_c_cancels_the_leader_but_still_reaches_the_app() {
        let mut map = Keymap::default();
        let t0 = Instant::now();
        map.feed(&ctrl('x'), t0, true);
        assert_eq!(map.feed(&ctrl('c'), at(t0, 10), true), Feed::Pass);
        assert!(!map.is_armed());
    }

    #[test]
    fn disabled_bindings_pass_everything_and_drop_a_pending_leader() {
        let mut map = Keymap::default();
        let t0 = Instant::now();
        assert_eq!(map.feed(&ctrl('x'), t0, false), Feed::Pass);
        assert_eq!(map.feed(&ctrl('p'), t0, false), Feed::Pass);
        assert!(!map.is_armed());

        map.feed(&ctrl('x'), t0, true);
        assert_eq!(map.feed(&plain('n'), at(t0, 10), false), Feed::Pass);
        assert!(!map.is_armed(), "a modal that opened mid-sequence ends it");
        assert_eq!(map.feed(&plain('n'), at(t0, 20), true), Feed::Pass);
    }

    #[test]
    fn key_releases_are_ignored() {
        let mut map = Keymap::default();
        let mut release = ctrl('x');
        release.kind = KeyEventKind::Release;
        assert_eq!(map.feed(&release, Instant::now(), true), Feed::Pass);
        assert!(!map.is_armed());
    }

    #[test]
    fn ctrl_v_is_the_clipboard_paste_key_and_not_a_global_binding() {
        assert!(is_paste(&ctrl('v')));
        assert!(!is_paste(&plain('v')));
        assert!(!is_paste(&ctrl('p')));
        assert!(!is_paste(&key(
            KeyCode::Char('v'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )));
        let mut release = ctrl('v');
        release.kind = KeyEventKind::Release;
        assert!(!is_paste(&release), "a release is not a second paste");
        assert_eq!(
            Keymap::default().feed(&ctrl('v'), Instant::now(), true),
            Feed::Pass,
            "the prompt owns it, so it is not a global binding"
        );
    }

    #[test]
    fn hints_come_from_the_leader_table() {
        assert_eq!(hint(CommandAction::Sessions).as_deref(), Some("ctrl+x l"));
        assert_eq!(hint(CommandAction::New).as_deref(), Some("ctrl+x n"));
        assert_eq!(hint(CommandAction::Models).as_deref(), Some("ctrl+x m"));
        assert_eq!(
            hint(CommandAction::QueuedPrompts).as_deref(),
            Some("ctrl+x q"),
            "leader q opens the queue"
        );
        assert_eq!(
            hint(CommandAction::Exit),
            None,
            "leader q now belongs to the queue"
        );
        assert_eq!(hint(CommandAction::Subagents).as_deref(), Some("ctrl+x ↓"));
        assert_eq!(
            hint(CommandAction::ToggleSidebar).as_deref(),
            Some("ctrl+x b"),
            "leader b toggles the sidebar"
        );
        assert_eq!(
            hint(CommandAction::Shells),
            None,
            "leader b moved to the sidebar"
        );
        assert_eq!(hint(CommandAction::Fork), None);
        assert_eq!(hint(CommandAction::Clear), None);
    }
}
