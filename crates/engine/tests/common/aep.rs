//! Synthetic RIFX data, created from scratch; no external project or media fixtures.
pub fn chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = tag.to_vec();
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(data);
    if !data.len().is_multiple_of(2) {
        out.push(0);
    }
    out
}
pub fn list(tag: &[u8; 4], children: &[Vec<u8>]) -> Vec<u8> {
    let mut data = tag.to_vec();
    for child in children {
        data.extend(child);
    }
    chunk(b"LIST", &data)
}
pub fn container(children: &[Vec<u8>]) -> Vec<u8> {
    let mut payload = b"Egg!".to_vec();
    for c in children {
        payload.extend(c);
    }
    chunk(b"RIFX", &payload)
}
fn put(data: &mut [u8], offset: usize, v: u32) {
    data[offset..offset + 4].copy_from_slice(&v.to_be_bytes());
}
fn item(id: u32, name: &str, children: &[Vec<u8>]) -> Vec<u8> {
    let mut header = vec![0; 84];
    put(&mut header, 16, id);
    let mut nodes = vec![chunk(b"idta", &header), chunk(b"Utf8", name.as_bytes())];
    nodes.extend_from_slice(children);
    list(b"Item", &nodes)
}
pub fn comp(id: u32, name: &str, source: u32) -> Vec<u8> {
    let mut meta = vec![0; 204];
    put(&mut meta, 44, 240);
    put(&mut meta, 48, 24);
    meta[140..142].copy_from_slice(&64u16.to_be_bytes());
    meta[142..144].copy_from_slice(&48u16.to_be_bytes());
    meta[156..158].copy_from_slice(&24u16.to_be_bytes());
    let mut ld = vec![0; 160];
    put(&mut ld, 0, 100 + id);
    put(&mut ld, 8, 1);
    put(&mut ld, 12, 12);
    put(&mut ld, 16, 24);
    put(&mut ld, 20, 24);
    put(&mut ld, 24, 24);
    put(&mut ld, 28, 240);
    put(&mut ld, 32, 24);
    put(&mut ld, 36, 7);
    put(&mut ld, 40, source);
    let pos: Vec<_> = [23.0f64, 17.0, 0.0].into_iter().flat_map(f64::to_be_bytes).collect();
    let transform = list(b"tdgp", &[chunk(b"tdmn", b"ADBE Position"), list(b"tdbs", &[chunk(b"cdat", &pos)])]);
    let layer = list(b"Layr", &[chunk(b"ldta", &ld), chunk(b"Utf8", b"Layer"), list(b"tdgp", &[chunk(b"tdmn", b"ADBE Transform Group"), transform])]);
    item(
        id,
        name,
        &[
            chunk(b"cdta", &meta),
            layer,
            list(b"SLay", &[chunk(b"ldta", &ld), chunk(b"Utf8", b"Front")]),
            list(b"CLay", &[chunk(b"ldta", &ld), chunk(b"Utf8", b"Custom View 1")]),
            list(b"SecL", &[chunk(b"ldta", &ld), chunk(b"Utf8", b"Markers")]),
        ],
    )
}
pub fn project() -> Vec<u8> {
    let mut metadata = vec![0; 286];
    put(&mut metadata, 30, 64);
    put(&mut metadata, 34, 48);
    let mut solid = b"Soli".to_vec();
    solid.resize(26, 0);
    for (i, v) in [1.0f32, 0.8, 0.2, 0.1].into_iter().enumerate() {
        solid[10 + i * 4..14 + i * 4].copy_from_slice(&v.to_be_bytes());
    }
    solid.extend(b"Original solid\0");
    let solid = item(3, "Original solid", &[list(b"Pin ", &[chunk(b"sspc", &metadata), chunk(b"opti", &solid)])]);
    container(&[list(b"Fold", &[comp(1, "Main", 2), comp(2, "Nested", 3), solid])])
}

pub fn footage_project(path: &str) -> Vec<u8> {
    let mut metadata = vec![0; 286];
    put(&mut metadata, 30, 64);
    put(&mut metadata, 34, 48);
    let mut alias = vec![0; 16];
    alias.extend(path.as_bytes());
    alias.push(0);
    let footage = item(3, "Media", &[list(b"Pin ", &[chunk(b"sspc", &metadata), list(b"Alas", &[chunk(b"alas", &alias)])])]);
    let folder = item(4, "Assets", &[list(b"Sfdr", &[footage])]);
    container(&[list(b"Fold", &[comp(1, "Main", 3), folder])])
}

pub fn layered_footage_project() -> Vec<u8> {
    let mut metadata = vec![0; 286];
    put(&mut metadata, 30, 64);
    put(&mut metadata, 34, 48);
    put(&mut metadata, 192, 2);
    metadata[196..201].copy_from_slice(b"Stars");
    let mut options = vec![0; 40];
    options[..4].copy_from_slice(b"8BPS");
    options[30..32].copy_from_slice(&4u16.to_le_bytes());
    options[32..36].copy_from_slice(&48u32.to_le_bytes());
    options[36..40].copy_from_slice(&64u32.to_le_bytes());
    container(&[comp(1, "Layered", 3), item(3, "Stars", &[list(b"Pin ", &[chunk(b"sspc", &metadata), chunk(b"opti", &options)])])])
}
