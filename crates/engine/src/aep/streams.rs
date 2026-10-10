//! Numeric AEP property streams, observed from contributor-provided project files.
//!
//! RIFX numeric streams are big endian. The opaque orientation payload is explicitly little
//! endian. `tdb4` carries the stream's integer timebase; times are never inferred from frame rate.
//! This module does not interpret plug-in-owned custom binary values.

use super::ChunkNode;
use effectcraft_keyframe::{Ease, Interp, Keyframe, Value};
use effectcraft_project::Layer;
use effectcraft_project::build::Ids;
use effectcraft_project::props::{Expression, Node, Property};
use effectcraft_time::Tick;

const MAX_KEYS: usize = 100_000;
const MAX_EXPRESSION: usize = 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub(crate) struct DecodedStream {
    pub value: Option<Value>,
    pub keys: Vec<Keyframe>,
    pub spatial: bool,
    pub expression: Option<Expression>,
    pub warnings: Vec<String>,
}

impl DecodedStream {
    /// Convert raw channel order/units, including every key's value. Temporal speeds and spatial
    /// tangents need the caller's corresponding unit conversion when changing distance units.
    pub fn map_values(&mut self, mut map: impl FnMut(&Value) -> Value) {
        if let Some(value) = &mut self.value {
            *value = map(value);
        }
        for key in &mut self.keys {
            key.value = map(&key.value);
        }
    }

    pub fn install(&self, property: &mut Property) {
        if let Some(value) = &self.value {
            property.value = value.clone();
        }
        property.keys.clone_from(&self.keys);
        // Text animator Position and effect points are not spatial in every
        // native builder. The stream's motion-path flag must accompany its
        // tangents, or evaluation silently interpolates independent axes.
        property.spatial |= self.spatial;
        property.expr.clone_from(&self.expression);
    }

    fn multiply(&mut self, factor: f64) {
        self.map_values(|value| value.with_components(&value.components().iter().map(|v| v * factor).collect::<Vec<_>>()));
        for key in &mut self.keys {
            for ease in key.in_ease.iter_mut().chain(&mut key.out_ease) {
                ease.speed *= factor;
            }
            for tangent in key.spatial_in.iter_mut().chain(&mut key.spatial_out) {
                *tangent *= factor;
            }
        }
    }
}

fn chunk<'a>(nodes: &'a [ChunkNode<'a>], id: &[u8; 4]) -> Option<&'a [u8]> {
    nodes.iter().find_map(|node| match node {
        ChunkNode::Chunk { id: found, data } if found == id => Some(*data),
        _ => None,
    })
}
fn u16_at(data: &[u8], offset: usize) -> Result<u16, String> {
    Ok(u16::from_be_bytes(data.get(offset..offset.saturating_add(2)).and_then(|v| v.try_into().ok()).ok_or("truncated AEP stream integer")?))
}
fn u32_at(data: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_be_bytes(data.get(offset..offset.saturating_add(4)).and_then(|v| v.try_into().ok()).ok_or("truncated AEP stream integer")?))
}
fn number(data: &[u8], offset: usize, little: bool) -> Result<f64, String> {
    let bytes = data.get(offset..offset.saturating_add(8)).and_then(|v| v.try_into().ok()).ok_or("truncated AEP numeric stream")?;
    let value = if little { f64::from_le_bytes(bytes) } else { f64::from_be_bytes(bytes) };
    if !value.is_finite() || value.abs() > 1.0e12 {
        return Err("invalid or excessive AEP numeric stream value".into());
    }
    Ok(value)
}
fn components(data: &[u8], offset: usize, count: usize, little: bool) -> Result<Vec<f64>, String> {
    (0..count).map(|i| number(data, offset.saturating_add(i.saturating_mul(8)), little)).collect()
}
fn interpolation(code: u8) -> Result<Interp, String> {
    match code {
        1 => Ok(Interp::Linear),
        2 => Ok(Interp::Bezier),
        3 => Ok(Interp::Hold),
        _ => Err(format!("unsupported AEP keyframe interpolation {code}")),
    }
}
fn ease(data: &[u8], speed: usize, influence: usize) -> Result<Ease, String> {
    let speed = number(data, speed, false)?;
    let influence = number(data, influence, false)?;
    if !(0.0..=1.0).contains(&influence) {
        return Err("AEP keyframe influence is outside 0..1".into());
    }
    Ok(Ease { speed, influence })
}

