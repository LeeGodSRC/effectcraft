//! Experimental import of binary RIFX After Effects projects (`.aep`).
//!
//! An `.aep` file is a RIFX (big-endian RIFF) or RIFF container with form type `Egg!`. Inside
//! is a chunk hierarchy:
//! - `Fold`: Root project folder
//! - `Item`: Project items (folders, compositions, footage, solids)
//! - `Sfdr`: Subfolder containing child `Item`s
//! - `cdta`: Composition metadata (width, height, frame rate, rational duration)
//! - `Layr` / `AVLa`: Layers; `SLay` / `CLay` are editor views, not content layers
//! - `ldta`: Layer metadata (source item id, layer index, timings, layer name)
//! - `tdgp`: Property stream groups (transform, effects, text, shape contents)
//!
//! Decodes supported structure into [`effectcraft_project::Project`]. XML `.aepx` and
//! little-endian RIFF files return an explicit unsupported-format error.

/// Maximum binary project bytes accepted by the importer.
pub const MAX_FILE_BYTES: usize = 64 * 1024 * 1024;

use std::collections::HashMap;
use std::sync::Arc;

use effectcraft_color::Label;
use effectcraft_project::build;
use effectcraft_project::{AutoOrient, Comp, Footage, FootageKind, Item, ItemId, ItemKind, LayerSource, Project, Solid};
use effectcraft_time::{FrameRate, Tick};

mod effects;
mod shapes;
mod streams;
mod text;

/// Detailed report on assets, fonts, and plugins found during `.aep` import.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AepReport {
    /// Project file path if known.
    pub project_path: Option<String>,
    /// Font names used by imported text styles (not unused entries in AEP font tables).
    pub fonts: Vec<String>,
    /// Fonts missing from the local system.
    pub missing_fonts: Vec<String>,
    /// Paths or names of missing footage files.
    pub missing_footage: Vec<String>,
    /// Total footage items discovered.
    pub total_footage_count: usize,
    /// Number of missing footage items.
    pub missing_footage_count: usize,
    /// Total compositions discovered.
    pub comp_count: usize,
    /// All effects/plugins used in the project (friendly display names).
    pub plugins: Vec<String>,
    /// Unsupported effect match IDs, including built-in effects without an adapter.
    pub custom_plugins: Vec<String>,
    /// Actual effect instances reconstructed on layers.
    #[serde(default)]
    pub imported_effect_count: usize,
    /// Import limitations and repairs; never silently discard unsupported content.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl AepReport {
    /// Whether the report has missing assets or fidelity limitations to show the user.
    pub fn has_issues(&self) -> bool {
        !self.warnings.is_empty() || !self.custom_plugins.is_empty() || self.missing_footage_count > 0 || !self.missing_fonts.is_empty()
    }
}

/// Whether `bytes` has the binary After Effects project signature (RIFX/RIFF `Egg!`).
pub fn is_aep(bytes: &[u8]) -> bool {
    if bytes.len() < 12 {
        return false;
    }
    let magic = bytes.get(..4);
    let form = bytes.get(8..12);
    (magic == Some(b"RIFX") || magic == Some(b"RIFF")) && form == Some(b"Egg!")
}

/// Whether `bytes` looks like an After Effects XML project (`.aepx`).
pub fn is_aepx(bytes: &[u8]) -> bool {
    let s = match std::str::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return false,
    };
    let mut t = s.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with("<?xml") {
        let Some((_, rest)) = t.split_once("?>") else { return false };
        t = rest.trim_start();
    }
    t.starts_with("<AfterEffects") || t.starts_with("<Egg")
}

/// A node in a parsed RIFF / RIFX chunk tree.
#[derive(Clone, Debug)]
pub enum ChunkNode<'a> {
    Chunk { id: [u8; 4], data: &'a [u8] },
    List { id: [u8; 4], list_type: [u8; 4], children: Vec<ChunkNode<'a>> },
}

/// Parse a RIFX or RIFF binary stream into a tree of chunks.
pub fn parse_rifx(bytes: &[u8]) -> Result<Vec<ChunkNode<'_>>, String> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err("AEP exceeds the 64 MiB import limit".into());
    }
    if !is_aep(bytes) {
        return Err("not an After Effects Egg! container".into());
    }
    if bytes.get(..4) != Some(b"RIFX") {
        return Err("little-endian RIFF AEP projects are not supported yet".into());
    }
    let len = read_u32(bytes, 4).ok_or("missing AEP container size")? as usize;
    let end = len.checked_add(8).ok_or("AEP container size overflow")?;
    let data = bytes.get(12..end).ok_or("truncated AEP container")?;
    parse_chunks(data, 0, &mut 0)
}

fn parse_chunks<'a>(mut data: &'a [u8], depth: usize, count: &mut usize) -> Result<Vec<ChunkNode<'a>>, String> {
    if depth > 64 {
        return Err("AEP chunk nesting exceeds 64 levels".into());
    }
    let mut nodes = Vec::new();
    while !data.is_empty() {
        *count = count.saturating_add(1);
        if *count > 1_000_000 {
            return Err("AEP has too many chunks".into());
        }
        let id = data.get(..4).and_then(|v| v.try_into().ok()).ok_or("truncated AEP chunk header")?;
        let len = read_u32(data, 4).ok_or("truncated AEP chunk size")? as usize;
        let end = 8usize.checked_add(len).ok_or("AEP chunk size overflow")?;
        let payload = data.get(8..end).ok_or("truncated AEP chunk payload")?;
        let padded = end.checked_add(len % 2).ok_or("AEP chunk padding overflow")?;
        data = data.get(padded..).ok_or("missing AEP chunk padding")?;
        if &id == b"LIST" {
            let list_type = payload.get(..4).and_then(|v| v.try_into().ok()).ok_or("missing AEP list type")?;
            let sub = payload.get(4..).ok_or("missing AEP list data")?;
            // Text document payloads use LIST/btdk as an opaque blob, not a RIFF subtree.
            if &list_type == b"btdk" {
                nodes.push(ChunkNode::Chunk { id: list_type, data: sub });
                continue;
            }
            let children = parse_chunks(sub, depth + 1, count)?;
            nodes.push(ChunkNode::List { id, list_type, children });
        } else {
            nodes.push(ChunkNode::Chunk { id, data: payload });
        }
    }
    Ok(nodes)
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes(data.get(offset..offset.checked_add(4)?)?.try_into().ok()?))
}

