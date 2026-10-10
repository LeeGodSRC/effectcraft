//! Bounded reader for the dictionary stored in AEP's opaque text-document blob.
use super::ChunkNode;
use effectcraft_keyframe::{Composer, Justify, ParaStyle, StyleRun, TextDoc};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
enum Entry {
    Dict(BTreeMap<String, Entry>),
    Array(Vec<Entry>),
    String(String),
    Number(f64),
    Bool(bool),
    Name,
}
impl Entry {
    fn get(&self, key: &str) -> Option<&Self> {
        if let Self::Dict(d) = self { d.get(key) } else { None }
    }
    fn at(&self, index: usize) -> Option<&Self> {
        if let Self::Array(a) = self { a.get(index) } else { None }
    }
    fn number(&self) -> Option<f64> {
        if let Self::Number(n) = self { Some(*n) } else { None }
    }
    fn boolean(&self) -> Option<bool> {
        if let Self::Bool(b) = self { Some(*b) } else { None }
    }
    fn string(&self) -> Option<&str> {
        if let Self::String(s) = self { Some(s) } else { None }
    }
}
struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    count: usize,
}
impl Parser<'_> {
    fn whitespace(&mut self) {
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }
    fn starts(&self, prefix: &[u8]) -> bool {
        self.bytes.get(self.at..).is_some_and(|b| b.starts_with(prefix))
    }
    fn token(&mut self) -> String {
        let start = self.at;
        while self.bytes.get(self.at).is_some_and(|b| !b.is_ascii_whitespace() && !b"[]<>()/".contains(b)) {
            self.at += 1;
        }
        String::from_utf8_lossy(self.bytes.get(start..self.at).unwrap_or(&[])).into_owned()
    }
    fn dictionary(&mut self, depth: usize, explicit: bool) -> Result<Entry, String> {
        let mut out = BTreeMap::new();
        loop {
            self.whitespace();
            if explicit && self.starts(b">>") {
                self.at += 2;
                break;
            }
            if self.at >= self.bytes.len() {
                if explicit {
                    return Err("unterminated AEP text dictionary".into());
                }
                break;
            }
            if !self.starts(b"/") {
                return Err("invalid AEP text dictionary key".into());
            }
            self.at += 1;
            let key = self.token();
            if key.is_empty() {
                return Err("empty AEP text dictionary key".into());
            }
            out.insert(key, self.value(depth + 1)?);
        }
        Ok(Entry::Dict(out))
    }
    fn value(&mut self, depth: usize) -> Result<Entry, String> {
        self.count = self.count.saturating_add(1);
        if depth > 64 || self.count > 100_000 {
            return Err("AEP text document exceeds nesting or value limit".into());
        }
        self.whitespace();
        if self.starts(b"<<") {
            self.at += 2;
            return self.dictionary(depth, true);
        }
        if self.starts(b"[") {
            self.at += 1;
            let mut out = Vec::new();
            loop {
                self.whitespace();
                if self.starts(b"]") {
                    self.at += 1;
                    break;
                }
                if self.at >= self.bytes.len() {
                    return Err("unterminated AEP text array".into());
                }
                out.push(self.value(depth + 1)?);
            }
            return Ok(Entry::Array(out));
        }
        if self.starts(b"(") {
            self.at += 1;
            let mut bytes = Vec::new();
            let mut nesting = 1usize;
            while let Some(b) = self.bytes.get(self.at).copied() {
                self.at += 1;
                if b == b'\\' {
                    let next = self.bytes.get(self.at).copied().ok_or("truncated AEP text escape")?;
                    self.at += 1;
                    bytes.push(match next {
                        b'n' => b'\n',
                        b'r' => b'\r',
                        b't' => b'\t',
                        _ => next,
                    });
                } else if b == b'(' {
                    nesting += 1;
                    if nesting > 64 {
                        return Err("AEP text string nesting exceeds limit".into());
                    }
                    bytes.push(b);
                } else if b == b')' {
                    nesting -= 1;
                    if nesting == 0 {
                        break;
                    }
                    bytes.push(b);
                } else {
                    bytes.push(b);
                }
            }
            if nesting != 0 {
                return Err("unterminated AEP text string".into());
            }
            let text = if bytes.starts_with(b"\xfe\xff") {
                let data = bytes.get(2..).unwrap_or(&[]);
                if !data.len().is_multiple_of(2) {
                    return Err("invalid UTF-16 AEP text string".into());
                }
                String::from_utf16_lossy(&data.as_chunks::<2>().0.iter().map(|p| u16::from_be_bytes(*p)).collect::<Vec<_>>())
            } else {
                String::from_utf8_lossy(&bytes).into_owned()
            };
            return Ok(Entry::String(text));
        }
        if self.starts(b"/") {
            self.at += 1;
            self.token();
            return Ok(Entry::Name);
        }
        let token = self.token();
        if token == "true" {
            return Ok(Entry::Bool(true));
        }
        if token == "false" {
            return Ok(Entry::Bool(false));
        }
        let number = token.parse::<f64>().map_err(|_| "invalid AEP text value")?;
        if !number.is_finite() || number.abs() > 1.0e12 {
            return Err("invalid AEP text number".into());
        }
        Ok(Entry::Number(number))
    }
}