/// Decode a numeric `tdbs` list in its original units. The supplied value selects the output
/// variant; colors remain in their original channel order until the importing caller converts it.
pub(crate) fn decode(nodes: &[ChunkNode<'_>], template: &Value) -> Result<DecodedStream, String> {
    let dimensions = template.dims();
    if !(1..=4).contains(&dimensions) {
        return Err("AEP stream requires a numeric property template".into());
    }
    let header = chunk(nodes, b"tdb4");
    let kind = header.map(|h| u16_at(h, 4)).transpose()?.unwrap_or(0);
    let mut decoded = DecodedStream { spatial: kind & 8 != 0 && (2..=3).contains(&dimensions), ..DecodedStream::default() };
    // Orientation's custom stream advertises one opaque item, containing three LE doubles.
    let orientation = kind == 7 && dimensions == 3 && header.is_some_and(|h| u16_at(h, 8).ok() == Some(6));
    if let Some(data) = chunk(nodes, b"cdat") {
        if data.len() < dimensions * 8 {
            return Err("AEP custom property data cannot be decoded as numeric channels".into());
        }
        decoded.value = Some(template.with_components(&components(data, 0, dimensions, orientation)?));
    }
    if let Some(data) = chunk(nodes, b"expr") {
        if data.len() > MAX_EXPRESSION {
            return Err("AEP expression exceeds 1 MiB".into());
        }
        let bytes = data.split(|b| *b == 0).next().unwrap_or(&[]);
        let text = std::str::from_utf8(bytes).map_err(|_| "AEP expression is not valid UTF-8")?.to_string();
        if !text.trim().is_empty() {
            // In observed streams byte 120 marks an expression and byte 119 marks its
            // disabled/error state. Preserve that state rather than reactivating a broken
            // or intentionally disabled expression while opening the document.
            let enabled = !header.is_some_and(|header| header.get(119).is_some_and(|flag| *flag != 0));
            if !enabled {
                decoded.warnings.push("An AEP expression stored in a disabled/error state was preserved disabled".into());
            }
            decoded.expression = Some(Expression { text, enabled });
        }
    }
    for node in nodes {
        let ChunkNode::List { list_type, children, .. } = node else { continue };
        if list_type != b"list" {
            continue;
        }
        let Some(list_header) = chunk(children, b"lhd3") else { continue };
        if !decoded.keys.is_empty() {
            return Err("multiple AEP keyframe lists in a numeric property".into());
        }
        let data = chunk(children, b"ldat").ok_or("missing AEP keyframe data")?;
        let count = u32_at(list_header, 8)? as usize;
        let size = u32_at(list_header, 16)? as usize;
        if count > MAX_KEYS {
            return Err("AEP property exceeds 100000 keyframes".into());
        }
        let header = header.ok_or("AEP animated property has no stream header")?;
        let timebase = u32_at(header, 12)?;
        if timebase == 0 {
            return Err("AEP keyframe timebase is zero".into());
        }
        let stored_dimensions = usize::from(u16_at(header, 2)?);
        if stored_dimensions != dimensions {
            return Err(format!("AEP animated property has {stored_dimensions} channels, expected {dimensions}"));
        }
        let spatial = kind & 8 != 0;
        let extended = kind & 6 == 6;
        let expected_size = if extended { 56 + dimensions * 24 } else { 8 + dimensions * 40 };
        if size != expected_size {
            return Err(format!("unsupported AEP keyframe record size {size} (expected {expected_size})"));
        }
        let length = count.checked_mul(size).ok_or("AEP keyframe byte count overflow")?;
        if data.len() != length {
            return Err("AEP keyframe count does not match its data length".into());
        }
        for row in data.chunks_exact(size) {
            let raw_time = u32_at(row, 0)? as i32;
            let time = Tick::from_units(i64::from(raw_time), i64::from(timebase));
            if decoded.keys.last().is_some_and(|key| key.time >= time) {
                return Err("AEP keyframe times are not strictly increasing".into());
            }
            let value_offset = if extended { 56 } else { 8 };
            let value = template.with_components(&components(row, value_offset, dimensions, false)?);
            let mut key = Keyframe::new(time, value);
            key.in_interp = interpolation(*row.get(4).ok_or("missing AEP incoming interpolation")?)?;
            key.out_interp = interpolation(*row.get(5).ok_or("missing AEP outgoing interpolation")?)?;
            let flags = u16_at(row, 6)?;
            key.continuous = flags & 8 != 0;
            key.spatial_auto = false;
            if extended {
                key.in_ease.push(ease(row, 24, 32)?);
                key.out_ease.push(ease(row, 40, 48)?);
                if spatial {
                    for (axis, component) in key.spatial_in.iter_mut().take(dimensions).enumerate() {
                        *component = number(row, value_offset + dimensions * 8 + axis * 8, false)?;
                    }
                    for (axis, component) in key.spatial_out.iter_mut().take(dimensions).enumerate() {
                        *component = number(row, value_offset + dimensions * 16 + axis * 8, false)?;
                    }
                }
            } else {
                for axis in 0..dimensions {
                    key.in_ease.push(ease(row, 8 + dimensions * 8 + axis * 8, 8 + dimensions * 16 + axis * 8)?);
                    key.out_ease.push(ease(row, 8 + dimensions * 24 + axis * 8, 8 + dimensions * 32 + axis * 8)?);
                }
            }
            decoded.keys.push(key);
        }
    }
    if decoded.value.is_none() {
        decoded.value = decoded.keys.first().map(|key| key.value.clone());
    }
    Ok(decoded)
}

/// Read only timing/interpolation for opaque path streams. Their payload can contain native
/// pointer-shaped tokens, so callers associate the separately stored shape records by key order.
pub(crate) fn key_times(nodes: &[ChunkNode<'_>]) -> Result<Vec<(Tick, Interp, Interp)>, String> {
    let mut result = Vec::new();
    for node in nodes {
        let ChunkNode::List { list_type, children, .. } = node else { continue };
        if list_type != b"list" {
            continue;
        }
        let Some(header) = chunk(children, b"lhd3") else { continue };
        let data = chunk(children, b"ldat").ok_or("missing AEP path keyframe data")?;
        let count = u32_at(header, 8)? as usize;
        let size = u32_at(header, 16)? as usize;
        if count > MAX_KEYS || !(8..=4096).contains(&size) {
            return Err("invalid AEP path keyframe count or size".into());
        }
        if count.checked_mul(size) != Some(data.len()) {
            return Err("AEP path keyframe count does not match data".into());
        }
        let stream = chunk(nodes, b"tdb4").ok_or("missing AEP path stream header")?;
        let timebase = u32_at(stream, 12)?;
        if timebase == 0 {
            return Err("AEP path keyframe timebase is zero".into());
        }
        for row in data.chunks_exact(size) {
            let time = Tick::from_units(i64::from(u32_at(row, 0)? as i32), i64::from(timebase));
            if result.last().is_some_and(|(last, _, _)| *last >= time) {
                return Err("AEP path keyframe times are not increasing".into());
            }
            result.push((
                time,
                interpolation(*row.get(4).ok_or("missing AEP interpolation")?)?,
                interpolation(*row.get(5).ok_or("missing AEP interpolation")?)?,
            ));
        }
    }
    Ok(result)
}

pub(crate) fn apply(nodes: &[ChunkNode<'_>], property: &mut Property) -> Result<Vec<String>, String> {
    let decoded = decode(nodes, &property.value)?;
    decoded.install(property);
    Ok(decoded.warnings)
}

#[derive(Clone, Debug)]
pub(crate) struct DecodedTransform {
    pub path: String,
    pub stream: DecodedStream,
}

pub(crate) fn collect_transforms(nodes: &[ChunkNode<'_>]) -> Result<Vec<DecodedTransform>, String> {
    let mut result = Vec::new();
    collect_transform_groups(nodes, 0, &mut result)?;
    Ok(result)
}
fn collect_transform_groups(nodes: &[ChunkNode<'_>], depth: usize, result: &mut Vec<DecodedTransform>) -> Result<(), String> {
    if depth > 64 {
        return Err("AEP transform nesting exceeds 64 levels".into());
    }
    let mut name = "";
    for node in nodes {
        match node {
            ChunkNode::Chunk { id: [b't', b'd', b'm', b'n'], data } => {
                name = std::str::from_utf8(data).unwrap_or("").trim_end_matches('\0');
            }
            ChunkNode::List { list_type: [b't', b'd', b'b', b's'], children, .. } if name == "ADBE Time Remapping" => {
                if chunk(children, b"tdsb").is_some_and(|flags| u32_at(flags, 0).is_ok_and(|flags| flags & 2 != 0)) {
                    continue;
                }
                result.push(DecodedTransform { path: "timeRemap".into(), stream: decode(children, &Value::Scalar(0.0))? });
            }
            ChunkNode::List { list_type: [b't', b'd', b'g', b'p'], children, .. } => {
                if matches!(name, "ADBE Transform Group" | "ADBE Camera Options Group" | "ADBE Light Options Group" | "ADBE Material Options Group") {
                    collect_transform_properties(children, 0, "", result)?;
                } else {
                    collect_transform_groups(children, depth + 1, result)?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}
fn collect_transform_properties(nodes: &[ChunkNode<'_>], depth: usize, inherited: &str, result: &mut Vec<DecodedTransform>) -> Result<(), String> {
    if depth > 64 {
        return Err("AEP transform property nesting exceeds 64 levels".into());
    }
    let mut name = inherited;
    for node in nodes {
        match node {
            ChunkNode::Chunk { id: [b't', b'd', b'm', b'n'], data } => {
                name = std::str::from_utf8(data).unwrap_or("").trim_end_matches('\0');
            }
            ChunkNode::List { list_type: [b't', b'd', b'b', b's'], children, .. } => {
                let path = match name {
                    "ADBE Anchor Point" => "anchor",
                    "ADBE Position" => "position",
                    "ADBE Position_0" => "positionX",
                    "ADBE Position_1" => "positionY",
                    "ADBE Position_2" => "positionZ",
                    "ADBE Point of Interest" => "poi",
                    "ADBE Scale" => "scale",
                    "ADBE Orientation" => "orientation",
                    "ADBE Rotate X" => "rotationX",
                    "ADBE Rotate Y" => "rotationY",
                    "ADBE Rotate Z" => "rotation",
                    "ADBE Opacity" => "opacity",
                    "ADBE Camera Zoom" => "cameraOptions/zoom",
                    "ADBE Camera Focus Distance" => "cameraOptions/focusDistance",
                    "ADBE Camera Aperture" => "cameraOptions/aperture",
                    "ADBE Camera Depth of Field" => "cameraOptions/dof",
                    "ADBE Camera Blur Level" => "cameraOptions/blurLevel",
                    "ADBE Light Intensity" => "lightOptions/intensity",
                    "ADBE Light Color" => "lightOptions/color",
                    "ADBE Light Cone Angle" => "lightOptions/coneAngle",
                    "ADBE Light Cone Feather 2" => "lightOptions/coneFeather",
                    "ADBE Ambient Coefficient" => "materialOptions/ambient",
                    "ADBE Diffuse Coefficient" => "materialOptions/diffuse",
                    "ADBE Specular Coefficient" => "materialOptions/specularIntensity",
                    "ADBE Shininess Coefficient" => "materialOptions/specularShininess",
                    "ADBE Metal Coefficient" => "materialOptions/metal",
                    _ => continue,
                };
                // The file also stores dormant separated channels for ordinary combined
                // Position. Bit 1 marks those followers inactive; installing them would
                // incorrectly override the live combined motion path in the renderer.
                if matches!(path, "positionX" | "positionY" | "positionZ")
                    && chunk(children, b"tdsb").is_some_and(|flags| u32_at(flags, 0).is_ok_and(|flags| flags & 2 != 0))
                {
                    continue;
                }
                let template = if path == "lightOptions/color" {
                    Value::Color([0.0; 4])
                } else if matches!(path, "anchor" | "position" | "scale" | "orientation" | "poi") {
                    Value::Vec3([0.0; 3])
                } else {
                    Value::Scalar(0.0)
                };
                let mut stream = decode(children, &template).map_err(|e| format!("AEP {name}: {e}"))?;
                if path == "orientation" {
                    // The opaque AEP orientation applies Z, then Y, then X to a vector.
                    // Native orientation uses the opposite Euler factorization. Preserve
                    // the actual rotation, rather than copying three incompatible angles.
                    stream.map_values(convert_orientation);
                    if !stream.keys.is_empty() {
                        stream.warnings.push("Animated AEP orientation key poses were converted, but interpolation between poses requires verification".into());
                    }
                }
                if matches!(path, "opacity" | "scale") && chunk(children, b"tdb4").is_some() {
                    stream.multiply(100.0);
                }
                if path == "lightOptions/color" {
                    stream.map_values(|value| match value {
                        Value::Color([a, r, g, b]) => Value::Color([r / 255.0, g / 255.0, b / 255.0, a / 255.0]),
                        value => value.clone(),
                    });
                }
                result.push(DecodedTransform { path: path.into(), stream });
            }
            ChunkNode::List { children, .. } => {
                collect_transform_properties(children, depth + 1, name, result)?;
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn install_transforms(decoded: &[DecodedTransform], layer: &mut Layer, ids: &mut Ids<'_>) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    // AE nulls are anchored at their upper-left corner. An untouched anchor may
    // be absent from the file; do not inherit the native builder's centred anchor.
    // Install explicit values, keys and expressions over this default below.
    if layer.source == effectcraft_project::LayerSource::Null
        && let Some(anchor) = layer.props.prop_mut("transform/anchor")
    {
        anchor.value = Value::Vec3([0.0; 3]);
    }
    for entry in decoded {
        let path = if entry.path.contains('/') || entry.path == "timeRemap" {
            entry.path.clone()
        } else {
            let property = if entry.path == "anchor" && layer.source == effectcraft_project::LayerSource::Camera { "poi" } else { &entry.path };
            format!("transform/{property}")
        };
        if entry.path == "timeRemap" && layer.props.prop(&path).is_none() {
            if ids.0.checked_add(1).is_none() {
                return Err("AEP property ID overflow".into());
            }
            layer.props.children.push(Node::Prop(ids.prop("timeRemap", "Time Remap", Value::Scalar(0.0))));
        }
        if layer.props.prop(&path).is_none() && matches!(entry.path.as_str(), "positionX" | "positionY" | "positionZ") {
            if ids.0.checked_add(1).is_none() {
                return Err("AEP property ID overflow".into());
            }
            if let Some(group) = layer.props.group_mut("transform") {
                group.children.push(Node::Prop(ids.prop(&entry.path, &entry.path, Value::Scalar(0.0))));
            }
        }
        if let Some(property) = layer.props.prop_mut(&path) {
            let mut stream = entry.stream.clone();
            let template = property.value.clone();
            stream.map_values(|value| template.with_components(&value.components()));
            stream.install(property);
            warnings.extend(entry.stream.warnings.iter().cloned());
        }
    }
    // AEP omits the combined Position for untouched cameras, even when their lens
    // differs from the native builder's 50 mm default. Preserve the imported lens's
    // default framing. Active separated coordinates always take precedence.
    if layer.source == effectcraft_project::LayerSource::Camera
        && !decoded.iter().any(|entry| matches!(entry.path.as_str(), "position" | "positionX" | "positionY" | "positionZ"))
    {
        let zoom = layer.props.prop("cameraOptions/zoom").and_then(|property| match property.value {
            Value::Scalar(value) if value.is_finite() && value > 0.0 => Some(value),
            _ => None,
        });
        if let (Some(zoom), Some(position)) = (zoom, layer.props.prop_mut("transform/position"))
            && let Value::Vec3(value) = &mut position.value
        {
            value[2] = -zoom;
        }
    }
    Ok(warnings)
}

fn convert_orientation(value: &Value) -> Value {
    let Value::Vec3([x, y, z]) = value else { return value.clone() };
    use effectcraft_geom::Mat4;
    let matrix = Mat4::rotate_x(*x) * Mat4::rotate_y(*y) * Mat4::rotate_z(*z);
    let rows = matrix.0;
    let y = (-rows[2][0]).clamp(-1.0, 1.0).asin();
    let (x, z) = if y.cos().abs() > 1e-8 { (rows[2][1].atan2(rows[2][2]), rows[1][0].atan2(rows[0][0])) } else { (0.0, (-rows[0][1]).atan2(rows[1][1])) };
    Value::Vec3([x.to_degrees(), y.to_degrees(), z.to_degrees()])
}

#[cfg(test)]
mod tests;
