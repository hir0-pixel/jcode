//! Plain-text extraction for xlsx/docx/pptx (OOXML zips) without extra deps:
//! a minimal zip reader on top of flate2 plus a tiny XML tokenizer.

use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::fmt::Write;
use std::io::Read;
use std::path::Path;

const MAX_PART: u64 = 64 * 1024 * 1024;

pub fn is_office_file(path: &Path) -> bool {
    matches!(ext(path).as_str(), "xlsx" | "xlsm" | "docx" | "pptx")
}

fn ext(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn extract(path: &Path) -> Result<String> {
    let zip = Zip::open(std::fs::read(path)?)?;
    match ext(path).as_str() {
        "docx" => docx(&zip),
        "pptx" => pptx(&zip),
        _ => xlsx(&zip),
    }
}

// ---- zip ----

struct Entry {
    method: u16,
    csize: usize,
    offset: usize,
}

struct Zip {
    data: Vec<u8>,
    entries: HashMap<String, Entry>,
}

fn u16le(b: &[u8], i: usize) -> usize {
    u16::from_le_bytes([b[i], b[i + 1]]) as usize
}
fn u32le(b: &[u8], i: usize) -> usize {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize
}

impl Zip {
    fn open(data: Vec<u8>) -> Result<Self> {
        let bad = || anyhow!("not a valid OOXML zip file");
        let eocd = (0..data.len().saturating_sub(21))
            .rev()
            .take(70_000)
            .find(|&i| data[i..i + 4] == [0x50, 0x4b, 5, 6])
            .ok_or_else(bad)?;
        let n = u16le(&data, eocd + 10);
        let mut p = u32le(&data, eocd + 16);
        let mut entries = HashMap::new();
        for _ in 0..n {
            if p + 46 > data.len() || data[p..p + 4] != [0x50, 0x4b, 1, 2] {
                return Err(bad());
            }
            let (nl, el, cl) = (u16le(&data, p + 28), u16le(&data, p + 30), u16le(&data, p + 32));
            let name = data.get(p + 46..p + 46 + nl).ok_or_else(bad)?;
            entries.insert(
                String::from_utf8_lossy(name).into_owned(),
                Entry { method: u16le(&data, p + 10) as u16, csize: u32le(&data, p + 20), offset: u32le(&data, p + 42) },
            );
            p += 46 + nl + el + cl;
        }
        Ok(Self { data, entries })
    }

    fn names(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    fn read(&self, name: &str) -> Option<String> {
        let e = self.entries.get(name)?;
        let d = &self.data;
        let h = e.offset;
        let start = h.checked_add(30 + u16le(d, h + 26) + u16le(d, h + 28))?;
        let raw = d.get(start..start.checked_add(e.csize)?)?;
        let mut out = Vec::new();
        match e.method {
            0 => out.extend_from_slice(raw),
            8 => {
                flate2::read::DeflateDecoder::new(raw).take(MAX_PART).read_to_end(&mut out).ok()?;
            }
            _ => return None,
        }
        Some(String::from_utf8_lossy(&out).into_owned())
    }
}

// ---- xml ----

enum Ev<'a> {
    Open(&'a str, &'a str),
    Close(&'a str),
    Text(&'a str),
}

fn scan<'a>(xml: &'a str, mut f: impl FnMut(Ev<'a>)) {
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        if lt > 0 {
            f(Ev::Text(&rest[..lt]));
        }
        rest = &rest[lt..];
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map_or("", |i| &rest[i + 3..]);
            continue;
        }
        let Some(gt) = rest.find('>') else { return };
        let tag = &rest[1..gt];
        rest = &rest[gt + 1..];
        if tag.starts_with('?') || tag.starts_with('!') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            f(Ev::Close(name.trim()));
            continue;
        }
        let self_close = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let (name, attrs) = tag.split_at(tag.find(char::is_whitespace).unwrap_or(tag.len()));
        f(Ev::Open(name, attrs));
        if self_close {
            f(Ev::Close(name));
        }
    }
    if !rest.is_empty() {
        f(Ev::Text(rest));
    }
}

fn attr(attrs: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let s = attrs.find(&key)? + key.len();
    let e = attrs[s..].find('"')?;
    Some(unescape(&attrs[s..s + e]))
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';') else { break };
        let ent = &rest[1..semi];
        match ent {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ => {
                let n = ent
                    .strip_prefix("#x")
                    .and_then(|h| u32::from_str_radix(h, 16).ok())
                    .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()));
                match n.and_then(char::from_u32) {
                    Some(c) => out.push(c),
                    None => out.push_str(&rest[..=semi]),
                }
            }
        }
        rest = &rest[semi + 1..];
    }
    out.push_str(rest);
    out
}

// ---- formats ----

