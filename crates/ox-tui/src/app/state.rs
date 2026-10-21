// state.rs — view state only. Protocol/domain state lives in `DataStore`;
// scroll/sticky live in `Transcript`. Copying those fields here created two
// sources of truth, so this struct stays intentionally tiny.
#[derive(Debug, Default)]
pub struct ViewState {
    pub should_quit: bool,
}

impl ViewState {
    pub fn new() -> Self {
        Self::default()
    }
}
