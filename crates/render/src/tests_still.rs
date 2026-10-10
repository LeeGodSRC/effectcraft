//! Pictures kept across frames: the frame of a comp nothing moves in, the still layers at the
//! bottom of a comp, and the layer cache's preference for them.

use std::sync::Arc;

use effectcraft_color::Label;
use effectcraft_effects::Buf;
use effectcraft_keyframe::{Keyframe, Value};
use effectcraft_project::build;
use effectcraft_project::{Comp, ItemId, ItemKind, LayerSource, Project, Solid};
use effectcraft_time::{FrameRate, Tick};

use crate::{Image, LayerCache, NoFootage, RenderOpts, Renderer, render_frame};

fn s(x: f64) -> Tick {
    Tick::from_seconds_f64(x)
}

fn solid(p: &mut Project, comp: &Comp, color: [f32; 3], size: u32) -> effectcraft_project::Layer {
    let sid = p.add_item("S", Label::Red, None, ItemKind::Solid(Solid { color, width: size, height: size, pixel_aspect: 1.0 }));
    build::layer(p, comp, "S", LayerSource::Solid { item: sid }, (size, size), None)
}

/// An outer comp with, bottom to top: a precomp of a comp nothing moves in (two solids), a still
/// half-transparent solid, and a small solid that moves across. Returns (project, outer, inner).
fn scene() -> (Project, ItemId, ItemId) {
    let mut p = Project::default();
    let mut inner = Comp::new(100, 100, FrameRate::FPS_30, s(4.0));
    let mut a = solid(&mut p, &inner, [0.2, 0.4, 0.6], 100);
    a.props.prop_mut("transform/opacity").unwrap().value = Value::Scalar(70.0);
    let mut b = solid(&mut p, &inner, [0.9, 0.1, 0.3], 30);
    b.props.prop_mut("transform/position").unwrap().value = Value::Vec3([30.0, 60.0, 0.0]);
    inner.layers = vec![b, a];
    let iid = p.add_item("Inner", Label::Sandstone, None, ItemKind::Comp(inner.into()));
    let outer = Comp::new(100, 100, FrameRate::FPS_30, s(4.0));
    let cid = p.add_item("Outer", Label::Sandstone, None, ItemKind::Comp(outer.clone().into()));
    let pre = build::layer(&mut p, &outer, "Inner", LayerSource::Comp { item: iid }, (100, 100), None);
    let mut still = solid(&mut p, &outer, [0.1, 0.8, 0.2], 50);
    still.props.prop_mut("transform/opacity").unwrap().value = Value::Scalar(40.0);
    let mut mover = solid(&mut p, &outer, [1.0, 1.0, 1.0], 10);
    mover.props.prop_mut("transform/position").unwrap().keys =
        vec![Keyframe::new(Tick::ZERO, Value::Vec3([10.0, 50.0, 0.0])), Keyframe::new(s(2.0), Value::Vec3([90.0, 50.0, 0.0]))];
    // Top first.
    p.comp_mut(cid).unwrap().layers = vec![mover, still, pre];
    (p, cid, iid)
}

fn cached(p: &Project, cache: &LayerCache, cid: ItemId, t: Tick) -> Image {
    let mut r = Renderer::new(p, &NoFootage, RenderOpts::default());
    r.cache = Some(cache);
    r.comp_frame(cid, t)
}

/// A frame rendered after its neighbours, from what the cache kept of them, is the frame
/// rendered cold; and the still parts are found in the cache, not rendered again.
#[test]
fn frames_after_the_first_reuse_the_still_parts_and_match_cold_frames() {
    let (p, cid, _) = scene();
    let cache = LayerCache::default();
    let mut hits = 0;
    for f in 0..6 {
        let t = FrameRate::FPS_30.tick_of(f * 7);
        let before = cache.stats().hits;
        assert_eq!(cached(&p, &cache, cid, t).data, render_frame(&p, cid, t, 1.0).data, "frame {f}");
        if f > 0 {
            assert!(cache.stats().hits > before, "frame {f} found nothing in the cache");
            hits += 1;
        }
    }
    assert_eq!(hits, 5);
    // Going back shows the same frames again.
    for f in [3, 0, 5] {
        let t = FrameRate::FPS_30.tick_of(f * 7);
        assert_eq!(cached(&p, &cache, cid, t).data, render_frame(&p, cid, t, 1.0).data, "frame {f} again");
    }
}

