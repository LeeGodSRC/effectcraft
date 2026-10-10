//! Original synthetic font data: fallback must not depend on earlier layers' font use.
use effectcraft_text::fonts::{self, FaceData, FaceInfo, FontSource};
use std::sync::Arc;

struct SyntheticFonts;
impl FontSource for SyntheticFonts {
    fn faces(&self) -> Vec<FaceInfo> {
        let data = FaceData::Shared(Arc::new(blank_glyph_font()));
        [("Already Loaded", "Regular", 400), ("Yu Gothic", "Regular", 400), ("Yu Gothic", "Bold", 700)]
            .into_iter()
            .map(|(family, style, weight)| FaceInfo {
                family: family.into(),
                style: style.into(),
                weight,
                italic: false,
                index: 0,
                data: data.clone(),
                origin: "test",
                native_family: None,
                full_name: format!("{family} {style}"),
                postscript: None,
            })
            .collect()
    }
}

#[test]
fn japanese_fallback_ignores_loaded_faces_and_preserves_weight() {
    fonts::add_source(&SyntheticFonts);
    let prior = fonts::resolve("Already Loaded", "Regular").face;
    assert!(fonts::face(prior).has_char('キ'));
    for style in ["Regular", "Bold", "Regular", "Bold"] {
        let primary = fonts::resolve("Inter", style).face;
        let actual = fonts::face(fonts::fallback_for('キ', primary));
        assert_eq!(actual.info.family, "Yu Gothic");
        assert_eq!(actual.info.style, style);
    }
}

fn blank_glyph_font() -> Vec<u8> {
    fn u16s(v: &[u16]) -> Vec<u8> {
        v.iter().flat_map(|x| x.to_be_bytes()).collect()
    }
    let delta_space = 1u16.wrapping_sub(0x20);
    let cmap4 = {
        let mut t = u16s(&[4, 0, 0, 6, 4, 1, 2]);
        t.extend(u16s(&[0x20, 0x30AD, 0xFFFF, 0, 0x20, 0x30AC, 0xFFFF, delta_space, 0xCF55, 1, 0, 0, 0]));
        let len = t.len() as u16;
        t[2..4].copy_from_slice(&len.to_be_bytes());
        t
    };
    let mut cmap = u16s(&[0, 1, 3, 1, 0, 12]);
    cmap.extend(cmap4);
    // glyph 2: one contour of three on-curve points (100,0) (500,0) (300,600)
    let mut glyf = u16s(&[1, 100, 0, 500, 600, 2, 0]);
    glyf.extend([1u8, 1, 1]);
    glyf.extend(u16s(&[100, 400, (-200i16) as u16, 0, 0, 600]));
    glyf.push(0);
    let loca = u16s(&[0, 0, 0, (glyf.len() / 2) as u16]);
    let mut head = u16s(&[1, 0, 1, 0, 0, 0, 0x5F0F, 0x3CF5, 0, 1000]);
    head.extend([0u8; 16]);
    head.extend(u16s(&[100, 0, 500, 600, 0, 8, 2, 0, 0]));
    let mut hhea = u16s(&[1, 0, 800, (-200i16) as u16, 0, 600, 0, 0, 500, 1, 0, 0]);
    hhea.extend([0u8; 8]);
    hhea.extend(u16s(&[0, 3]));
    let hmtx = u16s(&[600, 0, 600, 0, 600, 0]);
    let maxp = u16s(&[1, 0, 3, 3, 1, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0]);
    let strings: Vec<Vec<u8>> = ["Blank Glyph Test", "Regular"].iter().map(|s| s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()).collect();
    let mut name = u16s(&[0, 2, 30]);
    let mut off = 0;
    for (id, st) in strings.iter().enumerate() {
        name.extend(u16s(&[3, 1, 0x409, id as u16 + 1, st.len() as u16, off]));
        off += st.len() as u16;
    }
    strings.iter().for_each(|st| name.extend(st));
    let tables: [(&[u8; 4], Vec<u8>); 8] =
        [(b"cmap", cmap), (b"glyf", glyf), (b"head", head), (b"hhea", hhea), (b"hmtx", hmtx), (b"loca", loca), (b"maxp", maxp), (b"name", name)];
    let mut out = u16s(&[1, 0, tables.len() as u16, 128, 3, 0]);
    let mut offset = 12 + 16 * tables.len();
    let mut body = vec![];
    for (tag, data) in &tables {
        out.extend(*tag);
        out.extend([0u8; 4]);
        out.extend((offset as u32).to_be_bytes());
        out.extend((data.len() as u32).to_be_bytes());
        let padded = data.len().div_ceil(4) * 4;
        body.extend(data);
        body.resize(body.len() + padded - data.len(), 0);
        offset += padded;
    }
    out.extend(body);
    out
}
