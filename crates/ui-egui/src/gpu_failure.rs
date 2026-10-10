//! Host-owned shared-device failure forwarding. Device handlers belong to the executable or
//! embedding host, never to a library borrowing its device. This bridge does no device work,
//! document mutation, file I/O or renderer locking inside a notification.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use effectcraft_engine::Session;
use effectcraft_engine::autosave::{self, Sentinel};
use effectcraft_engine::config::{ConfigStore, StdFiles};
use effectcraft_engine::prefs::Prefs;
use effectcraft_engine::project::Project;

type Notifier = Arc<dyn Fn(String) + Send + Sync>;

/// The share of the driver's memory budget (percent) above which the window's device refuses
/// new textures and buffers with an error, for the host's instance descriptor.
pub const MEMORY_BUDGET_PERCENT: u8 = effectcraft_gpu::MEMORY_BUDGET_PERCENT;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GpuFailure {
    pub reason: String,
    /// Device loss requires restarting/recreating the presentation backend too. CPU
    /// compositing alone cannot make a lost eframe presentation device usable again.
    pub presentation_lost: bool,
}

#[derive(Default)]
struct State {
    failure: Option<GpuFailure>,
    notifier: Option<Notifier>,
}

/// One bridge per host device. Failure is latched; duplicate reports cannot grow a queue.
#[derive(Clone)]
pub struct GpuFailureBridge {
    state: Arc<Mutex<State>>,
    ctx: egui::Context,
}

impl GpuFailureBridge {
    pub fn new(ctx: &egui::Context) -> Self {
        Self { state: Arc::default(), ctx: ctx.clone() }
    }

    pub fn failure(&self) -> Option<GpuFailure> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).failure.clone()
    }

    /// Forward to a compositor's weak notifier once construction succeeds. A fault that
    /// preceded registration is replayed; the compositor must not then be attached to the UI.
    pub fn forward_to(&self, notifier: impl Fn(String) + Send + Sync + 'static) {
        let notifier: Notifier = Arc::new(notifier);
        let failure = {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            state.notifier = Some(notifier.clone());
            state.failure.clone()
        };
        if let Some(failure) = failure {
            notify(&notifier, failure.reason);
        }
    }

    /// Called by the device owner for an uncaptured error or device loss. A later device-loss
    /// notification escalates the recovery message without completing pending work twice.
    pub fn report(&self, reason: &str, presentation_lost: bool) -> bool {
        let (notification, changed) = {
            let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(failure) = &mut state.failure {
                let changed = presentation_lost && !failure.presentation_lost;
                failure.presentation_lost |= presentation_lost;
                (None, changed)
            } else {
                let reason: String = reason.chars().take(2048).collect();
                state.failure = Some(GpuFailure { reason: reason.clone(), presentation_lost });
                (state.notifier.clone().map(|notifier| (notifier, reason)), true)
            }
        };
        if let Some((notifier, reason)) = notification {
            notify(&notifier, reason);
        }
        if changed {
            self.ctx.request_repaint();
        }
        changed
    }
}

/// What the host is told when the window can no longer draw
/// ([`crate::Hooks::presentation_lost`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentationLost {
    pub reason: String,
    /// The open project's file (None: untitled).
    pub project: Option<String>,
    /// The recovery copy written for it: its path, or why it could not be written. `None`
    /// when the project had no unsaved changes.
    pub recovery: Option<Result<String, String>>,
}

/// The document as of the last frame, for a recovery copy written when the window's device is
/// lost. eframe cannot recreate a lost device and its renderer panics on the next paint, so the
/// copy may have to be written from wherever the loss is noticed: the frame's logic, or the
/// panic hook when the loss lands in the middle of a paint. The UI thread refreshes the
/// snapshot (an `Arc` clone of the project); [`RecoverySlot::write`] works from any thread.
#[derive(Clone, Default)]
pub struct RecoverySlot {
    doc: Arc<Mutex<Option<RecoveryDoc>>>,
    written: Arc<AtomicBool>,
}