fn path<'a>(mut e: &'a Entry, keys: &[&str]) -> Option<&'a Entry> {
    for key in keys {
        e = e.get(key)?;
    }
    Some(e)
}
fn first_run(e: &Entry) -> Option<&Entry> {
    e.get("0")?.at(0)?.get("0")?.get("0")
}
fn paint(e: &Entry) -> Option<[f32; 4]> {
    let array = path(e, &["0", "1"])?;
    Some([array.at(1)?.number()? as f32, array.at(2)?.number()? as f32, array.at(3)?.number()? as f32, array.at(0)?.number()? as f32])
}

pub(super) fn decode(nodes: &[ChunkNode<'_>]) -> Result<Option<TextDoc>, String> {
    fn find<'a>(nodes: &[ChunkNode<'a>], depth: usize) -> Option<&'a [u8]> {
        if depth > 64 {
            return None;
        }
        for node in nodes {
            match node {
                ChunkNode::Chunk { id: [b'b', b't', b'd', b'k'], data } => return Some(data),
                ChunkNode::List { children, .. } => {
                    if let Some(bytes) = find(children, depth + 1) {
                        return Some(bytes);
                    }
                }
                _ => {}
            }
        }
        None
    }
    let Some(bytes) = find(nodes, 0) else { return Ok(None) };
    decode_blob(bytes).map(Some)
}