fn trailing_num(name: &str) -> u32 {
    let digits: String = name.trim_end_matches(".xml").chars().rev().take_while(|c| c.is_ascii_digit()).collect();
    digits.chars().rev().collect::<String>().parse().unwrap_or(0)
}

fn pptx(zip: &Zip) -> Result<String> {
    let mut slides: Vec<&String> =
        zip.names().filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml")).collect();
    slides.sort_by_key(|n| trailing_num(n));
    let mut out = String::new();
    for (i, name) in slides.iter().enumerate() {
        let _ = writeln!(out, "--- Slide {} ---", i + 1);
        let xml = zip.read(name).unwrap_or_default();
        let mut in_t = false;
        scan(&xml, |ev| match ev {
            Ev::Open("a:t", _) => in_t = true,
            Ev::Close("a:t") => in_t = false,
            Ev::Text(t) if in_t => out.push_str(&unescape(t)),
            Ev::Close("a:p") => out.push('\n'),
            _ => {}
        });
    }
    Ok(out)
}

fn docx(zip: &Zip) -> Result<String> {
    let xml = zip.read("word/document.xml").ok_or_else(|| anyhow!("word/document.xml missing"))?;
    let (mut out, mut para, mut cell, mut row) = (String::new(), String::new(), String::new(), String::new());
    let (mut in_t, mut in_tc, mut first_cell) = (false, false, true);
    scan(&xml, |ev| match ev {
        Ev::Open("w:t", _) => in_t = true,
        Ev::Close("w:t") => in_t = false,
        Ev::Text(t) if in_t => para.push_str(&unescape(t)),
        Ev::Open("w:tab", _) => para.push('\t'),
        Ev::Open("w:br", _) => para.push(' '),
        Ev::Open("w:tr", _) => {
            row.clear();
            first_cell = true;
        }
        Ev::Open("w:tc", _) => {
            in_tc = true;
            cell.clear();
        }
        Ev::Close("w:p") => {
            if in_tc {
                if !cell.is_empty() && !para.is_empty() {
                    cell.push(' ');
                }
                cell.push_str(&para);
            } else {
                out.push_str(&para);
                out.push('\n');
            }
            para.clear();
        }
        Ev::Close("w:tc") => {
            in_tc = false;
            if !first_cell {
                row.push('\t');
            }
            first_cell = false;
            row.push_str(&cell);
        }
        Ev::Close("w:tr") => {
            out.push_str(&row);
            out.push('\n');
        }
        _ => {}
    });
    Ok(out)
}