struct RecoveryDoc {
    /// (project revision, settings revision) the snapshot was taken at.
    key: (u64, u64),
    project: Arc<Project>,
    path: Option<String>,
    dirty: bool,
    prefs: Prefs,
    root: Option<PathBuf>,
    config: Option<Arc<dyn ConfigStore>>,
    last_slot: Option<u32>,
    /// The session owns the crash-recovery sentinel (not agent-driven runs).
    sentinel: bool,
}

impl RecoverySlot {
    /// Snapshot the session's document (cheap; a no-op while nothing changed).
    pub fn update(&self, session: &Session) {
        let key = (session.revision, session.prefs_revision);
        let mut doc = self.doc.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if doc.as_ref().is_some_and(|d| d.key == key && d.path == session.path && d.dirty == session.is_dirty()) {
            return;
        }
        *doc = Some(RecoveryDoc {
            key,
            project: session.project.clone(),
            path: session.path.clone(),
            dirty: session.is_dirty(),
            prefs: session.autosave_prefs(),
            root: session.default_autosave_root(),
            config: session.config.clone(),
            last_slot: session.autosave.last_slot,
            sentinel: session.autosave.sentinel,
        });
    }

    /// The snapshot's project file.
    pub fn project_path(&self) -> Option<String> {
        self.doc.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref().and_then(|d| d.path.clone())
    }

    /// Write the recovery copy, once: an auto-save (`<name> auto-save N.ecproj`, written
    /// atomically) that the crash-recovery sentinel points the next launch at. `None` when
    /// there is nothing to write: no unsaved changes (the sentinel is then removed, the next
    /// launch has nothing to recover), no snapshot, or already written.
    pub fn write(&self) -> Option<Result<String, String>> {
        if self.written.swap(true, Ordering::AcqRel) {
            return None;
        }
        let doc = self.doc.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take()?;
        if !doc.dirty {
            if let (true, Some(c)) = (doc.sentinel, &doc.config) {
                autosave::end(c.as_ref());
            }
            return None;
        }
        let write = || -> std::io::Result<String> {
            let fs = match &doc.config {
                Some(c) => c.files(),
                None => &StdFiles,
            };
            let plan = autosave::plan_in(fs, &doc.prefs, doc.path.as_deref(), doc.root.as_deref(), doc.last_slot)?;
            plan.write(fs, doc.project.to_json_compact().as_bytes())?;
            Ok(plan.path.to_string_lossy().into_owned())
        };
        let result = write().map_err(|e| format!("the recovery copy could not be written: {e}"));
        if let (Ok(path), true, Some(c)) = (&result, doc.sentinel, &doc.config) {
            autosave::update(c.as_ref(), &Sentinel { pid: autosave::pid(), project: doc.path.clone(), autosave: Some(path.clone()), dirty: true });
        }
        Some(result)
    }
}

