//! Vector contents and masks decoded from bounded AEP property trees.
use super::{ChunkNode, chunk_data, read_u32};
use effectcraft_keyframe::{Keyframe, ShapePath, Value};
use effectcraft_project::props::MaskMode;
use effectcraft_project::{Layer, LayerSource, Node, Project, PropGroup, build};

#[derive(Clone, Debug, Default)]
pub(super) struct DecodedShapes {
    groups: Vec<PropGroup>,
    warnings: Vec<String>,
}

pub(super) fn decode(nodes: &[ChunkNode<'_>]) -> Result<DecodedShapes, String> {
    let mut out = DecodedShapes::default();
    let mut counter = 1;
    scan(nodes, &mut build::Ids(&mut counter), &mut out, 0)?;
    Ok(out)
}

fn scan(nodes: &[ChunkNode<'_>], ids: &mut build::Ids<'_>, out: &mut DecodedShapes, depth: usize) -> Result<(), String> {
    if depth > 64 {
        return Err("AEP shape nesting exceeds 64 levels".into());
    }
    for (name, _, children) in entries(nodes) {
        match name.as_str() {
            "ADBE Root Vectors Group" => {
                let mut group = build::contents(ids);
                group.children = vector_children(children, ids, &mut out.warnings, depth + 1)?.into_iter().map(Into::into).collect();
                out.groups.push(group);
            }
            "ADBE Text Properties" => {
                if let Some(doc) = super::text::decode(children)? {
                    let mut group = build::text(ids, doc);
                    for (name, _, sub) in entries(children) {
                        if name == "ADBE Text More Options" {
                            if let Some(more) = group.group_mut("moreOptions") {
                                text_numeric(sub, more, &mut out.warnings)?;
                            }
                        } else if name == "ADBE Text Animators" {
                            let animators = text_animators(sub, ids, &mut out.warnings)?;
                            if let Some(target) = group.group_mut("animators") {
                                target.children = animators.into_iter().map(Into::into).collect();
                            }
                        }
                    }
                    out.groups.push(group);
                }
            }
            "ADBE Mask Parade" => {
                let mut group = build::masks(ids);
                for (name, metadata, children) in entries(children) {
                    if name == "ADBE Mask Atom" {
                        group.children.push(mask(children, metadata, ids, &mut out.warnings)?.into());
                    }
                }
                out.groups.push(group);
            }
            _ => scan(children, ids, out, depth + 1)?,
        }
    }
    Ok(())
}

/// A match name applies to the following list; auxiliary metadata can intervene.
fn entries<'a, 'b>(nodes: &'b [ChunkNode<'a>]) -> Vec<(String, Option<&'a [u8]>, &'b [ChunkNode<'a>])> {
    let mut name = String::new();
    let mut metadata = None;
    let mut out = Vec::new();
    for node in nodes {
        match node {
            ChunkNode::Chunk { id: [b't', b'd', b'm', b'n'], data } => {
                name = string(data);
                metadata = None;
            }
            ChunkNode::Chunk { id: [b'm', b'k', b'i', b'f'], data } => metadata = Some(*data),
            ChunkNode::List { children, .. } => out.push((name.clone(), metadata, children.as_slice())),
            _ => {}
        }
    }
    out
}

fn string(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes.split(|b| *b == 0).next().unwrap_or(&[])).into_owned()
}
fn title(nodes: &[ChunkNode<'_>], fallback: &str) -> String {
    chunk_data(nodes, b"tdsn").map(string).filter(|s| !s.is_empty()).unwrap_or_else(|| fallback.into())
}
fn enabled(nodes: &[ChunkNode<'_>]) -> bool {
    chunk_data(nodes, b"tdsb").and_then(|d| read_u32(d, 0)).is_none_or(|v| v & 1 != 0)
}

fn vector_children(nodes: &[ChunkNode<'_>], ids: &mut build::Ids<'_>, warnings: &mut Vec<String>, depth: usize) -> Result<Vec<PropGroup>, String> {
    if depth > 64 {
        return Err("AEP vector nesting exceeds 64 levels".into());
    }
    let mut out = Vec::new();
    for (name, _, children) in entries(nodes) {
        let mut group = match name.as_str() {
            "ADBE Vector Group" => {
                let mut items = Vec::new();
                for (name, _, sub) in entries(children) {
                    if name == "ADBE Vectors Group" {
                        items = vector_children(sub, ids, warnings, depth + 1)?;
                    }
                }
                let mut group = build::shape_group(ids, &title(children, "Group"), items);
                for (name, _, sub) in entries(children) {
                    if name == "ADBE Vector Transform Group"
                        && let Some(transform) = group.group_mut("transform")
                    {
                        apply_numeric(sub, transform, warnings)?;
                    }
                }
                group
            }
            "ADBE Vector Shape - Rect" => build::shape_rect(ids, [100.0; 2], [0.0; 2], 0.0),
            "ADBE Vector Shape - Ellipse" => build::shape_ellipse(ids, [100.0; 2], [0.0; 2]),
            "ADBE Vector Shape - Star" => build::shape_star(ids, true, 5.0, [0.0; 2], 100.0, 50.0),
            "ADBE Vector Graphic - Fill" => build::shape_fill(ids, [1.0, 0.0, 0.0, 1.0]),
            "ADBE Vector Graphic - Stroke" => build::shape_stroke(ids, [1.0; 4], 2.0),
            "ADBE Vector Filter - Trim" => build::shape_trim(ids, 0.0, 100.0, 0.0),
            "ADBE Vector Filter - Repeater" => build::shape_repeater(ids, 3.0, [100.0, 0.0]),
            "ADBE Vector Shape - Group" => {
                let path_nodes = entries(children).into_iter().find(|(n, _, _)| n == "ADBE Vector Shape").map(|(_, _, n)| n).unwrap_or(children);
                build::shape_path(ids, decode_path(path_nodes)?.unwrap_or_default())
            }
            _ => {
                if name.starts_with("ADBE Vector") && name != "ADBE Vector Materials Group" {
                    warnings.push(format!("Unsupported AEP vector operator: {name}"));
                }
                continue;
            }
        };
        group.name = title(children, &group.name);
        group.enabled = enabled(children);
        apply_numeric(children, &mut group, warnings)?;
        out.push(group);
    }
    Ok(out)
}

fn text_animators(nodes: &[ChunkNode<'_>], ids: &mut build::Ids<'_>, warnings: &mut Vec<String>) -> Result<Vec<PropGroup>, String> {
    let mut groups = Vec::new();
    for (name, _, children) in entries(nodes) {
        if name != "ADBE Text Animator" {
            continue;
        }
        let mut properties = Vec::new();
        let mut selectors = Vec::new();
        for (name, _, sub) in entries(children) {
            if name == "ADBE Text Animator Properties" {
                for (name, _, data) in entries(sub) {
                    let kind = match name.as_str() {
                        "ADBE Text Position 3D" => "position",
                        "ADBE Text Scale 3D" => "scale",
                        "ADBE Text Opacity" => "opacity",
                        "ADBE Text Blur" => "blur",
                        "ADBE Text Rotation" => "rotation",
                        "ADBE Text Anchor Point 3D" => "anchor",
                        _ => {
                            warnings.push(format!("Unsupported AEP text animator property: {name}"));
                            continue;
                        }
                    };
                    if let Some(mut property) = build::text_anim_prop(ids, kind) {
                        warnings.extend(super::streams::apply(data, &mut property)?);
                        properties.push(property);
                    }
                }
            } else if name == "ADBE Text Selectors" {
                for (name, _, data) in entries(sub) {
                    if name == "ADBE Text Selector" {
                        let mut selector = build::range_selector(ids, &title(data, "Range Selector"));
                        selector.enabled = enabled(data);
                        text_numeric(data, &mut selector, warnings)?;
                        selectors.push(selector);
                    }
                }
            }
        }
        let mut group = build::text_animator(ids, &title(children, "Animator"), properties);
        group.enabled = enabled(children);
        if !selectors.is_empty()
            && let Some(target) = group.group_mut("selectors")
        {
            target.children = selectors.into_iter().map(Into::into).collect();
        }
        groups.push(group);
    }
    Ok(groups)
}

fn text_numeric(nodes: &[ChunkNode<'_>], group: &mut PropGroup, warnings: &mut Vec<String>) -> Result<(), String> {
    // Advanced/Units determines which of the two stored range channels is active,
    // regardless of the order in which those streams occur in the file.
    if let Some(advanced) = group.group_mut("advanced") {
        for (name, _, children) in entries(nodes) {
            if name == "ADBE Text Range Advanced" {
                text_numeric(children, advanced, warnings)?;
            }
        }
    }
    let index_units = group.prop("advanced/units").is_some_and(|p| p.value.as_enum() == 1);
    for (name, _, children) in entries(nodes) {
        if name == "ADBE Text Range Advanced" {
            continue;
        }
        if (name.starts_with("ADBE Text Percent ") && index_units) || (name.starts_with("ADBE Text Index ") && !index_units) {
            continue;
        }
        let (path, enum_offset) = match name.as_str() {
            "ADBE Text Percent Start" | "ADBE Text Index Start" => ("start", false),
            "ADBE Text Percent End" | "ADBE Text Index End" => ("end", false),
            "ADBE Text Percent Offset" | "ADBE Text Index Offset" => ("offset", false),
            "ADBE Text Range Units" => ("units", true),
            "ADBE Text Selector Mode" => ("mode", true),
            "ADBE Text Selector Max Amount" => ("amount", false),
            "ADBE Text Range Shape" => ("shape", true),
            "ADBE Text Range Type2" => ("basedOn", true),
            "ADBE Text Selector Smoothness" => ("smoothness", false),
            "ADBE Text Levels Max Ease" => ("easeHigh", false),
            "ADBE Text Levels Min Ease" => ("easeLow", false),
            "ADBE Text Randomize Order" => ("randomize", false),
            "ADBE Text Random Seed" => ("randomSeed", false),
            "ADBE Text Anchor Point Align" => ("groupingAlignment", false),
            "ADBE Text Anchor Point Option" => ("anchorGrouping", true),
            _ => continue,
        };
        if let Some(property) = group.prop_mut(path) {
            warnings.extend(super::streams::apply(children, property)?);
            if enum_offset {
                for value in std::iter::once(&mut property.value).chain(property.keys.iter_mut().map(|k| &mut k.value)) {
                    if let Value::Enum(value) = value {
                        *value = value.saturating_sub(1);
                    }
                }
            }
        }
    }
    Ok(())
}

fn path_for(name: &str) -> Option<&'static str> {
    Some(match name {
        "ADBE Vector Rect Size" | "ADBE Vector Ellipse Size" => "size",
        "ADBE Vector Rect Position" | "ADBE Vector Ellipse Position" | "ADBE Vector Star Position" | "ADBE Vector Position" => "position",
        "ADBE Vector Rect Roundness" => "roundness",
        "ADBE Vector Star Type" => "type",
        "ADBE Vector Star Points" => "points",
        "ADBE Vector Star Rotation" | "ADBE Vector Rotation" => "rotation",
        "ADBE Vector Star Inner Radius" => "innerRadius",
        "ADBE Vector Star Outer Radius" => "outerRadius",
        "ADBE Vector Star Inner Roundess" => "innerRoundness",
        "ADBE Vector Star Outer Roundess" => "outerRoundness",
        "ADBE Vector Anchor" => "anchor",
        "ADBE Vector Scale" => "scale",
        "ADBE Vector Skew" => "skew",
        "ADBE Vector Skew Axis" => "skewAxis",
        "ADBE Vector Fill Color" | "ADBE Vector Stroke Color" => "color",
        "ADBE Vector Fill Opacity" | "ADBE Vector Stroke Opacity" | "ADBE Vector Group Opacity" => "opacity",
        "ADBE Vector Stroke Width" => "width",
        "ADBE Vector Stroke Line Cap" => "cap",
        "ADBE Vector Stroke Line Join" => "join",
        "ADBE Vector Stroke Miter Limit" => "miter",
        "ADBE Vector Stroke Dash 1" => "dash",
        "ADBE Vector Stroke Gap 1" => "gap",
        "ADBE Vector Stroke Offset" => "offset",
        "ADBE Vector Trim Start" => "start",
        "ADBE Vector Trim End" => "end",
        "ADBE Vector Trim Offset" => "offset",
        "ADBE Mask Feather" => "feather",
        "ADBE Mask Opacity" => "opacity",
        "ADBE Mask Offset" => "expansion",
        _ => return None,
    })
}

fn apply_numeric(nodes: &[ChunkNode<'_>], group: &mut PropGroup, warnings: &mut Vec<String>) -> Result<(), String> {
    for (name, _, children) in entries(nodes) {
        if name == "ADBE Vector Stroke Dashes" {
            if let Some(dashes) = group.group_mut("dashes") {
                apply_numeric(children, dashes, warnings)?;
            }
        } else if let Some(path) = path_for(&name)
            && let Some(property) = group.prop_mut(path)
        {
            warnings.extend(super::streams::apply(children, property)?);
            if matches!(name.as_str(), "ADBE Vector Fill Color" | "ADBE Vector Stroke Color") {
                fn convert(value: &mut Value) {
                    if let Value::Color(c) = value {
                        *c = [c[1] / 255.0, c[2] / 255.0, c[3] / 255.0, c[0] / 255.0];
                    }
                }
                convert(&mut property.value);
                for key in &mut property.keys {
                    convert(&mut key.value);
                }
            }
            if matches!(name.as_str(), "ADBE Vector Star Type" | "ADBE Vector Stroke Line Cap" | "ADBE Vector Stroke Line Join") {
                if let Value::Enum(value) = &mut property.value {
                    *value = value.saturating_sub(1);
                }
                for key in &mut property.keys {
                    if let Value::Enum(value) = &mut key.value {
                        *value = value.saturating_sub(1);
                    }
                }
            }
        }
    }
    Ok(())
}

fn mask(nodes: &[ChunkNode<'_>], metadata: Option<&[u8]>, ids: &mut build::Ids<'_>, warnings: &mut Vec<String>) -> Result<PropGroup, String> {
    let mut path = None;
    let mut keys = Vec::new();
    for (name, _, children) in entries(nodes) {
        if name == "ADBE Mask Shape" {
            path = decode_path(children)?;
            let stream = children.iter().find_map(|n| match n {
                ChunkNode::List { list_type: [b't', b'd', b'b', b's'], children, .. } => Some(children.as_slice()),
                _ => None,
            });
            if let Some(stream) = stream {
                let times = super::streams::key_times(stream)?;
                if !times.is_empty() {
                    let shapes = children
                        .iter()
                        .find_map(|n| match n {
                            ChunkNode::List { list_type: [b'o', b'm', b'k', b's'], children, .. } => Some(children.as_slice()),
                            _ => None,
                        })
                        .ok_or("missing AEP animated mask shapes")?;
                    let shapes: Vec<_> = shapes.iter().filter(|n| matches!(n, ChunkNode::List { list_type, .. } if list_type == b"shap")).collect();
                    if shapes.len() != times.len() {
                        return Err("AEP mask keyframe and shape counts differ".into());
                    }
                    for (node, (time, incoming, outgoing)) in shapes.into_iter().zip(times) {
                        let path = decode_path(std::slice::from_ref(node))?.ok_or("missing AEP animated path")?;
                        let mut key = Keyframe::new(time, Value::Path(path));
                        key.in_interp = incoming;
                        key.out_interp = outgoing;
                        keys.push(key);
                    }
                }
            }
        }
    }
    let mode = match metadata.and_then(|d| read_u32(d, 4)).unwrap_or(1) {
        0 => MaskMode::None,
        1 => MaskMode::Add,
        2 => MaskMode::Subtract,
        3 => MaskMode::Intersect,
        4 => MaskMode::Lighten,
        5 => MaskMode::Darken,
        6 => MaskMode::Difference,
        _ => return Err("unsupported AEP mask mode".into()),
    };
    let mut group = build::mask(ids, &title(nodes, "Mask"), path.unwrap_or_default(), mode, [177, 180, 79]);
    // The mask information's first byte is the Inverted switch.
    if let effectcraft_project::GroupKind::Mask { inverted, .. } = &mut group.kind {
        *inverted = metadata.and_then(|d| d.first()).is_some_and(|b| *b != 0);
    }
    group.enabled = enabled(nodes);
    if let Some(property) = group.prop_mut("path") {
        property.keys = keys;
    }
    apply_numeric(nodes, &mut group, warnings)?;
    Ok(group)
}

fn number(data: &[u8], offset: usize) -> Result<f64, String> {
    let bytes = data.get(offset..offset.saturating_add(4)).and_then(|s| s.try_into().ok()).ok_or("truncated AEP path coordinate")?;
    let value = f64::from(f32::from_be_bytes(bytes));
    if !value.is_finite() || value.abs() > 1.0e12 {
        return Err("invalid AEP path coordinate".into());
    }
    Ok(value)
}

fn decode_path(nodes: &[ChunkNode<'_>]) -> Result<Option<ShapePath>, String> {
    fn find<'a, 'b>(nodes: &'b [ChunkNode<'a>], depth: usize) -> Option<&'b [ChunkNode<'a>]> {
        if depth > 64 {
            return None;
        }
        for node in nodes {
            if let ChunkNode::List { list_type, children, .. } = node {
                if list_type == b"shap" {
                    return Some(children);
                }
                if let Some(found) = find(children, depth + 1) {
                    return Some(found);
                }
            }
        }
        None
    }
    let Some(shape) = find(nodes, 0) else { return Ok(None) };
    let h = chunk_data(shape, b"shph").ok_or("missing AEP shape header")?.get(..24).ok_or("truncated AEP shape header")?;
    let bounds = [number(h, 4)?, number(h, 8)?, number(h, 12)?, number(h, 16)?];
    let data = shape
        .iter()
        .find_map(|n| match n {
            ChunkNode::List { list_type: [b'l', b'i', b's', b't'], children, .. } => chunk_data(children, b"ldat"),
            _ => None,
        })
        .ok_or("missing AEP shape vertices")?;
    if !data.len().is_multiple_of(24) || data.len() > 24 * 65536 {
        return Err("invalid AEP shape vertex count".into());
    }
    let mut points = Vec::new();
    for bytes in data.as_chunks::<8>().0 {
        points.push([bounds[0] + number(bytes, 0)? * (bounds[2] - bounds[0]), bounds[1] + number(bytes, 4)? * (bounds[3] - bounds[1])]);
    }
    let closed = h.get(20).copied().ok_or("truncated AEP shape flags")? != 0;
    let mut path = ShapePath { closed, ..Default::default() };
    let n = points.len();
    for (i, triple) in points.as_chunks::<3>().0.iter().enumerate() {
        let vertex = *triple.first().ok_or("missing AEP vertex")?;
        let outgoing = *triple.get(1).ok_or("missing AEP tangent")?;
        let incoming_index = if i == 0 { n.saturating_sub(1) } else { i.saturating_mul(3).saturating_sub(1) };
        let incoming = points.get(incoming_index).copied().unwrap_or(vertex);
        path.vertices.push(vertex);
        path.out_tangents.push([outgoing[0] - vertex[0], outgoing[1] - vertex[1]]);
        path.in_tangents.push([incoming[0] - vertex[0], incoming[1] - vertex[1]]);
    }
    Ok(Some(path))
}

pub(super) fn apply(decoded: &DecodedShapes, project: &mut Project, layer: &mut Layer, size: (u32, u32)) -> Result<Vec<String>, String> {
    fn assign(group: &mut PropGroup, project: &mut Project, scale: [f64; 2], depth: usize) -> Result<(), String> {
        if depth > 64 {
            return Err("AEP property nesting exceeds 64 levels".into());
        }
        project.next_id.checked_add(1).ok_or("AEP property ID overflow")?;
        group.uid = project.alloc();
        for node in &mut group.children {
            match node {
                Node::Group(group) => assign(group, project, scale, depth + 1)?,
                Node::Prop(property) => {
                    project.next_id.checked_add(1).ok_or("AEP property ID overflow")?;
                    property.uid = project.alloc();
                    for value in std::iter::once(&mut property.value).chain(property.keys.iter_mut().map(|k| &mut k.value)) {
                        if let Value::Path(path) = value {
                            for point in path.vertices.iter_mut().chain(&mut path.in_tangents).chain(&mut path.out_tangents) {
                                point[0] *= scale[0];
                                point[1] *= scale[1];
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
    for group in &decoded.groups {
        let mut group = group.clone();
        let normalized = group.match_id == "masks" && !matches!(layer.source, LayerSource::Shape | LayerSource::Text);
        let scale = if normalized { [f64::from(size.0), f64::from(size.1)] } else { [1.0; 2] };
        assign(&mut group, project, scale, 0)?;
        layer.props.children.retain(|n| n.match_id() != group.match_id);
        layer.props.children.push(group.into());
    }
    Ok(decoded.warnings.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn chunk<'a>(id: &[u8; 4], data: &'a [u8]) -> ChunkNode<'a> {
        ChunkNode::Chunk { id: *id, data }
    }
    fn list<'a>(kind: &[u8; 4], children: Vec<ChunkNode<'a>>) -> ChunkNode<'a> {
        ChunkNode::List { id: *b"LIST", list_type: *kind, children }
    }
    #[test]
    fn text_range_preserves_advanced_controls_and_active_index_channels() {
        let values: Vec<_> = [2.0f64, 3.0, -40.0, 4.0, 6.0, 25.0, -60.0, 70.0, 1.0, 19.0, 2.0, 7.0, -1.0, 90.0].map(f64::to_be_bytes).into();
        let advanced_names: [&[u8]; 10] = [
            b"ADBE Text Range Units",
            b"ADBE Text Selector Mode",
            b"ADBE Text Selector Max Amount",
            b"ADBE Text Range Type2",
            b"ADBE Text Range Shape",
            b"ADBE Text Selector Smoothness",
            b"ADBE Text Levels Max Ease",
            b"ADBE Text Levels Min Ease",
            b"ADBE Text Randomize Order",
            b"ADBE Text Random Seed",
        ];
        let mut advanced = Vec::new();
        for (name, value) in advanced_names.iter().zip(&values) {
            advanced.extend([chunk(b"tdmn", name), list(b"tdbs", vec![chunk(b"cdat", value)])]);
        }
        let mut nodes = Vec::new();
        for (name, value) in
            [b"ADBE Text Index Start".as_slice(), b"ADBE Text Index End", b"ADBE Text Index Offset", b"ADBE Text Percent Start"].iter().zip(&values[10..])
        {
            nodes.extend([chunk(b"tdmn", name), list(b"tdbs", vec![chunk(b"cdat", value)])]);
        }
        nodes.extend([chunk(b"tdmn", b"ADBE Text Range Advanced"), list(b"tdgp", advanced)]);
        let mut counter = 1;
        let mut group = build::range_selector(&mut build::Ids(&mut counter), "Range");
        let mut warnings = Vec::new();
        text_numeric(&nodes, &mut group, &mut warnings).unwrap();
        for (path, want) in [
            ("start", Value::Scalar(2.0)),
            ("end", Value::Scalar(7.0)),
            ("offset", Value::Scalar(-1.0)),
            ("advanced/units", Value::Enum(1)),
            ("advanced/mode", Value::Enum(2)),
            ("advanced/basedOn", Value::Enum(3)),
            ("advanced/shape", Value::Enum(5)),
            ("advanced/amount", Value::Scalar(-40.0)),
            ("advanced/smoothness", Value::Scalar(25.0)),
            ("advanced/easeHigh", Value::Scalar(-60.0)),
            ("advanced/easeLow", Value::Scalar(70.0)),
            ("advanced/randomize", Value::Bool(true)),
            ("advanced/randomSeed", Value::Scalar(19.0)),
        ] {
            assert_eq!(group.prop(path).unwrap().value, want, "{path}");
        }
        assert!(warnings.is_empty());
        let mut disabled = vec![chunk(b"tdsb", &[0, 0, 0, 0])];
        disabled.extend(nodes);
        let animators = vec![
            chunk(b"tdmn", b"ADBE Text Animator"),
            list(b"tdgp", vec![chunk(b"tdmn", b"ADBE Text Selectors"), list(b"tdgp", vec![chunk(b"tdmn", b"ADBE Text Selector"), list(b"tdgp", disabled)])]),
        ];
        let decoded = text_animators(&animators, &mut build::Ids(&mut counter), &mut warnings).unwrap();
        let selector = decoded[0].sub("selectors").unwrap().groups().next().unwrap();
        assert!(!selector.enabled);
        assert_eq!(selector.prop("advanced/units").unwrap().value, Value::Enum(1));
    }
    #[test]
    fn vector_primitives_preserve_disabled_fill_color_and_size() {
        let size: Vec<_> = [240f64, 120.0].into_iter().flat_map(f64::to_be_bytes).collect();
        let color: Vec<_> = [255f64, 128.0, 64.0, 32.0].into_iter().flat_map(f64::to_be_bytes).collect();
        let nodes = vec![
            chunk(b"tdmn", b"ADBE Root Vectors Group"),
            list(
                b"tdgp",
                vec![
                    chunk(b"tdmn", b"ADBE Vector Shape - Rect"),
                    list(b"tdgp", vec![chunk(b"tdmn", b"ADBE Vector Rect Size"), list(b"tdbs", vec![chunk(b"cdat", &size)])]),
                    chunk(b"tdmn", b"ADBE Vector Graphic - Fill"),
                    list(b"tdgp", vec![chunk(b"tdsb", &[0, 0, 0, 0]), chunk(b"tdmn", b"ADBE Vector Fill Color"), list(b"tdbs", vec![chunk(b"cdat", &color)])]),
                ],
            ),
        ];
        let decoded = decode(&nodes).unwrap();
        let group = &decoded.groups[0];
        let rect = group.children[0].as_group().unwrap();
        assert_eq!(rect.prop("size").unwrap().value, Value::Vec2([240.0, 120.0]));
        let fill = group.children[1].as_group().unwrap();
        assert!(!fill.enabled);
        assert_eq!(fill.prop("color").unwrap().value, Value::Color([128.0 / 255.0, 64.0 / 255.0, 32.0 / 255.0, 1.0]));
    }
    #[test]
    fn path_segments_become_relative_bezier_tangents() {
        let mut header = vec![0; 24];
        for (offset, number) in [(4, 10f32), (8, 20.0), (12, 110.0), (16, 220.0)] {
            header[offset..offset + 4].copy_from_slice(&number.to_be_bytes());
        }
        header[20] = 1;
        let data: Vec<_> = [0f32, 0.0, 0.25, 0.0, 1.0, 0.75, 1.0, 1.0, 0.75, 1.0, 0.0, 0.25].into_iter().flat_map(f32::to_be_bytes).collect();
        let nodes = vec![list(b"shap", vec![chunk(b"shph", &header), list(b"list", vec![chunk(b"ldat", &data)])])];
        let path = decode_path(&nodes).unwrap().unwrap();
        assert_eq!(path.vertices, [[10.0, 20.0], [110.0, 220.0]]);
        assert_eq!(path.out_tangents, [[25.0, 0.0], [-25.0, 0.0]]);
        assert_eq!(path.in_tangents, [[0.0, 50.0], [0.0, -50.0]]);
        assert!(path.closed);
        for length in 0..24 {
            let nodes = vec![list(b"shap", vec![chunk(b"shph", &header[..length]), list(b"list", vec![chunk(b"ldat", &data)])])];
            assert!(decode_path(&nodes).is_err());
        }
        let mut invalid = data.clone();
        invalid[..4].copy_from_slice(&f32::NAN.to_be_bytes());
        let nodes = vec![list(b"shap", vec![chunk(b"shph", &header), list(b"list", vec![chunk(b"ldat", &invalid)])])];
        assert!(decode_path(&nodes).is_err());
    }
    #[test]
    fn mask_mode_is_independent_of_its_source_identity() {
        let mut metadata = [0; 12];
        metadata[4..8].copy_from_slice(&2u32.to_be_bytes());
        metadata[8..12].copy_from_slice(&37u32.to_be_bytes());
        let mut counter = 1;
        let group = mask(&[], Some(&metadata), &mut build::Ids(&mut counter), &mut Vec::new()).unwrap();
        assert!(matches!(group.kind, effectcraft_project::GroupKind::Mask { mode: MaskMode::Subtract, inverted: false, .. }));
    }
    #[test]
    fn mask_keeps_its_inverted_switch() {
        let mut metadata = [0; 12];
        metadata[0] = 1;
        metadata[4..8].copy_from_slice(&1u32.to_be_bytes());
        let mut counter = 1;
        let group = mask(&[], Some(&metadata), &mut build::Ids(&mut counter), &mut Vec::new()).unwrap();
        assert!(matches!(group.kind, effectcraft_project::GroupKind::Mask { mode: MaskMode::Add, inverted: true, .. }));
    }
    #[test]
    fn applying_masks_replaces_builder_group_and_scales_static_and_animated_paths() {
        let mut project = Project::default();
        let comp = effectcraft_project::Comp::new(200, 100, effectcraft_time::FrameRate::from_f64(24.0), effectcraft_time::Tick::from_seconds_f64(1.0));
        let mut layer = build::layer(&mut project, &comp, "Image", LayerSource::Footage { item: effectcraft_project::ItemId(1000) }, (200, 100), None);
        let mut counter = 1;
        let mut ids = build::Ids(&mut counter);
        let path = ShapePath::polygon(&[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0]], true);
        let mut mask = build::mask(&mut ids, "Mask", path.clone(), MaskMode::Add, [0; 3]);
        mask.prop_mut("path").unwrap().keys.push(Keyframe::new(effectcraft_time::Tick::ZERO, Value::Path(path)));
        let group = build::masks(&mut ids).with(mask);
        apply(&DecodedShapes { groups: vec![group], warnings: vec![] }, &mut project, &mut layer, (200, 100)).unwrap();
        assert_eq!(layer.props.children.iter().filter(|n| n.match_id() == "masks").count(), 1);
        let mask = layer.masks().unwrap().groups().next().unwrap();
        let property = mask.prop("path").unwrap();
        let Value::Path(path) = &property.value else { panic!("missing path") };
        assert_eq!(path.vertices, [[0.0, 0.0], [200.0, 0.0], [200.0, 100.0]]);
        assert_eq!(property.value, property.keys[0].value);
    }
    #[test]
    #[ignore = "set EFFECTCRAFT_AEP_TEST_FILE to an external local project"]
    fn external_vector_and_text_documents() {
        let path = std::env::var("EFFECTCRAFT_AEP_TEST_FILE").unwrap();
        let bytes = std::fs::read(path).unwrap();
        let nodes = super::super::parse_rifx(&bytes).unwrap();
        let decoded = decode(&nodes).unwrap();
        let text: Vec<_> = decoded.groups.iter().filter_map(|g| g.prop("sourceText")).collect();
        eprintln!("decoded {} property groups, {} text documents, warnings {:?}", decoded.groups.len(), text.len(), decoded.warnings);
        for prop in text {
            if let Value::Text(doc) = &prop.value {
                eprintln!("{} | {} {} {:?}", doc.text, doc.font, doc.size, doc.justify);
            }
        }
        assert!(!decoded.groups.is_empty());
    }
}