/// Extracted composition information before turning into project items.
#[derive(Clone, Debug)]
struct DecodedFolder {
    name: String,
    parent: Option<usize>,
}

#[derive(Clone, Debug)]
struct DecodedComp {
    name: String,
    width: u32,
    height: u32,
    pixel_aspect: f64,
    frame_rate: f64,
    duration: Tick,
    background: [f32; 3],
    layers: Vec<DecodedLayer>,
    raw_item_id: u32,
    parent_folder: Option<usize>,
}

/// Extracted footage / media information.
#[derive(Clone, Debug)]
struct DecodedFootage {
    id: u32,
    name: String,
    path: String,
    width: u32,
    height: u32,
    frame_rate: f64,
    _duration_frames: u32,
    parent_folder: Option<usize>,
    is_solid: bool,
    color: [f32; 3],
    source_layer: Option<effectcraft_project::SourceLayer>,
}

/// Extracted layer information.
#[derive(Clone, Debug)]
struct DecodedLayer {
    name: String,
    is_camera: bool,
    is_light: bool,
    in_point: Tick,
    out_point: Tick,
    start: Tick,
    stretch: f64,
    source_item_id: u32,
    three_d: bool,
    is_text: bool,
    is_shape: bool,
    text_font: Option<String>,
    effects: Vec<effects::DecodedEffect>,
    transforms: Vec<streams::DecodedTransform>,
    shapes: shapes::DecodedShapes,
    raw_id: u32,
    parent_id: u32,
    flags: u32,
    blend: u32,
    matte: u32,
    preserve_transparency: bool,
}

/// Scan bytes for `/CoolTypeFont` declarations and extract font family names.
pub fn extract_aep_fonts(bytes: &[u8]) -> Vec<String> {
    let mut fonts = Vec::new();
    let marker = b"/CoolTypeFont";
    let bytes = bytes.get(..bytes.len().min(64 * 1024 * 1024)).unwrap_or(&[]);
    for start in bytes.windows(marker.len()).enumerate().filter_map(|(i, w)| (w == marker).then_some(i.saturating_add(marker.len()))).take(4096) {
        let window = bytes.get(start..start.saturating_add(128).min(bytes.len())).unwrap_or(&[]);
        if let Some(bom) = window.windows(2).position(|w| w == b"\xfe\xff") {
            let begin = start.saturating_add(bom).saturating_add(2);
            let data = bytes.get(begin..begin.saturating_add(256).min(bytes.len())).unwrap_or(&[]);
            let end = data.iter().position(|b| *b == b')').unwrap_or(data.len());
            let units: Vec<_> = data.get(..end).unwrap_or(&[]).as_chunks::<2>().0.iter().map(|b| u16::from_be_bytes(*b)).collect();
            let name = String::from_utf16_lossy(&units).trim().to_string();
            if !name.is_empty() && name != "AdobeInvisFont" && !fonts.contains(&name) {
                fonts.push(name);
            }
        }
    }
    fonts
}