/// Edits reach through the kept pictures: a change inside the nested comp, to the still layer
/// under the moving one, or to a solid's colour shows in the next frame.
#[test]
fn an_edit_to_a_still_part_is_not_served_from_the_cache() {
    let (mut p, cid, iid) = scene();
    let cache = LayerCache::default();
    let t = s(0.5);
    let first = cached(&p, &cache, cid, t);
    // Inside the nested comp.
    p.comp_mut(iid).unwrap().layers[0].props.prop_mut("transform/position").unwrap().value = Value::Vec3([70.0, 20.0, 0.0]);
    let moved = cached(&p, &cache, cid, s(0.6));
    assert_eq!(moved.data, render_frame(&p, cid, s(0.6), 1.0).data);
    assert_ne!(cached(&p, &cache, cid, t).data, first.data, "the nested edit shows");
    // The still layer of the outer comp.
    p.comp_mut(cid).unwrap().layers[1].props.prop_mut("transform/opacity").unwrap().value = Value::Scalar(90.0);
    assert_eq!(cached(&p, &cache, cid, t).data, render_frame(&p, cid, t, 1.0).data);
    // A source item.
    let LayerSource::Solid { item: sid } = p.comp(iid).unwrap().layers[1].source else { return };
    let ItemKind::Solid(sol) = &mut p.item_mut(sid).unwrap().kind else { return };
    sol.color = [0.0, 0.0, 1.0];
    assert_eq!(cached(&p, &cache, cid, t).data, render_frame(&p, cid, t, 1.0).data);
    // A layer of the nested comp starts to move: its frame is no longer kept, and still right.
    p.comp_mut(iid).unwrap().layers[0].props.prop_mut("transform/position").unwrap().keys =
        vec![Keyframe::new(Tick::ZERO, Value::Vec3([10.0, 10.0, 0.0])), Keyframe::new(s(2.0), Value::Vec3([90.0, 90.0, 0.0]))];
    for x in [0.5, 0.6, 1.0] {
        assert_eq!(cached(&p, &cache, cid, s(x)).data, render_frame(&p, cid, s(x), 1.0).data, "moving at {x}");
    }
}

/// A layer that starts or ends changes the picture of an otherwise still comp.
#[test]
fn a_still_comp_follows_its_layers_spans() {
    let (mut p, cid, iid) = scene();
    p.comp_mut(iid).unwrap().layers[0].out_point = s(1.0);
    p.comp_mut(cid).unwrap().layers[1].in_point = s(1.5);
    let cache = LayerCache::default();
    for x in [0.5, 0.9, 1.0, 1.2, 1.5, 1.8, 0.5] {
        assert_eq!(cached(&p, &cache, cid, s(x)).data, render_frame(&p, cid, s(x), 1.0).data, "at {x}");
    }
}

/// Buffers every frame can use outlast the ones a frame used once, however recent those are.
#[test]
fn the_cache_keeps_stable_buffers_ahead_of_the_rest() {
    let buf = || Arc::new(Buf { img: Image::new(64, 64), offset: [0.0; 2], scale: 1.0 });
    let one = 64 * 64 * 16;
    let cache = LayerCache::new(one * 8);
    cache.insert_stable(1, buf());
    cache.insert_stable(2, buf());
    for k in 100..140 {
        cache.insert(k, buf());
    }
    assert!(cache.get(1).is_some() && cache.get(2).is_some(), "stable buffers were pushed out");
    assert!(cache.get(139).is_some() && cache.get(100).is_none(), "the oldest of the rest make room");
    assert!(cache.stats().bytes <= one * 8);
    // Stable buffers alone past the budget: the least recently used of them go.
    for k in 200..220 {
        cache.insert_stable(k, buf());
    }
    assert!(cache.stats().bytes <= one * 8);
    assert!(cache.get(219).is_some() && cache.get(1).is_none());
}