fn decode_blob(bytes: &[u8]) -> Result<TextDoc, String> {
    if bytes.len() > 8 * 1024 * 1024 {
        return Err("AEP text document exceeds 8 MiB".into());
    }
    let root = Parser { bytes, at: 0, count: 0 }.dictionary(0, false)?;
    let element = path(&root, &["1", "1"]).and_then(|a| a.at(0));
    let story = element.and_then(|e| e.get("0")).ok_or("missing AEP text story")?;
    let source = story.get("0").and_then(Entry::string).ok_or("missing AEP source text")?;
    let text = source.strip_suffix('\r').unwrap_or(source).replace('\r', "\n");
    let mut doc = TextDoc { text, composer: Composer::SingleLine, ..Default::default() };
    if let Some(style) = story.get("6").and_then(first_run).and_then(|e| e.get("6")) {
        apply_style(&root, style, &mut doc);
    }
    if let Some(Entry::Array(runs)) = path(story, &["6", "0"])
        && runs.len() > 1
    {
        let mut characters = doc.text.chars().peekable();
        for run in runs {
            let Some(style) = path(run, &["0", "0", "6"]) else { continue };
            let units = run.get("1").and_then(Entry::number).filter(|n| *n >= 0.0 && *n <= 1_000_000.0).ok_or("invalid AEP text style length")? as usize;
            let mut consumed = 0usize;
            let mut len = 0usize;
            while let Some(c) = characters.peek() {
                if consumed.saturating_add(c.len_utf16()) > units {
                    break;
                }
                consumed += c.len_utf16();
                len += 1;
                characters.next();
            }
            let mut styled = TextDoc::default();
            apply_style(&root, style, &mut styled);
            if len > 0 {
                doc.runs.push(StyleRun { len, style: styled.base_style() });
            }
        }
    }
    if let Some(paragraph) = story.get("5").and_then(first_run).and_then(|e| e.get("5")) {
        let para = paragraph_style(paragraph);
        doc.justify = para.justify;
        doc.indent_first = para.indent_first;
        doc.indent_left = para.indent_left;
        doc.indent_right = para.indent_right;
        doc.space_before = para.space_before;
        doc.space_after = para.space_after;
        doc.composer = para.composer;
    }
    if let Some(Entry::Array(runs)) = path(story, &["5", "0"]) {
        let mut characters = doc.text.chars().peekable();
        let mut at_start = true;
        for run in runs {
            let Some(para) = path(run, &["0", "0", "5"]) else { continue };
            let style = paragraph_style(para);
            let units = run.get("1").and_then(Entry::number).filter(|n| *n >= 0.0 && *n <= 1_000_000.0).unwrap_or(1_000_000.0) as usize;
            let mut consumed = 0usize;
            while let Some(c) = characters.peek().copied() {
                if consumed.saturating_add(c.len_utf16()) > units {
                    break;
                }
                if at_start {
                    doc.paragraphs.push(style.clone());
                }
                consumed += c.len_utf16();
                at_start = c == '\n';
                characters.next();
            }
        }
    }
    // Paragraph text: the first frame of the story's view carries the box as its origin (from
    // the layer's anchor) and a rectangle; point text has neither.
    if let Some(frame) = element.and_then(|e| path(e, &["1", "2"])).and_then(|views| views.at(0)).and_then(|view| view.get("6")).and_then(|frames| frames.at(0))
    {
        let number = |e: Option<&Entry>| e.and_then(Entry::number).filter(|n| n.is_finite() && n.abs() <= 1.0e7);
        let origin = path(frame, &["0", "0"]);
        let rect = frame.get("1");
        if let (Some(x), Some(y), Some(left), Some(top), Some(right), Some(bottom)) = (
            number(origin.and_then(|o| o.at(0))),
            number(origin.and_then(|o| o.at(1))),
            number(rect.and_then(|r| r.at(0))),
            number(rect.and_then(|r| r.at(1))),
            number(rect.and_then(|r| r.at(2))),
            number(rect.and_then(|r| r.at(3))),
        ) && right - left >= 1.0
            && bottom - top >= 1.0
        {
            doc.box_pos = [x + left, y + top];
            doc.box_size = Some([right - left, bottom - top]);
        }
    }
    Ok(doc)
}

fn paragraph_style(paragraph: &Entry) -> ParaStyle {
    let number = |key| paragraph.get(key).and_then(Entry::number).unwrap_or(0.0);
    ParaStyle {
        justify: match number("0") as u32 {
            1 => Justify::Right,
            2 => Justify::Center,
            3 => Justify::JustifyLastLeft,
            4 => Justify::JustifyLastRight,
            5 => Justify::JustifyLastCenter,
            6 => Justify::JustifyAll,
            _ => Justify::Left,
        },
        indent_first: number("1"),
        indent_left: number("2"),
        indent_right: number("3"),
        space_before: number("4"),
        space_after: number("5"),
        // The composer choice is not decoded yet. After Effects' default fills each line in
        // turn, and so do the line breaks stored with observed paragraph text.
        composer: Composer::SingleLine,
        ..Default::default()
    }
}