/// Extract all effects and plugins referenced in the project from `Pefl` (pjef) and `EfdG` (EfDf).
pub fn extract_aep_plugins(bytes: &[u8]) -> (Vec<String>, Vec<String>) {
    let mut all_plugins = Vec::new();
    let mut custom_plugins = Vec::new();

    let match_map: HashMap<&str, &str> = [
        ("ADBE 4ColorGradient", "4-Color Gradient"),
        ("ADBE Box Blur2", "Fast Box Blur"),
        ("ADBE Checkbox Control", "Checkbox Control"),
        ("ADBE Color Control", "Color Control"),
        ("ADBE CurvesCustom", "Curves"),
        ("ADBE Drop Shadow", "Drop Shadow"),
        ("ADBE Easy Levels2", "Levels"),
        ("ADBE Fill", "Fill"),
        ("ADBE Gaussian Blur 2", "Gaussian Blur"),
        ("ADBE Geometry2", "Transform"),
        ("ADBE Glo2", "Glow"),
        ("ADBE Grid", "Grid"),
        ("ADBE Linear Wipe", "Linear Wipe"),
        ("ADBE Noise2", "Noise"),
        ("ADBE PhotoFilterPS", "Photo Filter"),
        ("ADBE Sharpen", "Sharpen"),
        ("ADBE Slider Control", "Slider Control"),
        ("ADBE Tile", "Motion Tile"),
        ("ADBE Tint", "Tint"),
        ("ADBE Tritone", "Tritone"),
        ("ADBE Vibrance", "Vibrance"),
        ("CS Composite", "CC Composite"),
    ]
    .into_iter()
    .collect();

    fn walk(nodes: &[ChunkNode<'_>], names: &mut std::collections::BTreeSet<String>) {
        let mut name = String::new();
        for node in nodes {
            match node {
                ChunkNode::Chunk { id: [b't', b'd', b'm', b'n'], data } => {
                    name = c_string(data);
                }
                ChunkNode::List { list_type, children, .. } => {
                    if list_type == b"sspc" && !name.is_empty() {
                        names.insert(name.clone());
                    }
                    walk(children, names);
                }
                _ => {}
            }
        }
    }
    if let Ok(tree) = parse_rifx(bytes) {
        let mut names = std::collections::BTreeSet::new();
        walk(&tree, &mut names);
        for name in names {
            let friendly = match_map.get(name.as_str()).copied().unwrap_or(&name).to_string();
            if effects::effect_id(&name).is_none() {
                custom_plugins.push(friendly.clone());
            }
            all_plugins.push(friendly);
        }
    }
    (all_plugins, custom_plugins)
}

/// Decode an After Effects `.aep` project file into an [`effectcraft_project::Project`].
pub fn from_aep(bytes: &[u8]) -> Result<Project, String> {
    from_aep_with_report(bytes, None).map(|(p, _)| p)
}

/// Decode an After Effects `.aep` project file and produce a detailed report.
pub fn from_aep_with_report(bytes: &[u8], project_path: Option<&std::path::Path>) -> Result<(Project, AepReport), String> {
    let tree = parse_rifx(bytes)?;
    let definitions = effects::definitions(&tree)?;
    let mut comps = Vec::new();
    let mut folders = Vec::new();
    let mut footage = Vec::new();

    scan_nodes(&tree, None, project_path, &mut comps, &mut folders, &mut footage, &definitions)?;

    if comps.is_empty() {
        return Err("no compositions found in After Effects project".into());
    }

    let detected_fonts = extract_aep_fonts(bytes);
    let (all_plugins, custom_plugins) = extract_aep_plugins(bytes);
    build_project(comps, folders, footage, detected_fonts, all_plugins, custom_plugins, project_path)
}

/// Intelligently resolve footage file paths: checks local disk first, then attempts relinking
/// relative to the project directory or common subfolders (`(Footage)`, `Footage`, etc.).
fn resolve_footage_path(file_path: &str, project_path: Option<&std::path::Path>) -> String {
    if file_path.is_empty() {
        return String::new();
    }
    let normalized = file_path.replace('\\', "/");
    let p = std::path::Path::new(&normalized);
    if p.exists() {
        return file_path.to_string();
    }
    let Some(proj_dir) = project_path.and_then(|p| p.parent()) else {
        return file_path.to_string();
    };

    let relative = proj_dir.join(p);
    if relative.is_file() {
        return relative.to_string_lossy().to_string();
    }
    if let Some(file_name) = p.file_name() {
        let candidate = proj_dir.join(file_name);
        if candidate.exists() {
            return candidate.to_string_lossy().to_string();
        }

        for sub in &["(Footage)", "Footage", "footage", "Media", "media", "Assets", "assets", "Source", "source"] {
            let candidate = proj_dir.join(sub).join(file_name);
            if candidate.exists() {
                return candidate.to_string_lossy().to_string();
            }
        }
    }

    let norm = file_path.replace('\\', "/");
    for marker in &["(Footage)/", "Footage/", "footage/", "media/", "assets/"] {
        if let Some(idx) = norm.find(marker) {
            let Some(suffix) = norm.get(idx..) else { continue };
            let candidate = proj_dir.join(suffix);
            if candidate.exists() {
                return candidate.to_string_lossy().to_string();
            }
        }
    }

    if let Some(file_name) = p.file_name().and_then(|f| f.to_str())
        && let Some(found) = find_file_in_dir(proj_dir, file_name, 0, &mut 4096)
    {
        return found.to_string_lossy().to_string();
    }

    file_path.to_string()
}

fn find_file_in_dir(dir: &std::path::Path, target: &str, depth: usize, budget: &mut usize) -> Option<std::path::PathBuf> {
    if depth > 3 || *budget == 0 {
        return None;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return None };
    for entry in entries.flatten() {
        if *budget == 0 {
            return None;
        }
        *budget = budget.saturating_sub(1);
        if entry.file_type().is_ok_and(|t| t.is_symlink()) {
            continue;
        }
        let path = entry.path();
        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && name.eq_ignore_ascii_case(target)
            {
                return Some(path);
            }
        } else if path.is_dir()
            && let Some(found) = find_file_in_dir(&path, target, depth + 1, budget)
        {
            return Some(found);
        }
    }
    None
}

