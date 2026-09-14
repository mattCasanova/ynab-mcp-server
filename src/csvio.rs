//! Minimal RFC 4180 CSV reading and writing so bank files and exports need no extra crate.
//! Handles quoted fields, doubled quotes, embedded newlines, CRLF, and a UTF-8 BOM.

use anyhow::{Result, bail};

/// Parse CSV text into rows of fields. The first row is returned like any other.
pub fn parse(text: &str) -> Result<Vec<Vec<String>>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    let mut line = 1usize;
    while let Some(c) = chars.next() {
        match (in_quotes, c) {
            (true, '"') => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    in_quotes = false;
                }
            }
            (true, '\n') => {
                line += 1;
                field.push(c);
            }
            (true, _) => field.push(c),
            (false, '"') => {
                if !field.is_empty() {
                    bail!("line {line}: quote inside an unquoted field");
                }
                in_quotes = true;
            }
            (false, ',') => row.push(std::mem::take(&mut field)),
            (false, '\r') => {
                if chars.peek() == Some(&'\n') {
                    continue;
                }
                field.push(c);
            }
            (false, '\n') => {
                line += 1;
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            (false, _) => field.push(c),
        }
    }
    if in_quotes {
        bail!("unterminated quoted field at end of input");
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    // Drop fully blank trailing rows (a final newline, or blank lines at the end).
    while rows
        .last()
        .is_some_and(|r| r.iter().all(|f| f.trim().is_empty()))
    {
        rows.pop();
    }
    Ok(rows)
}

pub fn escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

pub fn write_rows(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str(
        &header
            .iter()
            .map(|h| escape(h))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push('\n');
    for row in rows {
        out.push_str(&row.iter().map(|f| escape(f)).collect::<Vec<_>>().join(","));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_quotes_commas_newlines_and_bom() {
        let text = "\u{feff}Date,Description,Amount\r\n09/01/2026,\"COFFEE, \"\"THE\"\" SHOP\",-4.50\r\n09/02/2026,\"multi\nline\",10\r\n";
        let rows = parse(text).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], vec!["Date", "Description", "Amount"]);
        assert_eq!(rows[1][1], "COFFEE, \"THE\" SHOP");
        assert_eq!(rows[2][1], "multi\nline");
        assert_eq!(rows[2][2], "10");
    }

    #[test]
    fn rejects_unterminated_quote() {
        assert!(parse("a,\"b\n").is_err());
    }

    #[test]
    fn round_trips_through_write() {
        let rows = vec![vec![
            "a,b".to_string(),
            "say \"hi\"".to_string(),
            "plain".to_string(),
        ]];
        let text = write_rows(&["x", "y", "z"], &rows);
        let back = parse(&text).unwrap();
        assert_eq!(back[1], rows[0]);
    }
}
