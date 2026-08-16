use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use notify::{Event, EventKind};

use crate::error::Error;
use crate::workspace::SKIP_NAMES;

/// How often a blocked watch loop wakes to check its stop handle. Waking
/// is a condition check, never a filesystem scan — edits arrive as events.
pub(crate) const STOP_TICK: Duration = Duration::from_millis(100);

/// What a running watch loop reports as it works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// The watcher is armed: edits from now on raise events. The catch-up
    /// scan for edits made while no watcher ran follows immediately.
    Started,
    /// Outstanding edits became this snapshot, journaled like any snapshot.
    Snapshotted { snapshot: String },
}

/// Stops a running watch loop from another thread; the loop returns within
/// its tick. Outstanding edits stay for the next watcher's catch-up scan.
#[derive(Debug, Clone, Default)]
pub struct WatchStop(Arc<AtomicBool>);

impl WatchStop {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask the loop to return.
    pub fn stop(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn stopped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// What the notify callback forwards to the watch loop: a content pulse,
/// or the watcher's own failure — loud, never swallowed.
pub(crate) type Pulse = Result<(), notify::Error>;

/// Drain the event storm until it stays quiet for `debounce`, then let the
/// caller snapshot once. A stop request wins over the storm.
pub(crate) fn settle(
    pulses: &Receiver<Pulse>,
    debounce: Duration,
    stop: &WatchStop,
) -> Result<(), Error> {
    loop {
        if stop.stopped() {
            return Ok(());
        }
        match pulses.recv_timeout(debounce) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(watcher_failed(&error)),
            Err(RecvTimeoutError::Timeout) => return Ok(()),
            Err(RecvTimeoutError::Disconnected) => return Err(watcher_gone()),
        }
    }
}

pub(crate) fn watcher_failed(error: &notify::Error) -> Error {
    Error::Engine(format!("the file watcher failed: {error}"))
}

pub(crate) fn watcher_gone() -> Error {
    Error::Engine("the file watcher stopped delivering events".to_owned())
}

/// An fs event is content when any of its paths lands outside the engine
/// internals (`.atelier`, `.jj`, `.git`). Events wholly inside them are the
/// engine's and journal's own writes — reacting to those would loop.
pub(crate) fn event_is_content(root: &Path, event: &Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    // A pathless event (a rescan hint) may cover content; a snapshot of an
    // unchanged tree is a no-op, so erring toward content is safe.
    if event.paths.is_empty() {
        return true;
    }
    event.paths.iter().any(|path| is_content_path(root, path))
}

fn is_content_path(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    match relative.components().next() {
        Some(Component::Normal(name)) => !SKIP_NAMES.iter().any(|skip| name == *skip),
        _ => true,
    }
}
