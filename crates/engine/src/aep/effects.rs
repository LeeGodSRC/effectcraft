//! Clean-room AEP effect adapter. Definitions and values are read from the project, not presets.
use super::{ChunkNode, streams};
use effectcraft_keyframe::Value;
use effectcraft_project::{ParamUi, PropGroup, build::Ids};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub(super) struct DecodedEffect {
    name: String,
    match_name: String,
    enabled: bool,
    params: Vec<DecodedParam>,
}
#[derive(Clone, Debug)]
struct DecodedParam {
    id: String,
    name: String,
    kind: u8,
    options: Vec<String>,
    value: Option<Value>,
    stream: Option<streams::DecodedStream>,
    reference: Option<u32>,
    custom: Vec<u8>,
}

pub(super) fn effect_id(name: &str) -> Option<&'static str> {
    Some(match name {
        "ADBE Polar Coordinates" => "ec.distort.polar",
        "ADBE Ramp" => "ec.generate.gradientramp",
        "ADBE Shift Channels" => "ec.channel.shiftchannels",
        "ADBE Glo2" => "ec.stylize.glow",
        "CC Particle World" => "ec.sim.ccparticleworld",
        "ADBE CurvesCustom" => "ec.color.curves",
        "ADBE Gaussian Blur" | "ADBE Gaussian Blur 2" => "ec.blur.gaussian",
        "ADBE Color Balance (HLS)" => "ec.color.colorbalancehls",
        "ADBE Mirror" => "ec.distort.mirror",
        "ADBE Offset" => "ec.distort.offset",
        "APC Colorama" => "ec.color.colorama",
        "ADBE Fractal Noise" => "ec.noise.fractal",
        "ADBE Mosaic" => "ec.stylize.mosaic",
        "ADBE Fast Blur" => "ec.blur.gaussian",
        "ADBE Stroke" => "ec.generate.stroke",
        "ADBE Point3D Control" => "ec.control.point3d",
        "ADBE Motion Blur" => "ec.blur.directional",
        "ADBE Tritone" => "ec.color.tritone",
        "ADBE Venetian Blinds" => "ec.transition.venetian",
        "ADBE Easy Levels2" => "ec.color.levels",
        "ADBE Radial Blur" => "ec.blur.radial",
        _ => return None,
    })
}
fn text(data: &[u8]) -> String {
    String::from_utf8_lossy(data.split(|b| *b == 0).next().unwrap_or(&[])).to_string()
}
fn chunk<'a>(nodes: &'a [ChunkNode<'a>], tag: &[u8; 4]) -> Option<&'a [u8]> {
    nodes.iter().find_map(|n| match n {
        ChunkNode::Chunk { id, data } if id == tag => Some(*data),
        _ => None,
    })
}
fn uint(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}
fn fixed(data: &[u8], offset: usize) -> Option<f64> {
    Some(uint(data, offset)? as i32 as f64 / 65536.0)
}
fn default_value(kind: u8, data: &[u8]) -> Option<Value> {
    Some(match kind {
        1 | 4 | 7 => Value::Scalar(uint(data, 56)? as f64),
        2 | 3 => Value::Scalar(fixed(data, 56)?),
        10 => Value::Scalar(f64::from_be_bytes(data.get(56..64)?.try_into().ok()?)),
        18 => Value::Vec3([56, 64, 72].map(|o| data.get(o..o + 8).and_then(|v| v.try_into().ok()).map(f64::from_be_bytes).unwrap_or(0.0) / 100.0)),
        5 => {
            let b = data.get(56..60)?;
            Value::Color([*b.first()? as f64, *b.get(1)? as f64, *b.get(2)? as f64, *b.get(3)? as f64])
        }
        6 => {
            let factor = if uint(data, 64) == Some(1) { 1.0 } else { 0.01 };
            Value::Vec2([fixed(data, 56)? * factor, fixed(data, 60)? * factor])
        }
        _ => return None,
    })
}
/// AEP stores each effect's parameter declarations once, then reuses them across instances.
/// Keep this catalog scoped to a single import so another document cannot inherit defaults.
#[derive(Default)]
pub(super) struct Definitions(HashMap<String, Vec<DecodedParam>>);

pub(super) fn definitions(nodes: &[ChunkNode<'_>]) -> Result<Definitions, String> {
    fn collect(nodes: &[ChunkNode<'_>], out: &mut Definitions, depth: usize) -> Result<(), String> {
        if depth > 64 {
            return Err("AEP effect definitions exceed nesting limit".into());
        }
        let mut name = String::new();
        for node in nodes {
            match node {
                ChunkNode::Chunk { id: [b't', b'd', b'm', b'n'], data } => name = text(data),
                ChunkNode::List { list_type, children, .. } if list_type == b"sspc" && chunk(children, b"fnam").is_some() => {
                    let params = parameter_definitions(children)?;
                    if !params.is_empty() {
                        if out.0.len() >= 1024 && !out.0.contains_key(&name) {
                            return Err("AEP exceeds 1024 effect types".into());
                        }
                        out.0.entry(name.clone()).or_insert(params);
                    }
                }
                ChunkNode::List { children, .. } => collect(children, out, depth + 1)?,
                _ => {}
            }
        }
        Ok(())
    }
    let mut result = Definitions::default();
    collect(nodes, &mut result, 0)?;
    Ok(result)
}
fn parameter_definitions(nodes: &[ChunkNode<'_>]) -> Result<Vec<DecodedParam>, String> {
    let mut params = Vec::new();
    for node in nodes {
        if let ChunkNode::List { list_type: [b'p', b'a', b'r', b'T'], children, .. } = node {
            let mut id = String::new();
            for n in children {
                if let ChunkNode::Chunk { id: tag, data } = n {
                    if tag == b"tdmn" {
                        id = text(data);
                    } else if tag == b"pard" {
                        let kind = *data.get(15).ok_or("truncated AEP effect parameter definition")?;
                        if params.len() >= 1024 {
                            return Err("AEP effect exceeds 1024 parameter definitions".into());
                        }
                        let value = default_value(kind, data);
                        if value.as_ref().is_some_and(|v| v.components().iter().any(|x| !x.is_finite() || x.abs() > 1e12)) {
                            return Err("invalid AEP effect parameter default".into());
                        }
                        params.push(DecodedParam {
                            id: id.clone(),
                            name: text(data.get(16..48).ok_or("truncated AEP parameter name")?),
                            kind,
                            options: Vec::new(),
                            value,
                            stream: None,
                            reference: None,
                            custom: Vec::new(),
                        });
                    } else if let Some(p) = params.last_mut() {
                        if tag == b"pdnm" {
                            p.options = text(data).split('|').map(str::to_string).collect();
                            if p.name.is_empty() {
                                p.name = p.options.first().cloned().unwrap_or_default();
                            }
                        }
                        if tag == b"aRbp" {
                            p.custom = data.to_vec();
                        }
                    }
                }
            }
        }
    }
    Ok(params)
}

pub(super) fn decode(nodes: &[ChunkNode<'_>], definitions: &Definitions) -> Result<Vec<DecodedEffect>, String> {
    let mut result = Vec::new();
    walk(nodes, definitions, &mut result, 0)?;
    Ok(result)
}
fn walk(nodes: &[ChunkNode<'_>], definitions: &Definitions, out: &mut Vec<DecodedEffect>, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err("AEP effects exceed nesting limit".into());
    }
    let mut name = String::new();
    for node in nodes {
        match node {
            ChunkNode::Chunk { id: [b't', b'd', b'm', b'n'], data } => name = text(data),
            ChunkNode::List { list_type, children, .. } if list_type == b"sspc" && chunk(children, b"fnam").is_some() => {
                if out.len() >= 256 {
                    return Err("AEP layer exceeds 256 effects".into());
                }
                out.push(decode_effect(&name, children, definitions)?);
            }
            ChunkNode::List { children, .. } => walk(children, definitions, out, depth + 1)?,
            _ => {}
        }
    }
    Ok(())
}
fn decode_effect(match_name: &str, nodes: &[ChunkNode<'_>], definitions: &Definitions) -> Result<DecodedEffect, String> {
    let mut effect = DecodedEffect {
        name: chunk(nodes, b"fnam").map(text).unwrap_or_else(|| match_name.into()),
        match_name: match_name.into(),
        enabled: true,
        params: Vec::new(),
    };
    effect.params = definitions.0.get(match_name).cloned().unwrap_or_default();
    for param in parameter_definitions(nodes)? {
        if let Some(existing) = effect.params.iter_mut().find(|p| p.id == param.id) { *existing = param } else { effect.params.push(param) }
    }
    for node in nodes {
        if let ChunkNode::List { list_type: [b't', b'd', b'g', b'p'], children, .. } = node {
            if let Some(name) = chunk(children, b"tdsn").map(text).filter(|s| !s.is_empty()) {
                effect.name = name;
            }
            // tdsb's low bit is the effect's fx switch; other bits describe the stream.
            effect.enabled = chunk(children, b"tdsb").and_then(|b| uint(b, 0)).is_none_or(|v| v & 1 != 0);
            read_streams(children, &mut effect.params, 0)?;
        }
    }
    Ok(effect)
}
fn read_streams(nodes: &[ChunkNode<'_>], params: &mut [DecodedParam], depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err("AEP effect parameter nesting limit".into());
    }
    let mut name = String::new();
    for node in nodes {
        match node {
            ChunkNode::Chunk { id: [b't', b'd', b'm', b'n'], data } => name = text(data),
            ChunkNode::List { list_type: [b't', b'd', b'b', b's'], children, .. } => {
                if let Some(p) = params.iter_mut().find(|p| p.id == name) {
                    if let Some(label) = chunk(children, b"tdsn").map(text).filter(|s| !s.is_empty()) {
                        p.name = label;
                    }
                    p.reference = chunk(children, b"tdpi").and_then(|b| uint(b, 0));
                    let template = match p.kind {
                        5 => Value::Color([0.0; 4]),
                        6 => Value::Vec2([0.0; 2]),
                        18 => Value::Vec3([0.0; 3]),
                        1..=4 | 7 | 10 => Value::Scalar(0.0),
                        _ => continue,
                    };
                    p.stream = Some(streams::decode(children, &template)?);
                }
            }
            ChunkNode::List { list_type: [b'a', b'R', b'b', b's'], children, .. } => {
                if let Some(p) = params.iter_mut().find(|p| p.id == name)
                    && let Some(data) = chunk(children, b"aRbp")
                {
                    p.custom = data.to_vec();
                }
            }
            ChunkNode::List { children, .. } => read_streams(children, params, depth + 1)?,
            _ => {}
        }
    }
    Ok(())
}
fn normalized(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(char::to_lowercase).collect()
}
fn alias(s: &str) -> String {
    match normalized(s).as_str() {
        "luminance" => "intensity".into(),
        "viscouse" => "viscous".into(),
        "rg" => "redgreen".into(),
        "gb" => "greenblue".into(),
        "rb" => "bluered".into(),
        "channel" => "channel".into(),
        s => s.to_string(),
    }
}
fn native_param<'a>(effect: &str, p: &DecodedParam, spec: &'a effectcraft_effects::EffectSpec) -> Option<&'a effectcraft_effects::ParamSpec> {
    let explicit = match (effect, p.id.rsplit('-').next().unwrap_or("")) {
        ("CC Particle World", "0007") => Some("producer/producerX"),
        ("CC Particle World", "0008") => Some("producer/producerY"),
        ("CC Particle World", "0009") => Some("producer/producerZ"),
        ("CC Particle World", "0045") => Some("texture/layer"),
        ("CC Particle World", "0033") => Some("extras/effectCamera/distance"),
        ("CC Particle World", "0083") => Some("depthCue/distance"),
        _ => None,
    };
    if let Some(id) = explicit {
        return spec.params.iter().find(|q| q.id == id);
    }
    spec.params.iter().find(|q| normalized(q.name) == normalized(&p.name))
}
fn scalar_scale(effect: &str, id: &str) -> f64 {
    match (effect, id) {
        ("ADBE Polar Coordinates", "interpolation") | ("ADBE Glo2", "abMidpoint") | ("ADBE Stroke", "brushHardness" | "opacity") => 100.0,
        ("ADBE Glo2", "threshold") => 100.0 / 255.0,
        _ => 1.0,
    }
}
fn convert(value: &Value, p: &DecodedParam, target: &effectcraft_effects::ParamSpec, effect: &str, size: [f64; 2], layers: &HashMap<u32, u64>) -> Value {
    let scalar = value.components().first().copied().unwrap_or(0.0);
    match &target.default {
        Value::Color(_) => {
            let c = value.components();
            Value::Color([c.get(1).copied().unwrap_or(0.0) / 255.0, c.get(2).copied().unwrap_or(0.0) / 255.0, c.get(3).copied().unwrap_or(0.0) / 255.0, 1.0])
        }
        Value::Vec2(_) => {
            let c = value.components();
            Value::Vec2([c.first().copied().unwrap_or(0.0) * size[0], c.get(1).copied().unwrap_or(0.0) * size[1]])
        }
        Value::Vec3(_) => {
            let c = value.components();
            Value::Vec3([c.first().copied().unwrap_or(0.0) * size[0], c.get(1).copied().unwrap_or(0.0) * size[1], c.get(2).copied().unwrap_or(0.0) * size[1]])
        }
        Value::Bool(_) => Value::Bool(scalar != 0.0),
        Value::Enum(_) => {
            let index = (scalar.max(1.0) as usize).saturating_sub(1);
            let mapped = if let ParamUi::Popup { options } = &target.ui {
                p.options
                    .get(index)
                    .and_then(|label| options.iter().position(|s| alias(s) == alias(label)))
                    .unwrap_or(index.min(options.len().saturating_sub(1)))
            } else {
                index
            };
            Value::Enum(mapped as u32)
        }
        Value::Layer(_) => Value::Layer(p.reference.and_then(|id| layers.get(&id).copied())),
        Value::Scalar(_) => {
            let scale = scalar_scale(effect, target.id);
            Value::Scalar(scalar * scale)
        }
        _ => value.clone(),
    }
}
pub(super) fn instantiate(
    decoded: &[DecodedEffect],
    ids: &mut Ids,
    size: [f64; 2],
    layers: &HashMap<u32, u64>,
) -> Result<(Vec<PropGroup>, Vec<String>), String> {
    let mut groups = Vec::new();
    let mut warnings = Vec::new();
    for effect in decoded {
        let Some(spec) = effect_id(&effect.match_name).and_then(effectcraft_effects::find) else {
            warnings.push(format!("Unsupported AEP effect: {}", effect.match_name));
            continue;
        };
        let mut group = effectcraft_effects::instantiate(spec, ids, &effect.name, size);
        group.enabled = effect.enabled;
        for p in &effect.params {
            if p.kind == 11 {
                apply_custom(&effect.match_name, p, &mut group, &mut warnings)?;
                continue;
            }
            let Some(target) = native_param(&effect.match_name, p, spec) else {
                if p.stream.is_some() && matches!(p.kind, 1..=7 | 10 | 18) {
                    warnings.push(format!("Unmapped AEP parameter: {} / {}", effect.match_name, p.name));
                }
                continue;
            };
            let Some(prop) = group.prop_mut(target.id) else { continue };
            if let Some(value) = &p.value {
                prop.value = convert(value, p, target, &effect.match_name, size, layers);
            }
            if matches!(target.default, Value::Layer(_)) {
                prop.value = Value::Layer(p.reference.and_then(|id| layers.get(&id).copied()));
            }
            if let Some(stream) = &p.stream {
                if let Some(value) = &stream.value {
                    prop.value = convert(value, p, target, &effect.match_name, size, layers);
                }
                prop.keys = stream.keys.clone();
                for key in &mut prop.keys {
                    key.value = convert(&key.value, p, target, &effect.match_name, size, layers);
                    let factors = match target.default {
                        Value::Vec2(_) | Value::Vec3(_) => [size[0], size[1], size[1]],
                        Value::Color(_) => [1.0 / 255.0; 3],
                        _ => [scalar_scale(&effect.match_name, target.id); 3],
                    };
                    for (i, ease) in key.in_ease.iter_mut().enumerate() {
                        ease.speed *= factors.get(i).copied().unwrap_or(factors[0]);
                    }
                    for (i, ease) in key.out_ease.iter_mut().enumerate() {
                        ease.speed *= factors.get(i).copied().unwrap_or(factors[0]);
                    }
                    for (i, tangent) in key.spatial_in.iter_mut().enumerate() {
                        *tangent *= factors.get(i).copied().unwrap_or(1.0);
                    }
                    for (i, tangent) in key.spatial_out.iter_mut().enumerate() {
                        *tangent *= factors.get(i).copied().unwrap_or(1.0);
                    }
                }
                prop.expr = stream.expression.clone();
                warnings.extend(stream.warnings.iter().cloned());
            }
        }
        groups.push(group);
    }
    Ok((groups, warnings))
}
fn apply_custom(effect: &str, p: &DecodedParam, g: &mut PropGroup, warnings: &mut Vec<String>) -> Result<(), String> {
    let data = &p.custom;
    if data.is_empty() {
        return Ok(());
    }
    if effect == "APC Colorama" {
        let count = uint(data, 512).ok_or("truncated AEP Colorama palette")? as usize;
        if count > 64 {
            return Err("AEP Colorama palette exceeds 64 stops".into());
        }
        let mut stops = Vec::new();
        for bytes in data.get(..count.saturating_mul(8)).ok_or("truncated Colorama stops")?.as_chunks::<8>().0 {
            let pos = uint(bytes, 0).ok_or("invalid Colorama position")? as f64 / 65536.0;
            let c = bytes.get(4..8).ok_or("invalid Colorama color")?;
            stops.push(format!(
                "{pos}:{},{},{},{}",
                c.get(1).copied().unwrap_or(0) as f64 / 255.0,
                c.get(2).copied().unwrap_or(0) as f64 / 255.0,
                c.get(3).copied().unwrap_or(0) as f64 / 255.0,
                c.first().copied().unwrap_or(255) as f64 / 255.0
            ));
        }
        if let Some(v) = g.prop_mut("outputCycle/palette") {
            v.value = Value::Str(stops.join(" "));
        }
        let custom = effectcraft_effects::find("ec.color.colorama")
            .and_then(|s| s.params.iter().find(|p| p.id == "outputCycle/usePresetPalette"))
            .and_then(|p| if let ParamUi::Popup { options } = &p.ui { options.iter().position(|s| s == "Custom") } else { None });
        if let (Some(v), Some(index)) = (g.prop_mut("outputCycle/usePresetPalette"), custom) {
            v.value = Value::Enum(index as u32);
        }
    } else if effect == "ADBE CurvesCustom" {
        // Five 256-entry channel LUTs precede the editable curve handles.
        if data.len() < 1284 {
            return Err("truncated AEP Curves lookup tables".into());
        }
        for (i, id) in ["rgb", "red", "green", "blue", "alpha"].iter().enumerate() {
            let start = 4 + i * 256;
            let table = data.get(start..start + 256).ok_or("truncated curve channel")?;
            let points = table.iter().enumerate().map(|(x, y)| format!("{},{}", x as f64 / 255.0, *y as f64 / 255.0)).collect::<Vec<_>>().join(" ");
            if let Some(v) = g.prop_mut(id) {
                v.value = Value::Str(points);
            }
        }
    } else if effect == "ADBE Easy Levels2" && p.name == "Histogram" {
        // Histogram is an editor visualization, not a render parameter.
    } else {
        warnings.push(format!("Unsupported custom AEP parameter: {} / {}", effect, p.name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn param(id: &str, name: &str, kind: u8, value: Value) -> DecodedParam {
        DecodedParam { id: id.into(), name: name.into(), kind, options: Vec::new(), value: Some(value), stream: None, reference: None, custom: Vec::new() }
    }
    fn build(name: &str, params: Vec<DecodedParam>) -> PropGroup {
        let mut next = 1;
        instantiate(
            &[DecodedEffect { name: "My effect".into(), match_name: name.into(), enabled: false, params }],
            &mut Ids(&mut next),
            [1920.0, 1080.0],
            &HashMap::new(),
        )
        .unwrap()
        .0
        .remove(0)
    }
    #[test]
    fn raw_effect_hierarchy_preserves_saved_scalar_values_over_defaults() {
        fn leaf<'a>(id: &[u8; 4], data: &'a [u8]) -> ChunkNode<'a> {
            ChunkNode::Chunk { id: *id, data }
        }
        fn list<'a>(kind: &[u8; 4], children: Vec<ChunkNode<'a>>) -> ChunkNode<'a> {
            ChunkNode::List { id: *b"LIST", list_type: *kind, children }
        }
        for (effect, id, label, field, value) in [
            ("ADBE Gaussian Blur", "ADBE Gaussian Blur-0001", "Blurriness", "blurriness", 80.0f64),
            ("ADBE Color Balance (HLS)", "ADBE Color Balance (HLS)-0002", "Lightness", "lightness", -50.0),
        ] {
            let mut definition = vec![0; 148];
            definition[15] = 2;
            definition[16..16 + label.len()].copy_from_slice(label.as_bytes());
            let data = value.to_be_bytes();
            let enabled = 1u32.to_be_bytes();
            let tree = vec![
                leaf(b"tdmn", effect.as_bytes()),
                list(
                    b"sspc",
                    vec![
                        leaf(b"fnam", effect.as_bytes()),
                        list(b"parT", vec![leaf(b"tdmn", id.as_bytes()), leaf(b"pard", &definition)]),
                        list(
                            b"tdgp",
                            vec![
                                leaf(b"tdsb", &enabled),
                                leaf(b"tdmn", id.as_bytes()),
                                list(b"tdbs", vec![leaf(b"tdsn", label.as_bytes()), leaf(b"cdat", &data)]),
                            ],
                        ),
                    ],
                ),
            ];
            let catalog = definitions(&tree).unwrap();
            let later = value * 0.5;
            let later_data = later.to_be_bytes();
            let later_tree = vec![
                leaf(b"tdmn", effect.as_bytes()),
                list(
                    b"sspc",
                    vec![
                        leaf(b"fnam", effect.as_bytes()),
                        list(
                            b"tdgp",
                            vec![
                                leaf(b"tdsb", &enabled),
                                leaf(b"tdmn", id.as_bytes()),
                                list(b"tdbs", vec![leaf(b"tdsn", label.as_bytes()), leaf(b"cdat", &later_data)]),
                            ],
                        ),
                    ],
                ),
            ];
            let mut decoded = decode(&tree, &catalog).unwrap();
            decoded.extend(decode(&later_tree, &catalog).unwrap());
            let mut next = 1;
            let (groups, warnings) = instantiate(&decoded, &mut Ids(&mut next), [1920.0, 1080.0], &HashMap::new()).unwrap();
            assert!(warnings.is_empty());
            assert_eq!(groups[0].prop(field).unwrap().value, Value::Scalar(value));
            assert_eq!(groups[1].prop(field).unwrap().value, Value::Scalar(later));
        }
    }

    #[test]
    fn every_observed_effect_has_a_native_implementation() {
        for name in [
            "ADBE Polar Coordinates",
            "ADBE Ramp",
            "ADBE Shift Channels",
            "ADBE Glo2",
            "CC Particle World",
            "ADBE CurvesCustom",
            "ADBE Gaussian Blur",
            "ADBE Color Balance (HLS)",
            "ADBE Mirror",
            "ADBE Offset",
            "APC Colorama",
            "ADBE Fractal Noise",
            "ADBE Mosaic",
            "ADBE Fast Blur",
            "ADBE Stroke",
            "ADBE Point3D Control",
            "ADBE Motion Blur",
            "ADBE Tritone",
            "ADBE Venetian Blinds",
            "ADBE Easy Levels2",
            "ADBE Radial Blur",
        ] {
            assert!(effect_id(name).and_then(effectcraft_effects::find).is_some(), "{name}");
        }
    }
    #[test]
    fn converts_normalized_points_channel_order_percentage_and_nested_params() {
        let g = build(
            "ADBE Ramp",
            vec![
                param("ADBE Ramp-0001", "Start of Ramp", 6, Value::Vec2([0.25, 0.5])),
                param("ADBE Ramp-0002", "Start Color", 5, Value::Color([255.0, 128.0, 64.0, 32.0])),
            ],
        );
        assert_eq!(g.prop("start").unwrap().value, Value::Vec2([480.0, 540.0]));
        assert_eq!(g.prop("startColor").unwrap().value, Value::Color([128.0 / 255.0, 64.0 / 255.0, 32.0 / 255.0, 1.0]));
        assert_eq!(g.name, "My effect");
        assert!(!g.enabled);
        let g = build("ADBE Glo2", vec![param("", "Glow Threshold", 2, Value::Scalar(153.0))]);
        assert_eq!(g.prop("threshold").unwrap().value, Value::Scalar(60.0));
        let g = build("ADBE Fractal Noise", vec![param("", "Scale Width", 2, Value::Scalar(20.0))]);
        assert_eq!(g.prop("transform/scaleWidth").unwrap().value, Value::Scalar(20.0));
    }
    #[test]
    fn popup_labels_ignore_separator_slots() {
        let mut p = param("", "Blending Mode", 7, Value::Scalar(5.0));
        p.options = ["None", "Normal", "(-", "Add", "Multiply"].map(str::to_string).to_vec();
        let g = build("ADBE Fractal Noise", vec![p]);
        assert_eq!(g.prop("blendingMode").unwrap().value, Value::Enum(3));
    }
    #[test]
    fn decodes_pard_typed_defaults_without_native_default_substitution() {
        let mut bytes = vec![0; 148];
        bytes[56..60].copy_from_slice(&(32768u32).to_be_bytes());
        assert_eq!(default_value(2, &bytes), Some(Value::Scalar(0.5)));
        bytes[56..64].copy_from_slice(&1.5f64.to_be_bytes());
        assert_eq!(default_value(10, &bytes), Some(Value::Scalar(1.5)));
        assert_eq!(default_value(10, &bytes[..59]), None);
    }
    #[test]
    fn imports_colorama_palette_and_all_curve_channels() {
        let mut p = param("", "Output Cycle", 11, Value::Scalar(0.0));
        p.custom = vec![0; 520];
        p.custom[4..8].copy_from_slice(&[255, 255, 0, 0]);
        p.custom[8..12].copy_from_slice(&32768u32.to_be_bytes());
        p.custom[12..16].copy_from_slice(&[255, 0, 0, 255]);
        p.custom[512..516].copy_from_slice(&2u32.to_be_bytes());
        let g = build("APC Colorama", vec![p]);
        assert_eq!(g.prop("outputCycle/palette").unwrap().value, Value::Str("0:1,0,0,1 0.5:0,0,1,1".into()));
        let mut p = param("", "Curves", 11, Value::Scalar(0.0));
        p.custom = vec![0; 1284];
        for (i, v) in p.custom.iter_mut().skip(4).enumerate() {
            *v = (i % 256) as u8;
        }
        let g = build("ADBE CurvesCustom", vec![p]);
        assert!(matches!(&g.prop("alpha").unwrap().value,Value::Str(s) if s.ends_with("1,1")));
    }
    #[test]
    #[ignore = "requires a private local AEP via EFFECTCRAFT_AEP_TEST_FILE"]
    fn external_effect_adaptation() {
        let path = std::env::var("EFFECTCRAFT_AEP_TEST_FILE").unwrap();
        let bytes = std::fs::read(path).unwrap();
        let tree = super::super::parse_rifx(&bytes).unwrap();
        // The per-layer limit deliberately doesn't apply to the entire project's walk.
        fn layers(nodes: &[ChunkNode<'_>], definitions: &Definitions, result: &mut Vec<DecodedEffect>) {
            for n in nodes {
                if let ChunkNode::List { list_type, children, .. } = n {
                    if list_type == b"Layr" || list_type == b"AVLa" {
                        result.extend(decode(children, definitions).unwrap())
                    } else {
                        layers(children, definitions, result)
                    }
                }
            }
        }
        let mut decoded = Vec::new();
        let catalog = definitions(&tree).unwrap();
        layers(&tree, &catalog, &mut decoded);
        let mut next = 1;
        let (groups, mut warnings) = instantiate(&decoded, &mut Ids(&mut next), [1920.0, 1080.0], &HashMap::new()).unwrap();
        warnings.sort();
        warnings.dedup();
        eprintln!("Imported {} / {} effects. Warnings: {:#?}", groups.len(), decoded.len(), warnings);
        assert_eq!(groups.len(), decoded.len());
        assert!(!groups.is_empty());
        for (source, group) in decoded.iter().zip(&groups) {
            if source.match_name == "ADBE Gaussian Blur" {
                let raw = source.params.iter().find(|p| p.id.ends_with("-0001")).and_then(|p| p.stream.as_ref()).and_then(|s| s.value.as_ref());
                if let Some(raw) = raw {
                    assert_eq!(&group.prop("blurriness").unwrap().value, raw, "Gaussian values must survive the complete adapter");
                }
            }
        }
    }
}