fn xlsx(zip: &Zip) -> Result<String> {
    let mut shared: Vec<String> = Vec::new();
    if let Some(xml) = zip.read("xl/sharedStrings.xml") {
        let (mut in_t, mut cur) = (false, String::new());
        scan(&xml, |ev| match ev {
            Ev::Open("si", _) => cur.clear(),
            Ev::Open("t", _) => in_t = true,
            Ev::Close("t") => in_t = false,
            Ev::Text(t) if in_t => cur.push_str(&unescape(t)),
            Ev::Close("si") => shared.push(std::mem::take(&mut cur)),
            _ => {}
        });
    }
    let mut targets: HashMap<String, String> = HashMap::new();
    if let Some(xml) = zip.read("xl/_rels/workbook.xml.rels") {
        scan(&xml, |ev| {
            if let Ev::Open("Relationship", a) = ev
                && let (Some(id), Some(t)) = (attr(a, "Id"), attr(a, "Target"))
            {
                targets.insert(id, t.trim_start_matches('/').trim_start_matches("xl/").to_string());
            }
        });
    }
    let wb = zip.read("xl/workbook.xml").ok_or_else(|| anyhow!("xl/workbook.xml missing"))?;
    let mut sheets = Vec::new();
    scan(&wb, |ev| {
        if let Ev::Open("sheet", a) = ev {
            let name = attr(a, "name").unwrap_or_default();
            let part = attr(a, "r:id").and_then(|id| targets.get(&id).cloned());
            sheets.push((name, part));
        }
    });
    let mut out = String::new();
    for (i, (name, part)) in sheets.iter().enumerate() {
        let _ = writeln!(out, "Sheet: {name}");
        let part = format!("xl/{}", part.clone().unwrap_or_else(|| format!("worksheets/sheet{}.xml", i + 1)));
        let xml = zip.read(&part).unwrap_or_default();
        let (mut cref, mut ty) = (String::new(), String::new());
        let (mut in_v, mut in_t, mut val, mut line) = (false, false, String::new(), String::new());
        scan(&xml, |ev| match ev {
            Ev::Open("row", _) => line.clear(),
            Ev::Open("c", a) => {
                cref = attr(a, "r").unwrap_or_default();
                ty = attr(a, "t").unwrap_or_default();
                val.clear();
            }
            Ev::Open("v", _) => in_v = true,
            Ev::Close("v") => in_v = false,
            Ev::Open("t", _) => in_t = true,
            Ev::Close("t") => in_t = false,
            Ev::Text(t) if in_v || in_t => val.push_str(&unescape(t)),
            Ev::Close("c") => {
                let v = match ty.as_str() {
                    "s" => val.trim().parse::<usize>().ok().and_then(|n| shared.get(n).cloned()).unwrap_or_default(),
                    "b" => if val.trim() == "1" { "TRUE".into() } else { "FALSE".into() },
                    _ => std::mem::take(&mut val),
                };
                if !v.is_empty() {
                    if !line.is_empty() {
                        line.push('\t');
                    }
                    let _ = write!(line, "{cref}={}", v.replace(['\n', '\t'], " "));
                }
            }
            Ev::Close("row") if !line.is_empty() => {
                out.push_str(&line);
                out.push('\n');
            }
            _ => {}
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Minimal stored-only zip writer.
    fn zip_bytes(parts: &[(&str, &str)]) -> Vec<u8> {
        let (mut out, mut cd) = (Vec::new(), Vec::new());
        for (name, body) in parts {
            let off = out.len() as u32;
            let (n, b) = (name.as_bytes(), body.as_bytes());
            let mut h = vec![0x50, 0x4b, 3, 4, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            h.extend((b.len() as u32).to_le_bytes());
            h.extend((b.len() as u32).to_le_bytes());
            h.extend((n.len() as u16).to_le_bytes());
            h.extend([0, 0]);
            out.extend(&h);
            out.extend(n);
            out.extend(b);
            let mut c = vec![0x50, 0x4b, 1, 2, 20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
            c.extend((b.len() as u32).to_le_bytes());
            c.extend((b.len() as u32).to_le_bytes());
            c.extend((n.len() as u16).to_le_bytes());
            c.extend([0u8; 12]);
            c.extend(off.to_le_bytes());
            c.extend(n);
            cd.extend(c);
        }
        let cd_off = out.len() as u32;
        let n = parts.len() as u16;
        out.extend(&cd);
        out.extend([0x50, 0x4b, 5, 6, 0, 0, 0, 0]);
        out.extend(n.to_le_bytes());
        out.extend(n.to_le_bytes());
        out.extend((cd.len() as u32).to_le_bytes());
        out.extend(cd_off.to_le_bytes());
        out.extend([0, 0]);
        out
    }

    fn run(file: &str, parts: &[(&str, &str)]) -> String {
        let p = std::env::temp_dir().join(format!("office_text_{}_{file}", std::process::id()));
        std::fs::File::create(&p).unwrap().write_all(&zip_bytes(parts)).unwrap();
        let r = extract(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        r
    }

    #[test]
    fn xlsx_cells() {
        let out = run("a.xlsx", &[
            ("xl/workbook.xml", r#"<workbook><sheets><sheet name="Data &amp; Co" sheetId="1" r:id="rId1"/></sheets></workbook>"#),
            ("xl/_rels/workbook.xml.rels", r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#),
            ("xl/sharedStrings.xml", "<sst><si><t>Name</t></si><si><r><t>Al</t></r><r><t>ice</t></r></si></sst>"),
            ("xl/worksheets/sheet1.xml", r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c><c r="B1" t="inlineStr"><is><t>x</t></is></c></row><row r="2"><c r="A2" t="s"><v>1</v></c><c r="B2"><f>1+2</f><v>3</v></c><c r="C2" t="b"><v>1</v></c></row></sheetData></worksheet>"#),
        ]);
        assert_eq!(out, "Sheet: Data & Co\nA1=Name\tB1=x\nA2=Alice\tB2=3\tC2=TRUE\n");
    }

    #[test]
    fn docx_paragraphs_and_tables() {
        let out = run("a.docx", &[("word/document.xml", "<w:document><w:body><w:p><w:r><w:t>Hello</w:t></w:r></w:p><w:tbl><w:tr><w:tc><w:p><w:r><w:t>a</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>b</w:t></w:r></w:p></w:tc></w:tr></w:tbl></w:body></w:document>")]);
        assert_eq!(out, "Hello\na\tb\n");
    }

    #[test]
    fn pptx_slides_in_order() {
        let s = |t: &str| format!("<p:sld><a:p><a:r><a:t>{t}</a:t></a:r></a:p></p:sld>");
        let (s2, s10) = (s("two"), s("ten"));
        let out = run("a.pptx", &[("ppt/slides/slide10.xml", &s10), ("ppt/slides/slide2.xml", &s2)]);
        assert_eq!(out, "--- Slide 1 ---\ntwo\n--- Slide 2 ---\nten\n");
    }
}