fn notify(notifier: &Notifier, reason: String) {
    // A host callback must never propagate a consumer panic into wgpu's error handling.
    #[cfg(not(target_arch = "wasm32"))]
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| notifier(reason))).is_err() {
        log::error!("GPU failure consumer panicked");
    }
    #[cfg(target_arch = "wasm32")]
    notifier(reason);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_construction_failure_is_replayed_after_notifier_registration() {
        let bridge = GpuFailureBridge::new(&egui::Context::default());
        bridge.report("pipeline allocation failed", false);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let copy = observed.clone();
        let probe = Arc::downgrade(&bridge.state);
        bridge.forward_to(move |reason| {
            assert!(probe.upgrade().unwrap().try_lock().is_ok(), "consumer runs outside bridge lock");
            copy.lock().unwrap().push(reason);
        });
        assert_eq!(*observed.lock().unwrap(), ["pipeline allocation failed"]);
        bridge.report("duplicate", false);
        assert_eq!(observed.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_late_failure_notifies_once_and_later_device_loss_escalates() {
        let bridge = GpuFailureBridge::new(&egui::Context::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let copy = calls.clone();
        bridge.forward_to(move |_| {
            copy.fetch_add(1, Ordering::SeqCst);
        });
        assert!(bridge.failure().is_none());
        bridge.report("uncaptured allocation failure", false);
        bridge.report("device lost", true);
        bridge.report("another error", false);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let failure = bridge.failure().unwrap();
        assert!(failure.presentation_lost);
        assert_eq!(failure.reason, "uncaptured allocation failure");
    }

    #[test]
    fn diagnostics_are_bounded_and_native_consumer_panics_are_isolated() {
        let bridge = GpuFailureBridge::new(&egui::Context::default());
        #[cfg(not(target_arch = "wasm32"))]
        bridge.forward_to(|_| panic!("synthetic consumer failure"));
        bridge.report(&"é".repeat(3000), false);
        assert_eq!(bridge.failure().unwrap().reason.chars().count(), 2048);
        bridge.report("device lost after consumer failure", true);
        assert!(bridge.failure().unwrap().presentation_lost);
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ec-device-loss-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The window's device is lost with unsaved work: the recovery copy is written (from
    /// whichever thread notices, here not the UI's), once, and the next launch offers it. The
    /// app used to abort inside eframe's paint and the work was gone.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_lost_device_leaves_a_recovery_copy_the_next_launch_offers() {
        use effectcraft_engine::config::DirConfig;
        use serde_json::json;
        let d = temp_dir("copy");
        let store: Arc<dyn ConfigStore> = Arc::new(DirConfig::new(d.join("config")));
        let mut s = Session { config: Some(store.clone()), ..Default::default() };
        assert!(s.begin_recovery().is_none());
        s.execute("comp.new", json!({"name": "Work", "width": 8, "height": 8, "duration": 1})).unwrap();
        let project = d.join("Film.ecproj");
        s.execute("file.saveAs", json!({"path": project.to_string_lossy()})).unwrap();
        s.execute("layer.newNull", json!({"name": "unsaved work"})).unwrap();
        let slot = RecoverySlot::default();
        slot.update(&s);
        assert_eq!(slot.project_path().as_deref(), Some(project.to_string_lossy().as_ref()));
        let writer = slot.clone();
        let copy = std::thread::spawn(move || writer.write()).join().unwrap().expect("unsaved changes").expect("written");
        assert!(copy.ends_with("Film auto-save 1.ecproj"), "{copy}");
        assert!(slot.write().is_none(), "written once");
        // The process ends without a clean exit; the next launch finds the copy.
        drop(s);
        let mut next = Session { config: Some(store), ..Default::default() };
        let offered = next.begin_recovery().expect("the lost session is noticed");
        assert_eq!(offered.autosave.as_deref(), Some(copy.as_str()));
        assert!(offered.dirty);
        next.execute("file.open", json!({"path": copy})).unwrap();
        let has = |name: &str| next.project.comps().any(|(_, c)| c.layers.iter().any(|l| l.name == name));
        assert!(has("unsaved work"), "the copy holds the unsaved layer");
        let _ = std::fs::remove_dir_all(d);
    }

    /// Nothing unsaved: no copy, and the next launch has nothing to recover.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_lost_device_with_nothing_unsaved_writes_nothing() {
        use effectcraft_engine::config::DirConfig;
        use serde_json::json;
        let d = temp_dir("clean");
        let store: Arc<dyn ConfigStore> = Arc::new(DirConfig::new(d.join("config")));
        let mut s = Session { config: Some(store.clone()), ..Default::default() };
        assert!(s.begin_recovery().is_none());
        s.execute("comp.new", json!({"name": "Work", "width": 8, "height": 8, "duration": 1})).unwrap();
        s.execute("file.saveAs", json!({"path": d.join("Film.ecproj").to_string_lossy()})).unwrap();
        let slot = RecoverySlot::default();
        assert!(slot.clone().write().is_none(), "no snapshot yet");
        let slot = RecoverySlot::default();
        slot.update(&s);
        assert!(slot.write().is_none());
        assert!(!d.join("EffectCraft Auto-Save").exists());
        drop(s);
        assert!(Session { config: Some(store), ..Default::default() }.begin_recovery().is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn concurrent_registration_and_failure_deliver_exactly_once() {
        let bridge = GpuFailureBridge::new(&egui::Context::default());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let calls = Arc::new(AtomicUsize::new(0));
        let reporter = bridge.clone();
        let ready = barrier.clone();
        let thread = std::thread::spawn(move || {
            ready.wait();
            reporter.report("device failed during subscription", false);
        });
        let count = calls.clone();
        barrier.wait();
        bridge.forward_to(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        });
        thread.join().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
