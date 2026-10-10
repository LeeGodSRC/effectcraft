//! Leaving in one piece when the window's graphics device is lost.
//!
//! A lost device (the driver reset the GPU, ran out of video memory, was updated…) cannot be
//! brought back under eframe: it has no way to recreate its device, and its renderer panics on
//! the next paint (`egui-wgpu`: "Failed to create staging buffer"). That panic unwinds through a
//! surface texture still in use, a destructor panics in turn, and the process aborts with the
//! document. So the app leaves before that paint instead: the frontend writes a recovery copy
//! of unsaved work ([`RecoverySlot`]) and calls [`Leave::now`], which starts the app again on
//! the same project (once: a relaunched app that loses its device again says so and exits) and
//! ends this process without unwinding. The next launch offers the recovery copy through the
//! usual crash-recovery dialog and keeps the GPU compositor off, since compositing may be what
//! exhausted the device.
//!
//! A device lost in the middle of a frame can reach the paint before the frontend notices: the
//! panic hook ([`Leave::install_panic_hook`]) then does the same from the panicking paint.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use effectcraft_ui_egui::gpu_failure::{GpuFailureBridge, RecoverySlot};

/// Set for the app started after a lost device: it does not restart itself a second time.
pub const RELAUNCHED: &str = "EFFECTCRAFT_GPU_LOST";
/// The recovery copy the lost session wrote, for the relaunched app's notice.
pub const RECOVERY_COPY: &str = "EFFECTCRAFT_GPU_LOST_COPY";
/// Test hook: lose the window's device this many seconds after start-up (by destroying it,
/// which is what a driver reset looks like to wgpu, without touching the GPU's other users).
pub const SIMULATE: &str = "EFFECTCRAFT_SIMULATE_DEVICE_LOSS";

/// What to do about a lost device.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    /// Start the app again with these arguments.
    Relaunch(Vec<String>),
    /// This run already is the relaunch: say so and exit.
    Exit,
}

/// `relaunched`: this process was itself started after a lost device.
pub fn plan(relaunched: bool, control_port: Option<u16>, project: Option<&str>) -> Plan {
    if relaunched {
        return Plan::Exit;
    }
    let mut args = Vec::new();
    if let Some(port) = control_port {
        args.extend(["--control".to_string(), port.to_string()]);
    }
    match project {
        Some(p) => args.push(p.to_string()),
        // An untitled project: no Home screen over the recovery dialog.
        None => args.push("--empty".to_string()),
    }
    Plan::Relaunch(args)
}

/// The relaunched app's status line.
pub fn notice(copy: Option<&str>) -> String {
    let saved = match copy {
        Some(c) if !c.is_empty() => format!(" Unsaved changes were saved to {c}."),
        _ => String::new(),
    };
    format!("EffectCraft restarted because the graphics device was lost.{saved} GPU compositing is off for this session (frames render on the CPU).")
}

/// The relaunched app's notice, when this process is one.
pub fn notice_from_env() -> Option<String> {
    std::env::var_os(RELAUNCHED)?;
    Some(notice(std::env::var(RECOVERY_COPY).ok().as_deref()))
}

/// Leaves the process after a lost device.
pub struct Leave {
    slot: RecoverySlot,
    bridge: GpuFailureBridge,
    control_port: Option<u16>,
    leaving: AtomicBool,
}

impl Leave {
    pub fn new(slot: RecoverySlot, bridge: GpuFailureBridge, control_port: Option<u16>) -> Arc<Leave> {
        Arc::new(Leave { slot, bridge, control_port, leaving: AtomicBool::new(false) })
    }

    /// Restart (or explain), then end the process without unwinding into eframe's paint.
    /// `recovery`: the recovery copy's path, or why there is none.
    pub fn now(&self, reason: &str, project: Option<&str>, recovery: Option<&Result<String, String>>) -> ! {
        if self.leaving.swap(true, Ordering::AcqRel) {
            std::process::exit(1);
        }
        let copy = recovery.and_then(|r| r.as_ref().ok());
        let saved = match recovery {
            Some(Ok(path)) => format!("Unsaved changes were saved to {path}."),
            Some(Err(e)) => format!("Unsaved changes could not be saved: {e}."),
            None => "The project had no unsaved changes.".to_string(),
        };
        log::error!("the graphics device was lost ({reason}). {saved}");
        match plan(std::env::var_os(RELAUNCHED).is_some(), self.control_port, project) {
            Plan::Relaunch(args) => {
                let started = std::env::current_exe().and_then(|exe| {
                    let mut c = std::process::Command::new(exe);
                    c.args(&args).env(RELAUNCHED, "1").env_remove(SIMULATE);
                    match copy {
                        Some(path) => c.env(RECOVERY_COPY, path),
                        None => c.env_remove(RECOVERY_COPY),
                    };
                    c.spawn()
                });
                match started {
                    Ok(_) => {
                        log::error!("restarting EffectCraft");
                        eprintln!("effectcraft: the graphics device was lost. {saved} Restarting.");
                        std::process::exit(0);
                    }
                    Err(e) => self.explain(&format!("The graphics device was lost and EffectCraft could not restart itself ({e}). {saved}")),
                }
            }
            Plan::Exit => self.explain(&format!("The graphics device was lost again, so EffectCraft has to close. {saved}")),
        }
        std::process::exit(1);
    }

