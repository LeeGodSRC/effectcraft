//! Time remapping and separated Position dimensions.

use effectcraft_color::Label;
use effectcraft_keyframe::{Keyframe, Value};
use effectcraft_project::build::{self, Ids};
use effectcraft_project::{Comp, ItemKind, LayerSource, Node, Project, Solid};
use effectcraft_time::{FrameRate, Tick};

use crate::render_frame;

fn s(x: f64) -> Tick {
    Tick::from_seconds_f64(x)
}

#[test]
fn separated_position_drives_the_transform() {
    let mut p = Project::default();
    let comp = Comp::new(200, 100, FrameRate::FPS_30, s(2.0));
    let cid = p.add_item("Comp", Label::Sandstone, None, ItemKind::Comp(comp.clone().into()));
    let sid = p.add_item("S", Label::Red, None, ItemKind::Solid(Solid { color: [1.0, 1.0, 1.0], width: 20, height: 20, pixel_aspect: 1.0 }));
    let mut l = build::layer(&mut p, &comp, "S", LayerSource::Solid { item: sid }, (20, 20), None);
    let mut next = p.next_id;
    let mut ids = Ids(&mut next);
    let mut px = ids.prop("positionX", "X Position", Value::Scalar(20.0));
    px.keys = vec![Keyframe::new(Tick::ZERO, Value::Scalar(20.0)), Keyframe::new(s(1.0), Value::Scalar(180.0))];
    let py = ids.prop("positionY", "Y Position", Value::Scalar(50.0));
    let pz = ids.prop("positionZ", "Z Position", Value::Scalar(0.0));
    p.next_id = next;
    let tr = l.props.sub_mut("transform").unwrap();
    // The combined property is ignored while separated.
    tr.get_mut("position").unwrap().value = Value::Vec3([100.0, 10.0, 0.0]);
    tr.children.extend([Node::Prop(px), Node::Prop(py), Node::Prop(pz)]);
    p.comp_mut(cid).unwrap().layers.push(l);
    let a = render_frame(&p, cid, Tick::ZERO, 1.0);
    assert!(a.get(20, 50)[3] > 0.99 && a.get(100, 10)[3] < 0.01);
    let b = render_frame(&p, cid, s(0.5), 1.0);
    assert!(b.get(100, 50)[3] > 0.99 && b.get(20, 50)[3] < 0.01);
}

#[test]
fn time_remap_picks_the_source_frame() {
    let mut p = Project::default();
    let outer = Comp::new(100, 100, FrameRate::FPS_30, s(4.0));
    let mut inner = Comp::new(100, 100, FrameRate::FPS_30, s(4.0));
    // Inner comp: a solid that moves from x=10 (t=0) to x=90 (t=2).
    let sid = p.add_item("S", Label::Red, None, ItemKind::Solid(Solid { color: [1.0, 1.0, 1.0], width: 10, height: 10, pixel_aspect: 1.0 }));
    let mut l = build::layer(&mut p, &inner, "S", LayerSource::Solid { item: sid }, (10, 10), None);
    l.props.prop_mut("transform/position").unwrap().keys =
        vec![Keyframe::new(Tick::ZERO, Value::Vec3([10.0, 50.0, 0.0])), Keyframe::new(s(2.0), Value::Vec3([90.0, 50.0, 0.0]))];
    inner.layers.push(l);
    let iid = p.add_item("Inner", Label::Sandstone, None, ItemKind::Comp(inner.into()));
    let cid = p.add_item("Outer", Label::Sandstone, None, ItemKind::Comp(outer.clone().into()));
    let mut pre = build::layer(&mut p, &outer, "Inner", LayerSource::Comp { item: iid }, (100, 100), None);
    let mut next = p.next_id;
    // Freeze on source time 2 s.
    let tr = Ids(&mut next).prop("timeRemap", "Time Remap", Value::Scalar(2.0));
    p.next_id = next;
    let mut tr = tr;
    tr.keys = vec![Keyframe::new(Tick::ZERO, Value::Scalar(2.0)).hold()];
    pre.props.children.push(Node::Prop(tr));
    p.comp_mut(cid).unwrap().layers.push(pre);
    let img = render_frame(&p, cid, Tick::ZERO, 1.0);
    assert!(img.get(90, 50)[3] > 0.99, "frozen at the end position");
    assert!(img.get(10, 50)[3] < 0.01);
}

#[test]
fn time_remap_at_the_precomp_duration_is_empty() {
    for collapse in [false, true] {
        for preserve_frame_rate in [false, true] {
            let mut p = Project::default();
            let outer = Comp::new(96, 32, FrameRate::FPS_30, s(4.0));
            let mut inner = Comp::new(32, 32, FrameRate::FPS_24, s(1.0));
            inner.preserve_frame_rate = preserve_frame_rate;
            let sid = p.add_item("Solid", Label::Red, None, ItemKind::Solid(Solid { color: [1.0; 3], width: 32, height: 32, pixel_aspect: 1.0 }));
            let mut solid = build::layer(&mut p, &inner, "Solid", LayerSource::Solid { item: sid }, (32, 32), None);
            solid.out_point = s(4.0);
            inner.layers.push(solid);
            let iid = p.add_item("Inner", Label::Sandstone, None, ItemKind::Comp(inner.into()));
            let cid = p.add_item("Outer", Label::Sandstone, None, ItemKind::Comp(outer.clone().into()));
            // Shared source, three different remapped times: last frame, exact end,
            // and beyond the end. The outer layers themselves are still in range.
            for (index, time) in [23.0 / 24.0, 1.0, 1.5].into_iter().enumerate() {
                let mut pre = build::layer(&mut p, &outer, "Remapped", LayerSource::Comp { item: iid }, (32, 32), None);
                pre.out_point = s(4.0);
                pre.switches.collapse = collapse;
                pre.props.prop_mut("transform/position").unwrap().value = Value::Vec3([16.0 + index as f64 * 32.0, 16.0, 0.0]);
                let remap = Ids(&mut p.next_id).prop("timeRemap", "Time Remap", Value::Scalar(time));
                pre.props.children.push(Node::Prop(remap));
                p.comp_mut(cid).unwrap().layers.push(pre);
            }
            let image = render_frame(&p, cid, s(2.0), 1.0);
            assert!(image.get(16, 16)[3] > 0.99, "last frame, collapse={collapse}, preserve={preserve_frame_rate}");
            assert_eq!(image.get(48, 16)[3], 0.0, "exact end, collapse={collapse}, preserve={preserve_frame_rate}");
            assert_eq!(image.get(80, 16)[3], 0.0, "past end, collapse={collapse}, preserve={preserve_frame_rate}");
        }
    }
}
