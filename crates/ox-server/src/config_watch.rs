use std::io;
use std::path::Path;

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::api::AppState;

#[derive(Debug, Default)]
struct ContentTracker {
    last: Option<Vec<u8>>,
}

impl ContentTracker {
    fn new(last: Option<Vec<u8>>) -> Self {
        Self { last }
    }

    fn update(&mut self, current: io::Result<Vec<u8>>) -> bool {
        let Ok(current) = current else {
            return false;
        };
        if self.last.as_ref() == Some(&current) {
            return false;
        }
        self.last = Some(current);
        true
    }
}

fn event_targets_config(event: &Event, parent: &Path, config_path: &Path) -> bool {
    if event.need_rescan() {
        return true;
    }
    if matches!(
        event.kind,
        EventKind::Modify(notify::event::ModifyKind::Name(_))
    ) {
        return event.paths.iter().any(|path| path.parent() == Some(parent));
    }
    matches!(
        event.kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    ) && event.paths.iter().any(|path| path == config_path)
}

fn read_config(path: &Path) -> io::Result<Vec<u8>> {
    std::fs::read(path)
}

fn log_read_failure(path: &Path, error: &io::Error) {
    tracing::warn!(path = %path.display(), %error, "config watcher could not read config");
}

/// Start the config watcher under the composition runtime.
pub(crate) fn spawn(
    state: &AppState,
    stop: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let config_path = state.config_path.clone();
    let directory = state.directory.to_string_lossy().into_owned();
    let state = state.clone();
    Some(tokio::spawn(async move {
        let (signals, mut pending) = mpsc::unbounded_channel();
        let parent = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let callback_path = config_path.clone();
        let callback_parent = parent.clone();
        let callback = move |result: notify::Result<Event>| {
            let relevant = match result {
                Ok(event) => event_targets_config(&event, &callback_parent, &callback_path),
                Err(_) => true,
            };
            if relevant {
                let _ = signals.send(());
            }
        };
        let mut watcher = match notify::recommended_watcher(callback) {
            Ok(watcher) => watcher,
            Err(error) => {
                tracing::warn!(error = %error, "config watcher could not start");
                return;
            }
        };
        if let Err(error) = watcher.watch(&parent, RecursiveMode::NonRecursive) {
            tracing::warn!(
                path = %parent.display(),
                %error,
                "config watcher could not watch config directory"
            );
            return;
        }

        let initial = match read_config(&config_path) {
            Ok(bytes) => Some(bytes),
            Err(error) => {
                log_read_failure(&config_path, &error);
                None
            }
        };
        let mut contents = ContentTracker::new(initial);
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                signal = pending.recv() => {
                    if signal.is_none() {
                        return;
                    }
                    while pending.try_recv().is_ok() {}
                    let current = read_config(&config_path);
                    let changed = match current {
                        Ok(bytes) => contents.update(Ok(bytes)),
                        Err(error) => {
                            log_read_failure(&config_path, &error);
                            false
                        }
                    };
                    if !changed {
                        continue;
                    }
                    if let Err(error) = crate::reload::run(&state, &directory).await {
                        tracing::warn!(error = %format!("{error:#}"), "config watcher reload failed");
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use notify::event::{CreateKind, Flag, ModifyKind, RemoveKind, RenameMode};

    #[test]
    fn source_only_rename_in_parent_is_a_reload_candidate() {
        let parent = Path::new("/tmp/project");
        let config = parent.join("ox.toml");
        let temporary = parent.join(".ox.toml.tmp");
        let rename = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            paths: vec![temporary],
            attrs: Default::default(),
        };

        assert!(event_targets_config(
            &rename,
            config.parent().expect("config parent"),
            &config
        ));
    }

    #[test]
    fn filters_events_to_the_watched_config_path() {
        let config = Path::new("/tmp/project/ox.toml");
        let matching = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![config.to_owned()],
            attrs: Default::default(),
        };
        let sibling = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![PathBuf::from("/tmp/project/other.toml")],
            attrs: Default::default(),
        };

        assert!(event_targets_config(
            &matching,
            config.parent().expect("config parent"),
            config
        ));
        assert!(!event_targets_config(
            &sibling,
            config.parent().expect("config parent"),
            config
        ));
    }

    #[test]
    fn sibling_rename_in_parent_is_a_reload_candidate() {
        let parent = Path::new("/tmp/project");
        let config = parent.join("ox.toml");
        let sibling = parent.join("other.toml");
        let rename = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            paths: vec![sibling],
            attrs: Default::default(),
        };

        assert!(event_targets_config(
            &rename,
            config.parent().expect("config parent"),
            &config
        ));
    }

    #[test]
    fn relative_watched_root_paths_are_compared_in_the_reported_form() {
        // notify 9 reports event paths with the same root representation passed to watch.
        let parent = Path::new("config");
        let config = parent.join("ox.toml");
        let modify = Event {
            kind: EventKind::Modify(ModifyKind::Any),
            paths: vec![parent.join("ox.toml")],
            attrs: Default::default(),
        };
        let rename = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
            paths: vec![parent.join(".ox.toml.tmp"), parent.join("ox.toml")],
            attrs: Default::default(),
        };
        let nested = Event {
            kind: EventKind::Create(CreateKind::File),
            paths: vec![parent.join("nested/ox.toml")],
            attrs: Default::default(),
        };

        assert!(event_targets_config(&modify, parent, &config));
        assert!(event_targets_config(&rename, parent, &config));
        assert!(!event_targets_config(&nested, parent, &config));
    }

    #[test]
    fn remove_targets_only_the_config_path() {
        let parent = Path::new("/tmp/project");
        let config = parent.join("ox.toml");
        let matching = Event {
            kind: EventKind::Remove(RemoveKind::File),
            paths: vec![config.clone()],
            attrs: Default::default(),
        };
        let sibling = Event {
            kind: EventKind::Remove(RemoveKind::File),
            paths: vec![parent.join("other.toml")],
            attrs: Default::default(),
        };

        assert!(event_targets_config(&matching, parent, &config));
        assert!(!event_targets_config(&sibling, parent, &config));
    }

    #[test]
    fn rescan_is_a_candidate_even_with_unrelated_paths() {
        let parent = Path::new("/tmp/project");
        let config = parent.join("ox.toml");
        let rescan = Event::new(EventKind::Other)
            .set_flag(Flag::Rescan)
            .add_path(parent.join("other.toml"));

        assert!(rescan.need_rescan());
        assert!(event_targets_config(&rescan, parent, &config));
    }

    #[test]
    fn rename_candidate_with_unchanged_bytes_is_suppressed() {
        let parent = Path::new("config");
        let config = parent.join("ox.toml");
        let rename = Event {
            kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
            paths: vec![parent.join(".ox.toml.tmp")],
            attrs: Default::default(),
        };
        let mut contents = ContentTracker::new(Some(b"a = 1".to_vec()));

        assert!(event_targets_config(&rename, parent, &config));
        assert!(!contents.update(Ok(b"a = 1".to_vec())));
        assert!(contents.update(Ok(b"a = 2".to_vec())));
    }

    #[test]
    fn suppresses_repeated_bytes_including_invalid_content() {
        let mut tracker = ContentTracker::new(Some(b"invalid = [".to_vec()));

        assert!(!tracker.update(Ok(b"invalid = [".to_vec())));
        assert!(tracker.update(Ok(b"[providers]".to_vec())));
        assert!(!tracker.update(Ok(b"[providers]".to_vec())));
    }

    #[test]
    fn missing_file_is_not_a_reload_candidate_and_recovery_is() {
        let mut tracker = ContentTracker::new(Some(b"valid = true".to_vec()));

        assert!(!tracker.update(Err(io::Error::from(io::ErrorKind::NotFound))));
        assert!(tracker.update(Ok(b"valid = false".to_vec())));
    }

    #[test]
    fn failed_reload_retains_the_previous_content_baseline() {
        let mut tracker = ContentTracker::new(Some(b"valid = true".to_vec()));

        assert!(tracker.update(Ok(b"invalid = [".to_vec())));
        assert!(!tracker.update(Ok(b"invalid = [".to_vec())));
    }
}