fn scan_nodes(
    nodes: &[ChunkNode<'_>],
    parent_folder: Option<usize>,
    project_path: Option<&std::path::Path>,
    comps: &mut Vec<DecodedComp>,
    folders: &mut Vec<DecodedFolder>,
    footage: &mut Vec<DecodedFootage>,
    definitions: &effects::Definitions,
) -> Result<(), String> {
    for node in nodes {
        if let ChunkNode::List { list_type, children, .. } = node {
            if list_type == b"Item" {
                scan_item(children, parent_folder, project_path, comps, folders, footage, definitions)?;
            } else {
                scan_nodes(children, parent_folder, project_path, comps, folders, footage, definitions)?;
            }
        }
    }
    Ok(())
}

fn extract_alas_path(chunks: &[ChunkNode<'_>]) -> Option<String> {
    for c in chunks {
        match c {
            ChunkNode::Chunk { id, data } => {
                if id == b"alas"
                    && let Some(p) = parse_alas_json(data)
                {
                    return Some(p);
                }
            }
            ChunkNode::List { children, .. } => {
                if let Some(p) = extract_alas_path(children) {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn c_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(bytes.get(..end).unwrap_or(&[])).to_string()
}

fn parse_alas_json(data: &[u8]) -> Option<String> {
    // Windows alias records carry a NUL-terminated drive path, not JSON.
    if let Some(start) = data.windows(3).position(|b| matches!(b, [letter, b':', b'\\' | b'/'] if letter.is_ascii_alphabetic())) {
        let path = c_string(data.get(start..)?);
        if !path.is_empty() {
            return Some(path);
        }
    }
    let key = b"\"fullpath\":\"";
    let idx = data.windows(key.len()).position(|w| w == key)?;
    let start = idx + key.len();
    let mut end = start;
    let mut escaped = false;
    while end < data.len() {
        if escaped {
            escaped = false;
        } else if data.get(end) == Some(&b'\\') {
            escaped = true;
        } else if data.get(end) == Some(&b'"') {
            break;
        }
        end += 1;
    }
    let raw = std::str::from_utf8(data.get(start..end)?).ok()?;
    let unescaped = raw.replace(r"\\", r"\").replace(r#"\""#, r#"""#);
    if !unescaped.is_empty() { Some(unescaped) } else { None }
}

fn scan_item(
    chunks: &[ChunkNode<'_>],
    parent_folder: Option<usize>,
    project_path: Option<&std::path::Path>,
    comps: &mut Vec<DecodedComp>,
    folders: &mut Vec<DecodedFolder>,
    footage: &mut Vec<DecodedFootage>,
    definitions: &effects::Definitions,
) -> Result<(), String> {
    let mut name = String::new();
    let mut cdta: Option<&[u8]> = None;
    let mut idta: Option<&[u8]> = None;
    let mut iide: Option<u32> = None;
    let mut subfolder: Option<&[ChunkNode<'_>]> = None;
    let mut layer_chunks = Vec::new();

    for c in chunks {
        match c {
            ChunkNode::Chunk { id, data } => {
                if id == b"Utf8" {
                    let s = String::from_utf8_lossy(data).trim_matches('\0').trim().to_string();
                    if !s.is_empty() {
                        name = s;
                    }
                } else if id == b"Utf1" && name.is_empty() {
                    let u16s: Vec<u16> = data.as_chunks::<2>().0.iter().map(|b| u16::from_be_bytes(*b)).collect();
                    let s = String::from_utf16_lossy(&u16s).trim_matches('\0').trim().to_string();
                    if !s.is_empty() {
                        name = s;
                    }
                } else if id == b"cdta" {
                    cdta = Some(data);
                } else if id == b"idta" {
                    idta = Some(data);
                } else if id == b"iide" && data.len() >= 4 {
                    iide = Some(data.get(..4).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes).unwrap_or(0));
                }
            }
            ChunkNode::List { list_type, children, .. } => {
                if list_type == b"Sfdr" {
                    subfolder = Some(children);
                } else if list_type == b"Layr" || list_type == b"AVLa" {
                    layer_chunks.push((list_type, children.as_slice()));
                }
            }
        }
    }

    let raw_item_id = if let Some(d) = idta.filter(|d| d.len() >= 20) {
        let sub_id = read_u32(d, 16).unwrap_or(0);
        if sub_id > 0 { sub_id } else { iide.unwrap_or(0) }
    } else if let Some(lid) = iide {
        lid
    } else if let Some(d) = idta.filter(|d| d.len() >= 4) {
        read_u32(d, 0).unwrap_or(0)
    } else {
        0
    };

    if let Some(sub) = subfolder {
        let folder_name = if name.is_empty() { "Folder".to_string() } else { name };
        let folder_key = folders.len();
        folders.push(DecodedFolder { name: folder_name, parent: parent_folder });
        scan_nodes(sub, Some(folder_key), project_path, comps, folders, footage, definitions)?;
    } else if let Some(cd) = cdta {
        let comp_name = if name.is_empty() { format!("Comp {}", comps.len() + 1) } else { name };
        let (w, h, fps, duration, bg) = parse_cdta(cd)?;

        let mut decoded_layers = Vec::new();
        for (_, lchildren) in layer_chunks {
            let layer = parse_layer(lchildren, definitions)?;
            decoded_layers.push(layer);
        }

        comps.push(DecodedComp {
            name: comp_name,
            width: w,
            height: h,
            pixel_aspect: 1.0,
            frame_rate: fps,
            duration,
            background: bg,
            layers: decoded_layers,
            raw_item_id,
            parent_folder,
        });
    } else {
        let raw_file_path = extract_alas_path(chunks).unwrap_or_default();
        let file_path = if !raw_file_path.is_empty() { resolve_footage_path(&raw_file_path, project_path) } else { String::new() };
        let pin = chunks
            .iter()
            .find_map(|n| match n {
                ChunkNode::List { list_type: [b'P', b'i', b'n', b' '], children, .. } => Some(children.as_slice()),
                _ => None,
            })
            .unwrap_or(&[]);
        let options = chunk_data(pin, b"opti").unwrap_or(&[]);
        let is_solid = options.starts_with(b"Soli");
        let metadata = chunk_data(pin, b"sspc").unwrap_or(&[]);
        let width = read_u32(metadata, 30).filter(|n| *n > 0 && *n <= 32768).unwrap_or(1920);
        let height = read_u32(metadata, 34).filter(|n| *n > 0 && *n <= 32768).unwrap_or(1080);
        let source_layer = if options.starts_with(b"8BPS") {
            read_u32(metadata, 192).filter(|index| *index != u32::MAX && *index < 32768).map(|index| {
                let name = c_string(metadata.get(196..260).unwrap_or(&[]));
                let canvas = |offset: usize| options.get(offset..offset.saturating_add(4)).and_then(|b| b.try_into().ok()).map(u32::from_le_bytes);
                effectcraft_project::SourceLayer {
                    index,
                    name,
                    layer_size: canvas(36).is_some_and(|w| w != width) || canvas(32).is_some_and(|h| h != height),
                    ..Default::default()
                }
            })
        } else {
            None
        };
        let color = [14, 18, 22].map(|offset| {
            options.get(offset..offset + 4).and_then(|b| b.try_into().ok()).map(f32::from_be_bytes).filter(|v| v.is_finite()).unwrap_or(0.5).clamp(0.0, 1.0)
        });
        if name.is_empty() && is_solid {
            name = c_string(options.get(26..).unwrap_or(&[]));
        }
        if name.is_empty()
            && let Some(layer) = &source_layer
            && !layer.name.is_empty()
        {
            name.clone_from(&layer.name);
        }
        let fname = if !name.is_empty() {
            name
        } else if !file_path.is_empty() {
            file_path.rsplit(['/', '\\']).next().unwrap_or("Footage").to_string()
        } else {
            "Item".into()
        };

        footage.push(DecodedFootage {
            id: raw_item_id,
            name: fname,
            path: file_path,
            width,
            height,
            frame_rate: 30.0,
            _duration_frames: 300,
            parent_folder,
            is_solid,
            color,
            source_layer,
        });
    }
    Ok(())
}

/// Extract composition metadata from the `cdta` chunk.
fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes(data.get(offset..offset.checked_add(2)?)?.try_into().ok()?))
}

fn rational_time(data: &[u8], offset: usize) -> Result<Tick, String> {
    let numerator = read_u32(data, offset).ok_or("truncated AEP time")? as i32;
    let denominator = read_u32(data, offset.saturating_add(4)).filter(|v| *v > 0).ok_or("invalid AEP time denominator")?;
    Ok(Tick::from_units(i64::from(numerator), i64::from(denominator)))
}

fn parse_cdta(data: &[u8]) -> Result<(u32, u32, f64, Tick, [f32; 3]), String> {
    let w = read_u16(data, 140).filter(|v| *v > 0).ok_or("invalid AEP composition width")?;
    let h = read_u16(data, 142).filter(|v| *v > 0).ok_or("invalid AEP composition height")?;
    let integer = read_u16(data, 156).ok_or("truncated AEP frame rate")?;
    let fraction = read_u16(data, 158).ok_or("truncated AEP frame rate")?;
    let fps = f64::from(integer) + f64::from(fraction) / 65536.0;
    if !(1.0..=999.0).contains(&fps) {
        return Err("invalid AEP frame rate".into());
    }
    let duration = rational_time(data, 44)?;
    if duration <= Tick::ZERO {
        return Err("invalid AEP composition duration".into());
    }
    let bg = [52, 53, 54].map(|i| f32::from(data.get(i).copied().unwrap_or(0)) / 255.0);
    Ok((u32::from(w), u32::from(h), fps, duration, bg))
}

/// Extract layer metadata from a layer's child chunks.
fn parse_layer(chunks: &[ChunkNode<'_>], definitions: &effects::Definitions) -> Result<DecodedLayer, String> {
    let mut name = String::new();
    let mut ldta: Option<&[u8]> = None;
    let mut is_text = false;
    let mut is_shape = false;
    let is_camera = has_match(chunks, "ADBE Camera Options Group");
    let is_light = has_match(chunks, "ADBE Light Options Group");
    let three_d = is_camera || is_light;
    let mut effects = Vec::new();
    let mut text_font = None;

    for c in chunks {
        match c {
            ChunkNode::Chunk { id, data } => {
                if id == b"Utf8" && !data.is_empty() {
                    let s = String::from_utf8_lossy(data).trim_matches('\0').trim().to_string();
                    if !s.is_empty() {
                        name = s;
                    }
                } else if id == b"ldta" {
                    ldta = Some(data);
                }
            }
            ChunkNode::List { list_type, children, .. } => {
                if list_type == b"tdgp" {
                    inspect_tdgp(children, &mut is_text, &mut is_shape, &mut effects, &mut text_font);
                }
            }
        }
    }

    let ld = ldta.ok_or("AEP layer has no metadata")?;
    let start = rational_time(ld, 12)?;
    let direction = read_u32(ld, 8).ok_or("truncated AEP layer time direction")? as i32;
    let stretch = match direction {
        1 => 100.0,
        -1 => -100.0,
        _ => return Err(format!("unsupported AEP layer time direction {direction}")),
    };
    let source_in = rational_time(ld, 20)?;
    let source_out = rational_time(ld, 28)?;
    if source_out < source_in {
        return Err("AEP layer ends before its in point".into());
    }
    // Trim bounds are source times. A reversed layer maps the source out-point to its
    // composition in-point, and the source in-point to its composition out-point.
    let (in_point, out_point) = if direction < 0 { (start - source_out, start - source_in) } else { (start + source_in, start + source_out) };
    let source_item_id = read_u32(ld, 40).ok_or("truncated AEP layer source")?;
    let transforms = streams::collect_transforms(chunks).map_err(|e| format!("AEP transform: {e}"))?;

    if name.is_empty() {
        name = match () {
            _ if is_camera => "Camera".into(),
            _ if is_light => "Light".into(),

            _ if is_text => "Text".into(),
            _ if is_shape => "Shape Layer".into(),
            _ => "Layer".into(),
        };
    }

    Ok(DecodedLayer {
        name,
        is_camera,
        is_light,
        in_point,
        out_point,
        start,
        stretch,
        source_item_id,
        three_d,
        is_text,
        is_shape,
        text_font,
        effects: effects::decode(chunks, definitions)?,
        transforms,
        shapes: shapes::decode(chunks)?,
        raw_id: read_u32(ld, 0).unwrap_or(0),
        parent_id: read_u32(ld, 132).unwrap_or(0),
        flags: read_u32(ld, 36).unwrap_or(7),
        blend: read_u32(ld, 96).unwrap_or(2),
        matte: read_u32(ld, 104).unwrap_or(0),
        preserve_transparency: read_u32(ld, 100).unwrap_or(0) != 0,
    })
}

fn chunk_data<'a>(nodes: &[ChunkNode<'a>], tag: &[u8; 4]) -> Option<&'a [u8]> {
    nodes.iter().find_map(|n| match n {
        ChunkNode::Chunk { id, data } if id == tag => Some(*data),
        _ => None,
    })
}

fn inspect_tdgp(chunks: &[ChunkNode<'_>], is_text: &mut bool, is_shape: &mut bool, effects: &mut Vec<String>, text_font: &mut Option<String>) {
    for c in chunks {
        match c {
            ChunkNode::Chunk { id, data } => {
                if id == b"tdmn" {
                    let mn = String::from_utf8_lossy(data);
                    if mn.contains("ADBE Text Properties") {
                        *is_text = true;
                    } else if mn.contains("Shape Layer") || mn.contains("ADBE Vector") {
                        *is_shape = true;
                    }
                } else if id == b"tdsn" && !data.is_empty() {
                    let sn = String::from_utf8_lossy(data).trim_matches('\0').trim().to_string();
                    if !sn.is_empty() && sn != "Transform" && sn != "Compositing Options" {
                        effects.push(sn);
                    }
                } else if id == b" /98" && text_font.is_none() {
                    let fonts = extract_aep_fonts(data);
                    if let Some(first) = fonts.into_iter().next() {
                        *text_font = Some(first);
                    }
                }
            }
            ChunkNode::List { children, .. } => {
                inspect_tdgp(children, is_text, is_shape, effects, text_font);
            }
        }
    }
}

/// Build an [`effectcraft_project::Project`] and [`AepReport`] from the decoded components.
fn build_project(
    decoded_comps: Vec<DecodedComp>,
    decoded_folders: Vec<DecodedFolder>,
    decoded_footage: Vec<DecodedFootage>,
    fonts: Vec<String>,
    plugins: Vec<String>,
    custom_plugins: Vec<String>,
    project_path: Option<&std::path::Path>,
) -> Result<(Project, AepReport), String> {
    if decoded_comps.len().saturating_add(decoded_footage.len()).saturating_add(decoded_folders.len()) > 8192
        || decoded_comps.iter().map(|c| c.layers.len()).sum::<usize>() > 4096
    {
        return Err("AEP exceeds the import limit (8192 items / 4096 layers)".into());
    }
    let mut project = Project::default();
    let comp_count = decoded_comps.len();
    let mut folder_ids: HashMap<usize, ItemId> = HashMap::new();
    let mut missing_footage = Vec::new();
    let mut source_sizes = HashMap::new();
    let mut import_warnings = Vec::new();
    let mut imported_effect_count = 0;

    // 1. Create folders
    for (key, folder) in decoded_folders.into_iter().enumerate() {
        let fname = folder.name;
        let parent_name = folder.parent;
        let parent = parent_name.and_then(|p| folder_ids.get(&p)).copied();
        let fid = ItemId(project.alloc());
        project
            .items
            .insert(fid, Item { id: fid, name: fname.clone(), label: Label::Yellow, comment: String::new(), parent, kind: ItemKind::Folder, proxy: None });
        folder_ids.insert(key, fid);
    }

    // Default Solids folder for any solid layers
    let solids_folder = if let Some(id) = project.items.values().find(|i| i.name == "Solids" && i.is_folder()).map(|i| i.id) {
        id
    } else {
        let fid = ItemId(project.alloc());
        project.items.insert(
            fid,
            Item { id: fid, name: "Solids".into(), label: Label::Yellow, comment: String::new(), parent: None, kind: ItemKind::Folder, proxy: None },
        );
        fid
    };

    // 2. Pre-allocate comp item IDs so precomposing references resolve
    let mut comp_ids = Vec::new();
    let mut comp_names = HashMap::new();
    let mut comp_by_raw_id: HashMap<u32, ItemId> = HashMap::new();
    for c in &decoded_comps {
        let cid = ItemId(project.alloc());
        comp_ids.push(cid);
        comp_names.insert(cid, c.name.clone());
        source_sizes.insert(cid, (c.width, c.height));
        if c.raw_item_id > 0 && comp_by_raw_id.insert(c.raw_item_id, cid).is_some() {
            return Err("duplicate AEP composition ID".into());
        }
    }

    // 3. Create footage items
    let mut footage_ids: HashMap<u32, ItemId> = HashMap::new();

    let total_footage = decoded_footage.len();

    for dfoot in decoded_footage {
        let parent = dfoot.parent_folder.and_then(|p| folder_ids.get(&p)).copied();
        let fid = ItemId(project.alloc());

        source_sizes.insert(fid, (dfoot.width, dfoot.height));
        if dfoot.is_solid {
            let solid = Solid { color: dfoot.color, width: dfoot.width, height: dfoot.height, pixel_aspect: 1.0 };
            project.items.insert(
                fid,
                Item {
                    id: fid,
                    name: dfoot.name.clone(),
                    label: Label::Red,
                    comment: String::new(),
                    parent: parent.or(Some(solids_folder)),
                    kind: ItemKind::Solid(solid),
                    proxy: None,
                },
            );
        } else {
            let path_exists = !dfoot.path.is_empty() && std::path::Path::new(&dfoot.path).exists();
            let missing = !path_exists;
            if missing {
                missing_footage.push(if !dfoot.path.is_empty() { dfoot.path.clone() } else { dfoot.name.clone() });
            }

            let p_lower = dfoot.path.to_ascii_lowercase();
            let kind = if p_lower.ends_with(".wav") || p_lower.ends_with(".mp3") || p_lower.ends_with(".aac") || p_lower.ends_with(".m4a") {
                FootageKind::Audio
            } else if p_lower.ends_with(".mp4") || p_lower.ends_with(".mov") || p_lower.ends_with(".avi") || p_lower.ends_with(".mkv") {
                FootageKind::Video
            } else {
                FootageKind::Still
            };

            let label = match kind {
                FootageKind::Audio => Label::SeaFoam,
                FootageKind::Video => Label::Aqua,
                FootageKind::Still => Label::Lavender,
                _ => Label::Lavender,
            };

            let footage_obj = Footage {
                path: dfoot.path.clone(),
                kind,
                width: dfoot.width,
                height: dfoot.height,
                pixel_aspect: 1.0,
                frame_rate: FrameRate::from_f64(dfoot.frame_rate),
                native_rate: None,
                duration: Tick::from_seconds_f64(10.0),
                has_video: kind != FootageKind::Audio,
                has_audio: kind == FootageKind::Audio || kind == FootageKind::Video,
                alpha: effectcraft_project::AlphaMode::Straight,
                premul_color: [0.0, 0.0, 0.0],
                loop_count: 1,
                codec: String::new(),
                missing,
                sequence: Vec::new(),
                color_profile: None,
                preserve_rgb: false,
                layer: dfoot.source_layer,
                fields: Default::default(),
                invert_alpha: false,
                linear_light: false,
                data: None,
                page: 0,
                alphabetical: false,
                start_timecode: None,
            };

            project.items.insert(
                fid,
                Item { id: fid, name: dfoot.name.clone(), label, comment: String::new(), parent, kind: ItemKind::Footage(footage_obj), proxy: None },
            );
        }

        if dfoot.id > 0 && (footage_ids.insert(dfoot.id, fid).is_some() || comp_by_raw_id.contains_key(&dfoot.id)) {
            return Err("duplicate AEP item ID".into());
        }
    }

    // 4. Build each composition
    for (dcomp, cid) in decoded_comps.into_iter().zip(comp_ids) {
        let frame_rate = FrameRate::from_f64(dcomp.frame_rate);
        let duration = dcomp.duration;

        let mut comp = Comp {
            width: dcomp.width,
            height: dcomp.height,
            pixel_aspect: dcomp.pixel_aspect,
            frame_rate,
            duration,
            display_start: Tick::ZERO,
            background: dcomp.background,
            work_area: (Tick::ZERO, duration),
            layers: Vec::new(),
            markers: Vec::new(),
            shutter_angle: 180.0,
            shutter_phase: -90.0,
            motion_blur_samples: 16,
            motion_blur_adaptive_limit: 128,
            renderer: Default::default(),
            hide_shy: false,
            enable_motion_blur: true,
            enable_frame_blending: true,
            draft_3d: false,
            preserve_frame_rate: false,
            preserve_resolution: false,
            poster_time: Tick::ZERO,
            global_light: Default::default(),
            guides: Vec::new(),
            essential: None,
        };

        let mut raw_layer_ids = HashMap::new();
        let mut layer_ids = Vec::new();
        for dlayer in &dcomp.layers {
            let id = project.alloc();
            layer_ids.push(effectcraft_project::LayerId(id));
            if dlayer.raw_id != 0 && raw_layer_ids.insert(dlayer.raw_id, id).is_some() {
                return Err("duplicate AEP layer ID within a composition".into());
            }
        }
        let mut previous_layer = None;
        for (dlayer, layer_id) in dcomp.layers.into_iter().zip(layer_ids) {
            let source = match () {
                _ if dlayer.is_camera => LayerSource::Camera,
                _ if dlayer.is_light => LayerSource::Light { kind: effectcraft_project::LightKind::Point },
                _ if dlayer.is_text => LayerSource::Text,
                _ if dlayer.is_shape => LayerSource::Shape,
                _ if dlayer.flags & 0x8000 != 0 => LayerSource::Null,
                _ => {
                    if let Some(&item) = footage_ids.get(&dlayer.source_item_id) {
                        if project.item(item).is_some_and(|i| matches!(i.kind, ItemKind::Solid(_))) {
                            LayerSource::Solid { item }
                        } else {
                            LayerSource::Footage { item }
                        }
                    } else if let Some(&item) = comp_by_raw_id.get(&dlayer.source_item_id) {
                        LayerSource::Comp { item }
                    } else {
                        if dlayer.source_item_id != 0 {
                            import_warnings.push(format!(
                                "{} / {}: missing AEP source item {}; preserving the layer as an empty placeholder",
                                dcomp.name, dlayer.name, dlayer.source_item_id
                            ));
                        }
                        LayerSource::Null
                    }
                }
            };

            let layer_name = if dlayer.name == "Layer" {
                source
                    .item()
                    .and_then(|id| project.item(id).map(|i| i.name.clone()).or_else(|| comp_names.get(&id).cloned()))
                    .unwrap_or_else(|| dlayer.name.clone())
            } else {
                dlayer.name.clone()
            };
            let source_size = source.item().and_then(|id| source_sizes.get(&id)).copied();
            let size = source_size.unwrap_or((dcomp.width, dcomp.height));
            let mut layer = build::layer(&mut project, &comp, &layer_name, source, size, Some(duration));
            // Imports preserve names, including duplicates used by expressions. The interactive
            // builder's name uniquification is appropriate only for newly created layers.
            layer.name = layer_name.clone();
            layer.props.name = layer_name;
            layer.id = layer_id;
            layer.start_time = dlayer.start;
            layer.stretch = dlayer.stretch;
            layer.in_point = dlayer.in_point;
            layer.out_point = dlayer.out_point;
            layer.switches.video = dlayer.flags & 1 != 0;
            layer.switches.audio = dlayer.flags & 2 != 0;
            layer.switches.effects = dlayer.flags & 4 != 0;
            layer.switches.collapse = dlayer.flags & 0x80 != 0;
            layer.switches.adjustment = dlayer.flags & 0x200 != 0;
            layer.switches.motion_blur = dlayer.flags & 0x10000 != 0;
            // A camera has no motion blur switch: in observed files the same bit is set on
            // cameras that aim at their point of interest and clear on one-node cameras.
            if dlayer.is_camera {
                layer.switches.motion_blur = false;
                layer.auto_orient = if dlayer.flags & 0x10000 != 0 { AutoOrient::TowardsPointOfInterest } else { AutoOrient::Off };
            }
            layer.switches.three_d = dlayer.three_d || dlayer.flags & 0x400 != 0;
            layer.parent = raw_layer_ids.get(&dlayer.parent_id).copied().map(effectcraft_project::LayerId);
            if dlayer.parent_id != 0 && layer.parent.is_none() {
                return Err(format!("AEP layer {} references a missing parent", layer.name));
            }
            layer.preserve_transparency = dlayer.preserve_transparency;
            layer.blend_mode = match dlayer.blend {
                0 | 2 => effectcraft_color::BlendMode::Normal,
                4 => effectcraft_color::BlendMode::Add,
                5 => effectcraft_color::BlendMode::Multiply,
                6 => effectcraft_color::BlendMode::Screen,
                value => {
                    import_warnings.push(format!("{}: unsupported AEP blending mode {value}; using Normal", layer.name));
                    effectcraft_color::BlendMode::Normal
                }
            };
            if dlayer.matte != 0 {
                let kind = match dlayer.matte {
                    1 => effectcraft_project::MatteKind::Alpha,
                    2 => effectcraft_project::MatteKind::AlphaInverted,
                    3 => effectcraft_project::MatteKind::Luma,
                    4 => effectcraft_project::MatteKind::LumaInverted,
                    _ => return Err("unsupported AEP track matte mode".into()),
                };
                if let Some(matte_layer) = previous_layer {
                    layer.track_matte = Some(effectcraft_project::TrackMatte { layer: matte_layer, kind });
                } else {
                    import_warnings.push(format!("{} / {}: track matte mode has no source layer; leaving the matte unassigned", dcomp.name, layer.name));
                }
            }
            previous_layer = Some(layer_id);

            if dlayer.is_text {
                let font_name = dlayer.text_font.as_deref().or_else(|| fonts.first().map(String::as_str)).unwrap_or("SegoeUI").to_string();
                let doc = effectcraft_keyframe::TextDoc {
                    text: dlayer.name.clone(),
                    font: font_name,
                    size: 48.0,
                    fill: [1.0, 1.0, 1.0, 1.0],
                    apply_fill: true,
                    ..Default::default()
                };
                if let Some(pr) = layer.props.prop_mut("text/sourceText") {
                    pr.value = effectcraft_keyframe::Value::Text(Box::new(doc));
                }
            }

            import_warnings.extend(streams::install_transforms(&dlayer.transforms, &mut layer, source_size, &mut build::Ids(&mut project.next_id))?);
            import_warnings.extend(shapes::apply(&dlayer.shapes, &mut project, &mut layer, size)?);
            let (groups, warnings) =
                effects::instantiate(&dlayer.effects, &mut build::Ids(&mut project.next_id), [f64::from(size.0), f64::from(size.1)], &raw_layer_ids)?;
            import_warnings.extend(warnings);
            imported_effect_count += groups.len();
            if let Some(group) = layer.props.sub_mut("effects") {
                group.children.extend(groups.into_iter().map(Into::into));
            }
            comp.layers.push(layer);
        }

        for layer in &comp.layers {
            let mut seen = std::collections::HashSet::new();
            let mut cursor = Some(layer.id);
            while let Some(id) = cursor {
                if !seen.insert(id) || seen.len() > 64 {
                    return Err("AEP contains cyclic or excessively deep layer parenting".into());
                }
                cursor = comp.layer(id).and_then(|l| l.parent);
            }
        }
        let parent = dcomp.parent_folder.and_then(|p| folder_ids.get(&p)).copied();
        project.items.insert(
            cid,
            Item { id: cid, name: dcomp.name, label: Label::Sandstone, comment: String::new(), parent, kind: ItemKind::Comp(Arc::new(comp)), proxy: None },
        );
    }

    // Text blobs also contain editing defaults such as Myriad-Roman, even when no
    // character uses them. Report the reconstructed styles, including every run.
    let mut used_fonts = std::collections::BTreeSet::new();
    for (_, comp) in project.comps() {
        for layer in &comp.layers {
            if let Some(prop) = layer.props.prop("text/sourceText") {
                for value in std::iter::once(&prop.value).chain(prop.keys.iter().map(|k| &k.value)) {
                    if let effectcraft_keyframe::Value::Text(doc) = value {
                        used_fonts.insert(doc.font.clone());
                        used_fonts.extend(doc.runs.iter().map(|run| run.style.font.clone()));
                    }
                }
            }
        }
    }
    let missing_count = missing_footage.len();
    let report = AepReport {
        project_path: project_path.map(|p| p.to_string_lossy().to_string()),
        fonts: used_fonts.into_iter().collect(),
        missing_fonts: Vec::new(),
        missing_footage,
        total_footage_count: total_footage,
        missing_footage_count: missing_count,
        comp_count,
        plugins,
        custom_plugins,
        imported_effect_count,
        warnings: {
            import_warnings.push("Experimental AEP import: validate the rendered result and keep the original AEP project.".into());
            import_warnings.sort();
            import_warnings.dedup();
            import_warnings
        },
    };

    validate_comp_graph(&project)?;
    Ok((project, report))
}

/// Decode an After Effects XML project (`.aepx`) into an [`effectcraft_project::Project`].
pub fn from_aepx(_bytes: &[u8]) -> Result<Project, String> {
    Err("AEPX XML project import is not supported yet; use a binary .aep project".into())
}

/// Decode an After Effects project from either binary `.aep` or XML `.aepx` format.
pub fn from_aep_or_aepx(bytes: &[u8]) -> Result<Project, String> {
    from_aep_or_aepx_with_report(bytes, None).map(|(p, _)| p)
}

/// Decode an After Effects project with a detailed report on assets, fonts, and plugins.
pub fn from_aep_or_aepx_with_report(bytes: &[u8], project_path: Option<&std::path::Path>) -> Result<(Project, AepReport), String> {
    if is_aep(bytes) {
        from_aep_with_report(bytes, project_path)
    } else if is_aepx(bytes) {
        let p = from_aepx(bytes)?;
        let report = AepReport { comp_count: p.comps().count(), project_path: project_path.map(|p| p.to_string_lossy().to_string()), ..Default::default() };
        Ok((p, report))
    } else {
        Err("unrecognized After Effects project format (not RIFX or AEPX)".into())
    }
}

/// Recognized project extensions (including unsupported XML, for an actionable error).
pub fn is_aep_path(path: &str) -> bool {
    let path = path.to_ascii_lowercase();
    path.ends_with(".aep") || path.ends_with(".aepx")
}

fn validate_comp_graph(project: &Project) -> Result<(), String> {
    fn visit(project: &Project, id: ItemId, stack: &mut std::collections::HashSet<ItemId>, done: &mut HashMap<ItemId, usize>) -> Result<usize, String> {
        if let Some(height) = done.get(&id) {
            return Ok(*height);
        }
        if stack.len() >= 64 {
            return Err("AEP precomposition nesting exceeds 64 levels".into());
        }
        if !stack.insert(id) {
            return Err("AEP contains a cyclic precomposition reference".into());
        }
        let mut height = 1;
        if let Some(comp) = project.comp(id) {
            for layer in &comp.layers {
                if let LayerSource::Comp { item } = layer.source {
                    height = height.max(visit(project, item, stack, done)?.saturating_add(1));
                    if height > 64 {
                        return Err("AEP precomposition nesting exceeds 64 levels".into());
                    }
                }
            }
        }
        stack.remove(&id);
        done.insert(id, height);
        Ok(height)
    }
    let mut done = HashMap::new();
    for (id, _) in project.comps() {
        visit(project, *id, &mut std::collections::HashSet::new(), &mut done)?;
    }
    Ok(())
}

fn has_match(nodes: &[ChunkNode<'_>], name: &str) -> bool {
    nodes.iter().any(|node| match node {
        ChunkNode::Chunk { id: [b't', b'd', b'm', b'n'], data } => c_string(data) == name,
        ChunkNode::List { children, .. } => has_match(children, name),
        _ => false,
    })
}