fn apply_style(root: &Entry, style: &Entry, doc: &mut TextDoc) {
    if let Some(index) = style.get("0").and_then(Entry::number).filter(|n| *n >= 0.0 && *n < 4096.0)
        && let Some(font) = path(root, &["0", "1", "0"]).and_then(|e| e.at(index as usize)).and_then(|e| path(e, &["0", "0", "0"])).and_then(Entry::string)
    {
        doc.font = font.to_string();
    }
    if let Some(value) = style.get("1").and_then(Entry::number).filter(|n| *n > 0.0 && *n <= 100_000.0) {
        doc.size = value;
    }
    doc.faux_bold = style.get("2").and_then(Entry::boolean).unwrap_or(false);
    doc.faux_italic = style.get("3").and_then(Entry::boolean).unwrap_or(false);
    doc.leading = if style.get("4").and_then(Entry::boolean).unwrap_or(true) { None } else { style.get("5").and_then(Entry::number) };
    doc.h_scale = style.get("6").and_then(Entry::number).unwrap_or(1.0) * 100.0;
    doc.v_scale = style.get("7").and_then(Entry::number).unwrap_or(1.0) * 100.0;
    doc.tracking = style.get("8").and_then(Entry::number).unwrap_or(0.0);
    doc.baseline_shift = style.get("9").and_then(Entry::number).unwrap_or(0.0);
    // The character case enum belongs to each run, not to the source string. Keep the
    // original spelling so editing text and character-indexed animators still see it.
    let caps = style.get("12").and_then(Entry::number).unwrap_or(0.0);
    doc.small_caps = caps == 1.0;
    doc.all_caps = caps == 2.0;
    doc.fill = style.get("53").and_then(paint).unwrap_or([1.0; 4]);
    doc.stroke = style.get("54").and_then(paint).unwrap_or([0.0, 0.0, 0.0, 1.0]);
    doc.apply_fill = style.get("56").and_then(Entry::boolean).unwrap_or(true);
    doc.apply_stroke = style.get("57").and_then(Entry::boolean).unwrap_or(false);
    doc.stroke_over_fill = style.get("58").and_then(Entry::boolean).unwrap_or(true);
    doc.stroke_width = style.get("63").and_then(Entry::number).unwrap_or(0.0);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn character_case_and_mixed_styles_survive_import() {
        let blob = br"/0 << /1 << /0 [ << /0 << /0 << /0 (Inter) >> >> >> << /0 << /0 << /0 (NotoSerif-Regular) >> >> >> ] >> >>
            /1 << /1 [ << /0 << /0 (Ab cd\r)
            /6 << /0 [
                << /0 << /0 << /6 << /0 0 /1 40 /2 true /3 true /8 120 /9 7 /12 2 >> >> >> /1 3 >>
                << /0 << /0 << /6 << /0 1 /1 20 /12 1 >> >> >> /1 3 >>
            ] >> /5 << /0 [ << /0 << /0 << /5 << /0 2 >> >> >> /1 6 >> ] >> >> >> ] >>";
        let doc = decode_blob(blob).unwrap();
        assert_eq!(doc.text, "Ab cd");
        assert!(doc.all_caps && !doc.small_caps);
        assert_eq!(doc.justify, Justify::Center);
        assert_eq!(doc.runs.len(), 2);
        let first = &doc.runs[0];
        assert_eq!(first.len, 3);
        assert_eq!(first.style.font, "Inter");
        assert_eq!(first.style.size, 40.0);
        assert!(first.style.all_caps && first.style.faux_bold && first.style.faux_italic);
        assert_eq!((first.style.tracking, first.style.baseline_shift), (120.0, 7.0));
        let second = &doc.runs[1];
        assert_eq!(second.len, 2);
        assert_eq!(second.style.font, "NotoSerif-Regular");
        assert_eq!(second.style.size, 20.0);
        assert!(second.style.small_caps && !second.style.all_caps);
        let layout = effectcraft_text::layout_doc(&doc);
        let capitalized = TextDoc { text: "AB cd".into(), ..doc.clone() };
        assert_eq!(layout.layout.bounds, effectcraft_text::layout_doc(&capitalized).layout.bounds);
    }
    #[test]
    fn text_dictionary_preserves_escaped_unicode_and_style() {
        let blob = b"/0 << /1 << /0 [ << /0 << /0 << /0 (OriginalFont) >> >> >> ] >> >> /1 << /1 [ << /0 << /0 (Hello \\(world\\)\\r) /6 << /0 [ << /0 << /0 << /6 << /0 0 /1 48 /53 << /0 << /1 [ 1 0.2 0.4 0.6 ] >> >> >> >> >> >> ] >> /5 << /0 [ << /0 << /0 << /5 << /0 2 >> >> >> >> ] >> >> >> ] >>";
        let doc = decode_blob(blob).unwrap();
        assert_eq!(doc.text, "Hello (world)");
        assert_eq!(doc.size, 48.0);
        assert_eq!(doc.fill, [0.2, 0.4, 0.6, 1.0]);
        assert_eq!(doc.justify, Justify::Center);
    }
    #[test]
    fn styled_runs_count_unicode_characters_and_preserve_blank_line() {
        let mut blob = b"/1 << /1 [ << /0 << /0 (".to_vec();
        blob.extend_from_slice(&[0xfe, 0xff]);
        for unit in "A😀B\r\r".encode_utf16() {
            blob.extend_from_slice(&unit.to_be_bytes());
        }
        blob.extend_from_slice(b") /6 << /0 [ << /0 << /0 << /6 << /1 24 >> >> >> /1 3 >> << /0 << /0 << /6 << /1 12 >> >> >> /1 3 >> ] >> >> >> ] >>");
        let doc = decode_blob(&blob).unwrap();
        assert_eq!(doc.text, "A😀B\n");
        assert_eq!(doc.runs.len(), 2);
        assert_eq!(doc.runs[0].len, 2);
        assert_eq!(doc.runs[0].style.size, 24.0);
        assert_eq!(doc.runs[1].len, 2);
        assert_eq!(doc.runs[1].style.size, 12.0);
    }
    #[test]
    fn paragraph_spacing_preserves_multiline_baselines() {
        let blob = b"/1 << /1 [ << /0 << /0 (First\rSecond\r) /6 << /0 [ << /0 << /0 << /6 << /1 20 /4 false /5 3 >> >> >> /1 13 >> ] >> /5 << /0 [ << /0 << /0 << /5 << /0 0 /4 20 >> >> >> /1 13 >> ] >> >> >> ] >>";
        let doc = decode_blob(blob).unwrap();
        assert_eq!(doc.leading, Some(3.0));
        assert_eq!(doc.space_before, 20.0);
        assert_eq!(doc.paragraphs.len(), 2);
        let layout = effectcraft_text::layout_doc(&doc);
        assert_eq!(layout.line_boxes.len(), 2);
        assert_eq!(layout.line_boxes[0][1], 0.0);
        assert_eq!(layout.line_boxes[1][1], 23.0);
    }
    #[test]
    fn paragraph_text_keeps_its_box() {
        let blob = b"/1 << /1 [ << /0 << /0 (Box\r) >> /1 << /2 [ << /6 [ << /0 << /0 [ -218.0 -277.0 ] >> /1 [ 0.0 0.0 344.0 562.0 ] >> ] >> ] >> >> ] >>";
        let doc = decode_blob(blob).unwrap();
        assert_eq!(doc.box_size, Some([344.0, 562.0]));
        assert_eq!(doc.box_pos, [-218.0, -277.0]);
        let point = decode_blob(b"/1 << /1 [ << /0 << /0 (Point\r) >> /1 << /2 [ << /6 [ << /5 2 >> ] >> ] >> >> ] >>").unwrap();
        assert_eq!(point.box_size, None);
    }
    #[test]
    fn malformed_text_is_an_error() {
        for bytes in [b"/0 (bad".as_slice(), b"/0 [", b"/0 << /1 2", b"/0 NaN"] {
            assert!(decode_blob(bytes).is_err());
        }
    }
}
