//! The watch loop's core: filesystem events fold into one attributed
//! snapshot after quiet time; internals never trigger; stop is clean.

use std::fs;
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError, channel};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use atelier_core::{Act, ActorKind, DeltaKind, WatchEvent, WatchStop, Workspace};

/// The acceptance bound: an external edit becomes a snapshot within this.
const BOUND: Duration = Duration::from_secs(5);
const DEBOUNCE: Duration = Duration::from_millis(100);

/// Serialize tests: they all set the process-wide `ATELIER_CONFIG_HOME`.
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: LazyLock<Mutex<()>> = LazyLock::new(Mutex::default);
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[expect(unsafe_code, reason = "set_var wires the workspace to the test config")]
fn set_actor(config_home: &Path) {
    fs::create_dir_all(config_home).expect("create config home");
    fs::write(
        config_home.join("config.toml"),
        "[actor]\nname = \"test-actor\"\nkind = \"human\"\n",
    )
    .expect("write actor config");
    // SAFETY: every test holds `env_lock()` for its whole body, so no other
    // thread reads or writes the environment concurrently.
    unsafe {
        std::env::set_var("ATELIER_CONFIG_HOME", config_home);
    }
}

/// A watch loop running on its own thread; stopping it hands the
/// workspace back through `join`.
struct RunningWatch {
    stop: WatchStop,
    events: Receiver<WatchEvent>,
    handle: JoinHandle<Workspace>,
}

impl RunningWatch {
    /// Start the loop and consume its `Started` event: from here on,
    /// external edits raise events.
    fn start(mut workspace: Workspace) -> Self {
        let stop = WatchStop::new();
        let loop_stop = stop.clone();
        let (tx, events) = channel();
        let handle = std::thread::spawn(move || {
            workspace
                .watch(
                    DEBOUNCE,
                    |event| {
                        // The receiver may be gone once the test has what
                        // it needs; the loop's job is unaffected.
                        let _ = tx.send(event.clone());
                    },
                    &loop_stop,
                )
                .expect("watch loop runs until stopped");
            workspace
        });
        let running = Self {
            stop,
            events,
            handle,
        };
        assert_eq!(running.next_event(), WatchEvent::Started);
        running
    }

    fn next_event(&self) -> WatchEvent {
        self.events
            .recv_timeout(BOUND)
            .expect("a watch event within the bound")
    }

    fn assert_quiet(&self, wait: Duration) {
        let event = self.events.recv_timeout(wait);
        assert!(
            matches!(event, Err(RecvTimeoutError::Timeout)),
            "expected no watch event, got {event:?}"
        );
    }

    fn stop(self) -> (Workspace, Receiver<WatchEvent>) {
        self.stop.stop();
        let workspace = self.handle.join().expect("watch thread joins");
        (workspace, self.events)
    }
}

fn snapshotted(event: &WatchEvent) -> String {
    let snapshot = match event {
        WatchEvent::Snapshotted { snapshot } => Some(snapshot.clone()),
        WatchEvent::Started => None,
    };
    snapshot.expect("a snapshot event, not Started")
}

fn snapshot_acts(workspace: &mut Workspace) -> Vec<(String, ActorKind, Option<String>)> {
    workspace
        .journal(100)
        .expect("read the journal")
        .into_iter()
        .filter(|entry| entry.act == Act::Snapshot)
        .map(|entry| (entry.actor_name, entry.actor_kind, entry.reference))
        .collect()
}

#[test]
fn an_external_edit_becomes_an_attributed_snapshot_within_the_bound() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(root.path()).unwrap();

    let watch = RunningWatch::start(workspace);
    fs::write(root.path().join("notes.txt"), "hello\n").unwrap();
    let snapshot = snapshotted(&watch.next_event());
    let (mut workspace, _) = watch.stop();

    assert_eq!(
        snapshot_acts(&mut workspace),
        vec![(
            "test-actor".to_owned(),
            ActorKind::Human,
            Some(snapshot.clone()),
        )],
    );
}

#[test]
fn a_stopped_watcher_takes_no_snapshots() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(root.path()).unwrap();

    let watch = RunningWatch::start(workspace);
    fs::write(root.path().join("notes.txt"), "one\n").unwrap();
    let first = snapshotted(&watch.next_event());
    let (workspace, events) = watch.stop();

    fs::write(root.path().join("notes.txt"), "one\ntwo\n").unwrap();
    std::thread::sleep(DEBOUNCE * 3);
    assert_eq!(events.try_recv(), Err(TryRecvError::Disconnected));

    // The one observer of "nothing snapshotted while stopped" that does
    // not itself snapshot: a fresh watcher's catch-up scan still finds the
    // edit outstanding.
    let watch = RunningWatch::start(workspace);
    let second = snapshotted(&watch.next_event());
    let (mut workspace, _) = watch.stop();

    assert_ne!(first, second);
    assert_eq!(
        snapshot_acts(&mut workspace),
        vec![
            (
                "test-actor".to_owned(),
                ActorKind::Human,
                Some(second.clone()),
            ),
            (
                "test-actor".to_owned(),
                ActorKind::Human,
                Some(first.clone()),
            ),
        ],
    );
}

#[test]
fn a_restarted_watcher_catches_up_edits_made_while_stopped() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let mut workspace = Workspace::init(root.path()).unwrap();

    fs::write(root.path().join("notes.txt"), "one\n").unwrap();
    workspace.journal(1).unwrap();

    fs::write(root.path().join("notes.txt"), "one\ntwo\n").unwrap();
    let watch = RunningWatch::start(workspace);
    let snapshot = snapshotted(&watch.next_event());
    let (mut workspace, _) = watch.stop();

    let acts = snapshot_acts(&mut workspace);
    assert_eq!(
        acts.first(),
        Some(&(
            "test-actor".to_owned(),
            ActorKind::Human,
            Some(snapshot.clone()),
        )),
    );
    let diff = workspace.diff_latest().unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].address.as_str(), "notes.txt");
    assert_eq!(diff.deltas[0].kind, DeltaKind::Changed);
}

#[test]
fn engine_internals_never_become_snapshotted_content() {
    let _guard = env_lock();
    let config = tempfile::tempdir().unwrap();
    set_actor(config.path());
    let root = tempfile::tempdir().unwrap();
    let workspace = Workspace::init(root.path()).unwrap();

    let watch = RunningWatch::start(workspace);
    fs::write(root.path().join(".git").join("probe.txt"), "x").unwrap();
    fs::write(root.path().join(".jj").join("probe.txt"), "x").unwrap();
    fs::write(root.path().join(".atelier").join("probe.txt"), "x").unwrap();
    watch.assert_quiet(DEBOUNCE * 5);

    fs::write(root.path().join("notes.txt"), "content\n").unwrap();
    snapshotted(&watch.next_event());
    let (mut workspace, _) = watch.stop();

    let diff = workspace.diff_latest().unwrap();
    assert_eq!(diff.deltas.len(), 1);
    assert_eq!(diff.deltas[0].address.as_str(), "notes.txt");
    assert_eq!(diff.deltas[0].kind, DeltaKind::Added);
}
