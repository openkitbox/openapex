// state.rs — view state only. Protocol/domain state lives in `DataStore`;
// scroll/sticky live in `Transcript`. Copying those fields here created two
// sources of truth, so this struct stays intentionally tiny.
#[derive(Debug, Default)]
pub struct ViewState {
    pub should_quit: bool,
}

impl ViewState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

/// Which screen the app is showing. `Home` exists before any session is
/// created; `--session`/demo start directly in `Session`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Home,
    Session,
}
