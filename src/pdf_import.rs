//! Statement PDF → CSV, entirely on this machine. The PDF text is parsed and discarded;
//! only date / description / amount rows (descriptions scrubbed of long digit runs) are
//! written to the CSV and summarized back. Shared by the MCP tool and the CLI subcommand.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::money::to_decimal;
use crate::{csvio, pdf_text, statement};

#[derive(Debug, Serialize)]
pub struct SectionInfo {
    pub name: String,
    pub last4: Option<String>,
    pub rows: usize,
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub csv_path: String,
    pub section: SectionInfo,
    pub other_sections: Vec<SectionInfo>,
    pub period_start: Option<String>,
    pub period_end: Option<String>,
    pub rows: usize,
    pub first_date: Option<String>,
    pub last_date: Option<String>,
    pub total_amount: String,
    pub scrubbed_digit_runs: usize,
    pub unparsed_dated_lines: usize,
    pub unmapped_glyphs: usize,
    pub pages: usize,
    /// First three rows, so the caller can sanity-check the parse without the PDF.
    pub sample: Vec<SampleRow>,
}

#[derive(Debug, Serialize)]
pub struct SampleRow {
    pub date: String,
    pub description: String,
    pub amount: String,
}

pub struct Options<'a> {
    pub account: Option<&'a str>,
    pub year: Option<i32>,
    pub overwrite: bool,
}

pub fn convert(pdf: &Path, csv: &Path, opts: &Options<'_>) -> Result<Summary> {
    if csv.exists() && !opts.overwrite {
        bail!("{} exists; pass overwrite to replace it", csv.display());
    }
    if let Some(parent) = csv.parent()
        && !parent.as_os_str().is_empty()
        && !parent.is_dir()
    {
        bail!("directory {} does not exist", parent.display());
    }
    let extracted = pdf_text::extract_lines(pdf)?;
    if extracted.unmapped_glyphs > 0 {
        tracing::warn!(
            count = extracted.unmapped_glyphs,
            "PDF fonts had glyphs with no text mapping; some characters are missing"
        );
    }
    let parsed = statement::parse(&extracted.lines, opts.year);
    if parsed.sections.is_empty() {
        bail!(
            "no transaction rows found ({} dated lines could not be parsed; {} pages). \
             If dates have no year on the statement and no period line was found, pass year.",
            parsed.unparsed_dated_lines,
            extracted.pages
        );
    }
    let infos: Vec<SectionInfo> = parsed
        .sections
        .iter()
        .map(|s| SectionInfo {
            name: s.name.clone(),
            last4: s.last4.clone(),
            rows: s.rows.len(),
        })
        .collect();
    let chosen = match opts.account {
        Some(want) => {
            let want_l = want.to_ascii_lowercase();
            let matches: Vec<usize> = parsed
                .sections
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    s.name.to_ascii_lowercase().contains(&want_l)
                        || s.last4.as_deref() == Some(want)
                })
                .map(|(i, _)| i)
                .collect();
            match matches.as_slice() {
                [one] => *one,
                [] => bail!(
                    "no statement section matches {want:?}; sections: {}",
                    describe(&infos)
                ),
                _ => bail!(
                    "{want:?} matches more than one section; sections: {}",
                    describe(&infos)
                ),
            }
        }
        None if parsed.sections.len() == 1 => 0,
        None => bail!(
            "statement has {} account sections; pass account (name or last 4): {}",
            parsed.sections.len(),
            describe(&infos)
        ),
    };
    let section = &parsed.sections[chosen];
    let rows: Vec<Vec<String>> = section
        .rows
        .iter()
        .map(|r| {
            vec![
                r.date.to_string(),
                r.description.clone(),
                to_decimal(r.amount),
            ]
        })
        .collect();
    std::fs::write(
        csv,
        csvio::write_rows(&["Date", "Description", "Amount"], &rows),
    )
    .with_context(|| format!("write {}", csv.display()))?;
    let total: i64 = section.rows.iter().map(|r| r.amount).sum();
    Ok(Summary {
        csv_path: csv.display().to_string(),
        section: SectionInfo {
            name: section.name.clone(),
            last4: section.last4.clone(),
            rows: section.rows.len(),
        },
        other_sections: infos
            .into_iter()
            .enumerate()
            .filter(|(i, _)| *i != chosen)
            .map(|(_, s)| s)
            .collect(),
        period_start: parsed.period_start.map(|d| d.to_string()),
        period_end: parsed.period_end.map(|d| d.to_string()),
        rows: section.rows.len(),
        first_date: section
            .rows
            .iter()
            .map(|r| r.date)
            .min()
            .map(|d| d.to_string()),
        last_date: section
            .rows
            .iter()
            .map(|r| r.date)
            .max()
            .map(|d| d.to_string()),
        total_amount: to_decimal(total),
        scrubbed_digit_runs: parsed.scrubbed_digit_runs,
        unparsed_dated_lines: parsed.unparsed_dated_lines,
        unmapped_glyphs: extracted.unmapped_glyphs,
        pages: extracted.pages,
        sample: section
            .rows
            .iter()
            .take(3)
            .map(|r| SampleRow {
                date: r.date.to_string(),
                description: r.description.clone(),
                amount: to_decimal(r.amount),
            })
            .collect(),
    })
}

fn describe(infos: &[SectionInfo]) -> String {
    infos
        .iter()
        .map(|s| match &s.last4 {
            Some(l) => format!("{} (…{l}, {} rows)", s.name, s.rows),
            None => format!("{} ({} rows)", s.name, s.rows),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

pub fn resolve(path: &str) -> Result<PathBuf> {
    crate::paths::expand_tilde(path)
}