    /// Say why the app closes: the log and stderr, and a dialog unless an agent drives the app.
    fn explain(&self, message: &str) {
        log::error!("{message}");
        eprintln!("effectcraft: {message}");
        if self.control_port.is_none() {
            let _ = rfd::MessageDialog::new()
                .set_level(rfd::MessageLevel::Error)
                .set_title("EffectCraft")
                .set_description(message)
                .set_buttons(rfd::MessageButtons::Ok)
                .show();
        }
    }

    /// A panic on the UI thread once the device is lost, or inside egui's renderer, is the
    /// paint going through a lost device: write the recovery copy and leave from the hook,
    /// before the unwinding that aborts the process. Other panics go on as before (the
    /// previous hook has logged them; commands and frame renders catch theirs).
    pub fn install_panic_hook(self: &Arc<Self>) {
        let me = self.clone();
        let ui_thread = std::thread::current().id();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            if std::thread::current().id() != ui_thread {
                return;
            }
            let lost = me.bridge.failure().filter(|f| f.presentation_lost);
            let in_renderer = info.location().is_some_and(|l| l.file().contains("egui-wgpu"));
            if lost.is_none() && !in_renderer {
                return;
            }
            let reason = lost.map(|f| f.reason).unwrap_or_else(|| "the window's renderer failed".to_string());
            let project = me.slot.project_path();
            let recovery = me.slot.write();
            me.now(&reason, project.as_deref(), recovery.as_ref());
        }));
    }
}

/// [`SIMULATE`]: destroy the device after the given number of seconds.
pub fn simulate_from_env(device: &eframe::wgpu::Device, ctx: &egui::Context) {
    let Some(seconds) = std::env::var(SIMULATE).ok().and_then(|s| s.parse::<f64>().ok()).filter(|s| s.is_finite() && *s >= 0.0) else { return };
    let (device, ctx) = (device.clone(), ctx.clone());
    let spawned = std::thread::Builder::new().name("simulated-device-loss".into()).spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds.min(3600.0)));
        log::warn!("{SIMULATE}: destroying the window's graphics device");
        device.destroy();
        ctx.request_repaint();
    });
    if let Err(e) = spawned {
        log::warn!("{SIMULATE}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lost device restarts the app once on the same project and control port; the
    /// restarted app does not restart again (a device lost on every launch would loop).
    #[test]
    fn a_lost_device_restarts_the_app_once() {
        assert_eq!(plan(false, None, Some("/p/Film.ecproj")), Plan::Relaunch(vec!["/p/Film.ecproj".into()]));
        assert_eq!(plan(false, Some(9877), Some("a.ecproj")), Plan::Relaunch(vec!["--control".into(), "9877".into(), "a.ecproj".into()]));
        assert_eq!(plan(false, None, None), Plan::Relaunch(vec!["--empty".into()]));
        assert_eq!(plan(true, Some(9877), Some("a.ecproj")), Plan::Exit);
        // The arguments parse back to the same session.
        let Plan::Relaunch(args) = plan(false, Some(9877), Some("a.ecproj")) else { return };
        let parsed = crate::parse_args(args, None).unwrap_or_default();
        assert_eq!((parsed.control_port, parsed.files), (Some(9877), vec!["a.ecproj".to_string()]));
    }

    #[test]
    fn the_restarted_app_says_what_happened() {
        let n = notice(Some("/p/EffectCraft Auto-Save/Film auto-save 1.ecproj"));
        assert!(n.contains("graphics device was lost") && n.contains("Film auto-save 1.ecproj") && n.contains("GPU compositing is off"), "{n}");
        assert!(!notice(None).contains("saved to"));
    }
}
