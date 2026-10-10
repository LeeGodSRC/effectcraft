#[path = "common/aep.rs"]
mod fixture;
use effectcraft_engine::{Session, aep};
use effectcraft_project::{ItemKind, LayerSource};
use serde_json::json;

fn temp(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("effectcraft-aep-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn aep_decodes_sources_solid_transform_and_rational_time() {
    let (p, report) = aep::from_aep_or_aepx_with_report(&fixture::project(), None).unwrap();
    assert_eq!(report.comp_count, 2);
    let main = p.items.values().find(|i| i.name == "Main").unwrap().as_comp().unwrap();
    assert_eq!((main.width, main.height), (64, 48));
    assert_eq!(main.duration.seconds(), 10.0);
    assert_eq!(main.frame_rate.as_f64(), 24.0);
    assert_eq!(main.layers.len(), 1, "editor views and marker streams are not layers");
    let layer = &main.layers[0];
    assert_eq!(layer.start_time.seconds(), 0.5);
    assert_eq!(layer.in_point.seconds(), 1.5);
    assert_eq!(layer.out_point.seconds(), 10.5);
    assert_eq!(layer.props.prop("transform/position").unwrap().value, effectcraft_engine::keyframe::Value::Vec3([23.0, 17.0, 0.0]));
    let LayerSource::Comp { item } = layer.source else { panic!("expected precomp") };
    let nested = p.comp(item).unwrap();
    let LayerSource::Solid { item } = nested.layers[0].source else { panic!("expected solid") };
    let ItemKind::Solid(solid) = &p.item(item).unwrap().kind else { panic!("expected solid item") };
    assert_eq!(solid.color, [0.8, 0.2, 0.1]);
    assert_eq!(report.missing_footage_count, 0);
    assert!(report.has_issues());
}

#[test]
fn aep_open_never_overwrites_source_and_save_round_trips() {
    let dir = temp("save");
    let path = dir.join("source.aep");
    let bytes = fixture::project();
    std::fs::write(&path, &bytes).unwrap();
    let mut s = Session::new();
    s.check_footage_on_open = false;
    s.execute("file.open", json!({"path": path})).unwrap();
    assert!(s.path.is_none());
    assert!(s.is_dirty());
    assert!(s.execute("file.save", json!({})).is_err());
    for command in ["file.saveAs", "file.saveCopy", "file.saveCopyAsXml"] {
        assert!(s.execute(command, json!({"path":path})).is_err(), "{command} must preserve the original AEP");
    }
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    let saved = dir.join("saved.ecproj");
    s.execute("file.saveAs", json!({"path":saved})).unwrap();
    s.execute("file.open", json!({"path":saved})).unwrap();
    assert_eq!(s.project.comps().count(), 2);
    assert!(s.last_aep_report.is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn aep_import_remaps_all_ids_and_undo_restores_existing_project() {
    let dir = temp("import");
    let path = dir.join("source.aep");
    std::fs::write(&path, fixture::project()).unwrap();
    let mut s = Session::new();
    s.execute("comp.new", json!({"name":"Existing"})).unwrap();
    s.execute("layer.newSolid", json!({"name":"Existing solid"})).unwrap();
    let original = s.project.to_file_json().unwrap();
    let first_free = s.project.next_id;
    let result = s.execute("file.import", json!({"path":path})).unwrap();
    assert!(result["errors"].as_array().unwrap().is_empty(), "{result}");
    assert_eq!(s.project.comps().count(), 3);
    for id in result["comps"].as_array().unwrap() {
        let comp = s.project.comp(effectcraft_project::ItemId(id.as_u64().unwrap())).unwrap();
        for layer in &comp.layers {
            assert!(layer.id.0 >= first_free);
            assert!(layer.props.uid >= first_free);
            let target = layer.source.item().unwrap();
            assert!(target.0 >= first_free);
            assert!(s.project.item(target).is_some());
        }
    }
    s.execute("edit.undo", json!({})).unwrap();
    assert_eq!(s.project.to_file_json().unwrap(), original);
    s.execute("edit.redo", json!({})).unwrap();
    assert_eq!(s.project.comps().count(), 3);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn aep_rejects_truncation_deep_trees_cycles_and_fake_xml() {
    let bytes = fixture::project();
    for end in 0..bytes.len() {
        assert!(aep::from_aep(&bytes[..end]).is_err(), "accepted truncation {end}");
    }
    let mut deep = fixture::chunk(b"Utf8", b"x");
    for _ in 0..66 {
        deep = fixture::list(b"Fold", &[deep]);
    }
    assert!(aep::parse_rifx(&fixture::container(&[deep])).unwrap_err().contains("nesting"));
    let cycle = fixture::container(&[fixture::comp(1, "A", 2), fixture::comp(2, "B", 1)]);
    assert!(aep::from_aep(&cycle).unwrap_err().contains("cyclic"));
    assert!(aep::from_aepx(b"<AfterEffects/>").is_err());
}

#[test]
fn aep_text_blob_is_opaque_not_a_malformed_subtree() {
    let bytes = fixture::container(&[fixture::list(b"btdk", &[b" /98 text document bytes".to_vec()]), fixture::comp(1, "Text", 0)]);
    assert_eq!(aep::from_aep(&bytes).unwrap().comps().count(), 1);
}

#[test]
fn aep_font_report_uses_character_runs_instead_of_unused_editing_defaults() {
    let blob = br"/0 << /1 << /0 [
        << /0 << /0 << /0 (Unused-Editing-Default) >> >> >>
        << /0 << /0 << /0 (Inter-Bold) >> >> >>
        << /0 << /0 << /0 (Original-Test-Font) >> >> >>
    ] >> >> /1 << /1 [ << /0 << /0 (Ab\r)
        /6 << /0 [
            << /0 << /0 << /6 << /0 1 /1 40 /12 2 >> >> >> /1 1 >>
            << /0 << /0 << /6 << /0 2 /1 20 /12 1 >> >> >> /1 2 >>
        ] >> >> >> ] >>";
    let (p, report) = aep::from_aep_with_report(&fixture::text_project(blob), None).unwrap();
    assert_eq!(report.fonts, ["Inter-Bold", "Original-Test-Font"]);
    let l = &p.comps().next().unwrap().1.layers[0];
    assert!(matches!(l.source, LayerSource::Text));
    let effectcraft_engine::keyframe::Value::Text(doc) = &l.props.prop("text/sourceText").unwrap().value else { panic!("source text") };
    assert_eq!(doc.text, "Ab");
    assert!(doc.all_caps);
    assert!(doc.runs[1].style.small_caps);
}

#[test]
fn aep_accepts_large_property_streams_within_the_byte_budget() {
    let empty = fixture::chunk(b"JUNK", b"");
    let payload = empty.repeat(350_000);
    let bytes = fixture::container(&[payload]);
    assert_eq!(aep::parse_rifx(&bytes).unwrap().len(), 350_000);
    let excessive = fixture::container(&[empty.repeat(1_000_001)]);
    assert!(aep::parse_rifx(&excessive).unwrap_err().contains("too many chunks"));
}

#[test]
fn aep_duplicate_names_do_not_drop_compositions() {
    let bytes = fixture::container(&[fixture::comp(1, "Same", 0), fixture::comp(2, "Same", 0)]);
    assert_eq!(aep::from_aep(&bytes).unwrap().comps().count(), 2);
}

#[test]
fn aep_orphan_matte_is_reported_and_switches_are_preserved() {
    let mut bytes = fixture::project();
    let at = bytes.windows(4).position(|b| b == b"ldta").unwrap() + 8;
    bytes[at + 36..at + 40].copy_from_slice(&0x10486u32.to_be_bytes());
    bytes[at + 104..at + 108].copy_from_slice(&1u32.to_be_bytes());
    let (p, report) = aep::from_aep_or_aepx_with_report(&bytes, None).unwrap();
    let comp = p.items.values().find(|i| i.name == "Main").unwrap().as_comp().unwrap();
    let layer = &comp.layers[0];
    assert!(!layer.switches.video);
    assert!(layer.switches.audio && layer.switches.effects && layer.switches.three_d);
    assert!(layer.switches.collapse && layer.switches.motion_blur);
    assert!(layer.track_matte.is_none());
    assert!(format!("{report:?}").contains("no source layer"));
}

#[test]
fn aep_rejects_cyclic_layer_parenting() {
    let mut bytes = fixture::project();
    let at = bytes.windows(4).position(|b| b == b"ldta").unwrap() + 8;
    bytes[at + 132..at + 136].copy_from_slice(&101u32.to_be_bytes());
    assert!(aep::from_aep(&bytes).unwrap_err().contains("cyclic"));
}

#[test]
#[ignore = "requires EFFECTCRAFT_AEP_TEST_FILE pointing to a local, uncommitted project"]
fn external_aep_round_trip() {
    let path = std::env::var("EFFECTCRAFT_AEP_TEST_FILE").expect("set EFFECTCRAFT_AEP_TEST_FILE");
    let before = std::fs::read(&path).unwrap();
    let mut s = Session::new();
    s.check_footage_on_open = false;
    let result = s.execute("file.open", json!({"path":path})).unwrap();
    println!("{result}");
    let count = s.project.comps().count();
    assert!(count > 0);
    let layers: usize = s.project.comps().map(|(_, c)| c.layers.len()).sum();
    println!("compositions={count}, layers={layers}");
    let dir = temp("external");
    let saved = dir.join("roundtrip.ecproj");
    s.execute("file.saveAs", json!({"path":saved})).unwrap();
    s.execute("file.open", json!({"path":saved})).unwrap();
    assert_eq!(s.project.comps().count(), count);
    assert_eq!(s.project.comps().map(|(_, c)| c.layers.len()).sum::<usize>(), layers);
    assert_eq!(std::fs::read(path).unwrap(), before);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn aep_windows_alias_relinks_and_preserves_folder_reference() {
    let dir = temp("alias");
    let media = dir.join("still.png");
    std::fs::write(&media, b"only path resolution is under test").unwrap();
    let bytes = fixture::footage_project(r"C:\old\assets\still.png");
    let (p, report) = aep::from_aep_or_aepx_with_report(&bytes, Some(&dir.join("project.aep"))).unwrap();
    assert_eq!(report.missing_footage_count, 0);
    let item = p.items.values().find(|i| matches!(i.kind, ItemKind::Footage(_))).unwrap();
    let ItemKind::Footage(f) = &item.kind else { unreachable!() };
    assert_eq!(f.path, media.to_string_lossy());
    assert_eq!(p.item(item.parent.unwrap()).unwrap().name, "Assets");
    let (_, missing) = aep::from_aep_or_aepx_with_report(&bytes, None).unwrap();
    assert_eq!(missing.missing_footage_count, 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn aep_retains_psd_source_layer_instead_of_merged_composite() {
    let p = aep::from_aep(&fixture::layered_footage_project()).unwrap();
    let item = p.items.values().find(|i| matches!(i.kind, ItemKind::Footage(_))).unwrap();
    let ItemKind::Footage(f) = &item.kind else { unreachable!() };
    let source = f.layer.as_ref().unwrap();
    assert_eq!(source.index, 2);
    assert_eq!(source.name, "Stars");
    assert!(!source.layer_size, "composition-sized PSD layer keeps the full source canvas");
}

#[test]
fn aep_bad_input_leaves_current_work_unchanged() {
    let dir = temp("invalid");
    let path = dir.join("bad.aep");
    std::fs::write(&path, b"RIFX\xff\xff\xff\xffEgg!LIST\xff\xff\xff\xff").unwrap();
    let mut s = Session::new();
    s.execute("comp.new", json!({"name":"Keep me"})).unwrap();
    let before = s.project.to_file_json().unwrap();
    assert!(s.execute("file.open", json!({"path":path})).is_err());
    let result = s.execute("file.import", json!({"path":path})).unwrap();
    assert!(!result["errors"].as_array().unwrap().is_empty());
    assert_eq!(s.project.to_file_json().unwrap(), before);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn aep_rejects_non_finite_transforms_and_excessive_precomp_depth() {
    let mut bytes = fixture::project();
    let at = bytes.windows(4).position(|b| b == b"cdat").unwrap() + 8;
    bytes[at..at + 8].copy_from_slice(&f64::NAN.to_be_bytes());
    assert!(aep::from_aep(&bytes).unwrap_err().contains("transform"));
    // Child compositions precede their parents: memoization must still enforce graph height.
    let comps: Vec<_> = (1..=66).map(|i| fixture::comp(i, "Nested", i - 1)).collect();
    assert!(aep::from_aep(&fixture::container(&comps)).unwrap_err().contains("nesting"));
}

#[test]
fn aep_detection_does_not_capture_native_xml_with_effect_names() {
    assert!(!aep::is_aepx(b"<?xml version=\"1.0\"?><EffectCraft><text>AfterEffects Egg</text></EffectCraft>"));
    assert!(aep::is_aepx(b"<?xml version=\"1.0\"?><AfterEffectsProject/>"));
}

#[test]
fn aep_native_reader_limits_allocation_for_large_files() {
    let dir = temp("large");
    let path = dir.join("large.aep");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(aep::MAX_FILE_BYTES as u64 + 1).unwrap();
    let mut s = Session::new();
    let err = s.execute("file.open", json!({"path":path})).unwrap_err().to_string();
    assert!(err.contains("size limit"), "{err}");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn extensionless_aep_is_bounded_before_reading_and_keeps_unsaved_work() {
    use std::io::Write;
    let dir = temp("large-extensionless");
    let mut session = Session::new();
    session.execute("comp.new", json!({"name":"Unsaved work"})).unwrap();
    let before = session.project.to_file_json().unwrap();
    for (name, header) in [("binary", b"RIFX\0\0\0\x04Egg!".as_slice()), ("xml", b"<?xml version=\"1.0\"?><AfterEffectsProject>".as_slice())] {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(header).unwrap();
        file.set_len(aep::MAX_FILE_BYTES as u64 + 1).unwrap();
        let error = session.execute("file.open", json!({"path":path})).unwrap_err().to_string();
        assert!(error.contains("file exceeds import size limit"), "{error}");
        assert_eq!(session.project.to_file_json().unwrap(), before);
        assert!(session.is_dirty());
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn native_project_read_does_not_inherit_the_aep_size_limit() {
    use effectcraft_engine::Services;
    use std::io::Write;
    let dir = temp("large-native");
    let path = dir.join("project.ecproj");
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(b"{\"schema\":").unwrap();
    let length = aep::MAX_FILE_BYTES + 1;
    file.set_len(length as u64).unwrap();
    let data = effectcraft_engine::FsServices.read_project_file(path.to_str().unwrap()).unwrap();
    assert_eq!(data.len(), length);
    assert!(data.starts_with(b"{\"schema\":"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn reversed_layer_maps_source_trim_and_animation_backwards() {
    let mut bytes = fixture::project();
    let offset = bytes.windows(4).position(|tag| tag == b"ldta").unwrap() + 8;
    bytes[offset + 8..offset + 12].copy_from_slice(&(-1i32).to_be_bytes());
    bytes[offset + 12..offset + 16].copy_from_slice(&120u32.to_be_bytes());
    bytes[offset + 20..offset + 24].copy_from_slice(&24u32.to_be_bytes());
    bytes[offset + 28..offset + 32].copy_from_slice(&72u32.to_be_bytes());
    let (project, _) = aep::from_aep_or_aepx_with_report(&bytes, None).unwrap();
    let main = project.items.values().find(|item| item.name == "Main").unwrap().as_comp().unwrap();
    let layer = &main.layers[0];
    assert_eq!(layer.stretch, -100.0);
    assert_eq!(layer.start_time.seconds(), 5.0);
    assert_eq!(layer.in_point.seconds(), 2.0);
    assert_eq!(layer.out_point.seconds(), 4.0);
    assert_eq!(layer.layer_time(effectcraft_time::Tick::from_units(5, 2)).seconds(), 2.5);
    assert_eq!(layer.layer_time(effectcraft_time::Tick::from_units(7, 2)).seconds(), 1.5);
}

#[test]
fn unknown_source_item_is_reported_instead_of_silently_disappearing() {
    let bytes = fixture::container(&[fixture::comp(1, "Unknown source", 999)]);
    let (project, report) = aep::from_aep_or_aepx_with_report(&bytes, None).unwrap();
    assert!(report.warnings.iter().any(|warning| warning.contains("missing AEP source item 999")));
    assert!(matches!(project.comps().next().unwrap().1.layers[0].source, LayerSource::Null));
}

#[test]
fn extensionless_aep_and_aliases_remain_protected_after_report_is_cleared() {
    let dir = temp("extensionless");
    let original = dir.join("source-project");
    let bytes = fixture::project();
    std::fs::write(&original, &bytes).unwrap();
    let mut session = Session::new();
    session.execute("file.open", json!({"path": original})).unwrap();
    session.execute("file.saveAs", json!({"path": dir.join("safe.ecproj")})).unwrap();
    session.execute("file.import", json!({"paths": []})).unwrap();
    assert!(session.last_aep_report.is_none(), "report lifecycle must not remove source protection");
    let aliases = [original.clone(), dir.join(".").join("source-project")];
    for path in aliases {
        for command in ["file.save", "file.saveAs", "file.saveCopy", "file.saveCopyAsXml"] {
            assert!(session.execute(command, json!({"path": path})).is_err(), "{command} must protect the original binary at {path:?}");
        }
    }
    #[cfg(unix)]
    {
        let alias = dir.join("alias.ecproj");
        std::os::unix::fs::symlink(&original, &alias).unwrap();
        for command in ["file.saveAs", "file.saveCopy", "file.saveCopyAsXml"] {
            assert!(session.execute(command, json!({"path": alias})).is_err(), "{command} must protect a symlink alias");
        }
    }
    assert_eq!(std::fs::read(&original).unwrap(), bytes);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn aep_three_d_and_motion_blur_flags_are_independent() {
    for (flags, three_d, motion_blur) in [(7u32 | 0x400, true, false), (7 | 0x10000, false, true), (7 | 0x10400, true, true)] {
        let mut bytes = fixture::project();
        let offset = bytes.windows(4).position(|tag| tag == b"ldta").unwrap() + 8;
        bytes[offset + 36..offset + 40].copy_from_slice(&flags.to_be_bytes());
        let (project, _) = aep::from_aep_or_aepx_with_report(&bytes, None).unwrap();
        let main = project.items.values().find(|item| item.name == "Main").unwrap().as_comp().unwrap();
        let switches = &main.layers[0].switches;
        assert_eq!(switches.three_d, three_d);
        assert_eq!(switches.motion_blur, motion_blur);
    }
}

#[test]
fn aep_camera_aims_at_its_point_of_interest_only_when_flagged() {
    use effectcraft_project::AutoOrient;
    for (flags, aim) in [(1u32, AutoOrient::Off), (1 | 0x10000, AutoOrient::TowardsPointOfInterest)] {
        let mut bytes = fixture::camera_project();
        let offset = bytes.windows(4).position(|tag| tag == b"ldta").unwrap() + 8;
        bytes[offset + 36..offset + 40].copy_from_slice(&flags.to_be_bytes());
        let (project, _) = aep::from_aep_or_aepx_with_report(&bytes, None).unwrap();
        let comp = project.items.values().find(|item| item.name == "Camera").unwrap().as_comp().unwrap();
        let layer = &comp.layers[0];
        assert!(matches!(layer.source, LayerSource::Camera));
        assert_eq!(layer.auto_orient, aim);
        assert!(!layer.switches.motion_blur, "a camera has no motion blur switch");
    }
}
