//! Text extraction from a PDF, done locally so a statement never leaves the machine.
//! Cells are decoded through each font's ToUnicode map (two-byte codes for CID fonts),
//! positioned with the text matrix, grouped into visual rows by baseline, and joined
//! left to right. Nothing here logs or returns page text; callers decide what to keep.

use std::path::Path;

use anyhow::{Context, Result, bail};
use pdf::content::{Op, TextDrawAdjusted};
use pdf::file::FileOptions;
use pdf::font::ToUnicodeMap;

#[derive(Debug, Clone, Copy)]
struct Matrix {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    e: f32,
    f: f32,
}

impl Matrix {
    const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    fn translate(tx: f32, ty: f32) -> Self {
        Self {
            e: tx,
            f: ty,
            ..Self::IDENTITY
        }
    }

    /// self × other (PDF convention: row vectors, so translation applies first).
    fn then(self, other: Self) -> Self {
        Self {
            a: self.a * other.a + self.b * other.c,
            b: self.a * other.b + self.b * other.d,
            c: self.c * other.a + self.d * other.c,
            d: self.c * other.b + self.d * other.d,
            e: self.e * other.a + self.f * other.c + other.e,
            f: self.e * other.b + self.f * other.d + other.f,
        }
    }
}

struct Cell {
    page: usize,
    x: f32,
    y: f32,
    seq: usize,
    text: String,
}

struct FontState {
    map: Option<ToUnicodeMap>,
    two_byte: bool,
}

fn decode(bytes: &[u8], font: &FontState, unmapped: &mut usize) -> String {
    let Some(map) = &font.map else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    let codes: Vec<u16> = if font.two_byte {
        bytes
            .chunks(2)
            .map(|c| u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]))
            .collect()
    } else {
        bytes.iter().map(|b| *b as u16).collect()
    };
    let mut out = String::new();
    for code in codes {
        match map.get(code) {
            Some(s) => out.push_str(s),
            None => {
                *unmapped += 1;
                out.push('\u{fffd}');
            }
        }
    }
    out
}

pub struct Extracted {
    /// Visual rows, page by page, top to bottom, cells joined with single spaces.
    pub lines: Vec<String>,
    pub pages: usize,
    /// Glyphs the fonts could not map to text; non-zero means some characters are missing.
    pub unmapped_glyphs: usize,
}

pub fn extract_lines(path: &Path) -> Result<Extracted> {
    let file = FileOptions::cached()
        .open(path)
        .with_context(|| format!("open PDF {}", path.display()))?;
    let resolver = file.resolver();
    let mut cells: Vec<Cell> = Vec::new();
    let mut unmapped = 0usize;
    let mut pages = 0usize;

    for (page_index, page) in file.pages().enumerate() {
        let page = page.with_context(|| format!("read page {}", page_index + 1))?;
        pages += 1;
        let Some(content) = page.contents.as_ref() else {
            continue;
        };
        let ops = content
            .operations(&resolver)
            .with_context(|| format!("parse page {} content", page_index + 1))?;
        let resources = page
            .resources()
            .with_context(|| format!("page {} resources", page_index + 1))?;

        let mut tm = Matrix::IDENTITY;
        let mut tlm = Matrix::IDENTITY;
        let mut leading = 0.0f32;
        let mut font = FontState {
            map: None,
            two_byte: false,
        };
        let mut seq = 0usize;
        let push = |tm: &Matrix, text: String, cells: &mut Vec<Cell>, seq: &mut usize| {
            if !text.trim().is_empty() {
                cells.push(Cell {
                    page: page_index,
                    x: tm.e,
                    y: tm.f,
                    seq: *seq,
                    text,
                });
                *seq += 1;
            }
        };
        for op in ops {
            match op {
                Op::BeginText => {
                    tm = Matrix::IDENTITY;
                    tlm = Matrix::IDENTITY;
                }
                Op::SetTextMatrix { matrix } => {
                    tlm = Matrix {
                        a: matrix.a,
                        b: matrix.b,
                        c: matrix.c,
                        d: matrix.d,
                        e: matrix.e,
                        f: matrix.f,
                    };
                    tm = tlm;
                }
                Op::MoveTextPosition { translation } => {
                    tlm = Matrix::translate(translation.x, translation.y).then(tlm);
                    tm = tlm;
                }
                Op::Leading { leading: l } => leading = l,
                Op::TextNewline => {
                    tlm = Matrix::translate(0.0, -leading).then(tlm);
                    tm = tlm;
                }
                Op::TextFont { name, .. } => {
                    let loaded = resources
                        .fonts
                        .get(&name)
                        .and_then(|lazy| lazy.load(&resolver).ok());
                    font = FontState {
                        two_byte: loaded.as_ref().is_some_and(|f| f.is_cid()),
                        map: loaded.and_then(|f| f.to_unicode(&resolver).and_then(|r| r.ok())),
                    };
                }
                Op::TextDraw { text } => {
                    let s = decode(text.as_bytes(), &font, &mut unmapped);
                    push(&tm, s, &mut cells, &mut seq);
                }
                Op::TextDrawAdjusted { array } => {
                    let mut s = String::new();
                    for item in array {
                        if let TextDrawAdjusted::Text(t) = item {
                            s.push_str(&decode(t.as_bytes(), &font, &mut unmapped));
                        }
                    }
                    push(&tm, s, &mut cells, &mut seq);
                }
                _ => {}
            }
        }
    }
    if cells.is_empty() {
        bail!("no text found in {} (scanned image PDF?)", path.display());
    }

    // Group into rows: same page, baseline within 2pt; order rows top-down, cells left-right.
    cells.sort_by(|a, b| {
        a.page
            .cmp(&b.page)
            .then(b.y.partial_cmp(&a.y).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.x.partial_cmp(&b.x).unwrap_or(std::cmp::Ordering::Equal))
            .then(a.seq.cmp(&b.seq))
    });
    let mut lines: Vec<String> = Vec::new();
    let mut current: Vec<&Cell> = Vec::new();
    let flush = |current: &mut Vec<&Cell>, lines: &mut Vec<String>| {
        if current.is_empty() {
            return;
        }
        current.sort_by(|a, b| {
            a.x.partial_cmp(&b.x)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.seq.cmp(&b.seq))
        });
        let line = current
            .iter()
            .map(|c| c.text.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        lines.push(line);
        current.clear();
    };
    for cell in &cells {
        let same_row = current
            .last()
            .is_some_and(|last| last.page == cell.page && (last.y - cell.y).abs() <= 2.0);
        if !same_row {
            flush(&mut current, &mut lines);
        }
        current.push(cell);
    }
    flush(&mut current, &mut lines);
    Ok(Extracted {
        lines,
        pages,
        unmapped_glyphs: unmapped,
    })
}
