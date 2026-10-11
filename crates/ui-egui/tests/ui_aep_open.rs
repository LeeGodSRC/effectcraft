//! Opening an original synthetic AEP through the UI command path (no private fixtures).
#[allow(dead_code)]
#[path = "../../engine/tests/common/aep.rs"]
mod fixture;
use effectcraft_engine::Session;
use effectcraft_ui_egui::{Dialog, EffectcraftApp};
use egui::vec2;
use egui_kittest::Harness;
use serde_json::json;

#[test]
fn opens_and_displays_after_effects_project_in_ui() {
    let path = std::env::temp_dir().join(format!("effectcraft-ui-aep-{}.aep", std::process::id()));
    std::fs::write(&path, fixture::project()).unwrap();
    let mut session = Session::new();
    session.check_footage_on_open = false;
    let mut h = Harness::builder().with_size(vec2(1600.0, 1000.0)).build_eframe(|_| EffectcraftApp::new(session));
    let ctx = h.ctx.clone();
    let result = effectcraft_ui_egui::menus::invoke(h.state_mut(), &ctx, "file.open", json!({"path": path})).unwrap();
    assert_eq!(result["type"], "aep");
    h.run_steps(4);
    assert_eq!(h.state().session.project.comps().count(), 2);
    assert!(h.state().session.path.is_none());
    assert_eq!(h.state().dialog, Some(Dialog::AepReport));
    assert!(h.state().auto.elements.iter().any(|e| e.id == "dialog.aepReport.ok"));
    std::fs::remove_file(path).unwrap();
}
