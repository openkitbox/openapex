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
