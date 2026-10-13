//! Named revisions ("Save Revision" / "Open Revision").
//!
//! Behaviour follows the original feature: a revision has a label and a date, the list shows them
//! in the order they were saved, "Open" is only available once two dated revisions exist, and
//! opening one produces the file exactly as it was at that point as a separate document.
//!
//! Design choice: each revision is an incremental
//! update of the file plus a record in the catalog under `/CarbideRevisions`. The record's `Length`
//! is the file size at the end of that update, so the revision is simply a prefix of the file.

use chrono::{FixedOffset, Local, NaiveDate, TimeZone};
use lopdf::{dictionary, Object, StringFormat};

use crate::error::{Error, Result};
use crate::pdf::Pdf;

pub const CATALOG_KEY: &[u8] = b"CarbideRevisions";
/// Shown when the label is empty after trimming.
pub const EMPTY_LABEL_MESSAGE: &str = "Enter a label for the revision.";

/// Width of a revision record's Length, which is written as a string of digits.
const LENGTH_DIGITS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revision {
    pub label: String,
    /// PDF date string, e.g. `D:20261010164501-07'00'`.
    pub date: String,
    /// Size in bytes of the file at the end of this revision.
    pub file_offset: u64,
}

impl Revision {
    /// `yyyy-MM-dd HH:mm:ss` in local time; empty if the stored date is not valid.
    pub fn display_time(&self) -> String {
        pdf_date_to_local(&self.date)
    }
}

/// Only whitespace-only labels are rejected; the label is stored as typed.
pub fn validate_label(label: &str) -> std::result::Result<(), &'static str> {
    if label.trim().is_empty() {
        Err(EMPTY_LABEL_MESSAGE)
    } else {
        Ok(())
    }
}

/// The current time as a PDF date string with the local UTC offset.
pub fn pdf_date_now() -> String {
    let now = Local::now();
    let offset = now.offset().local_minus_utc();
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.abs() / 60;
    format!("D:{}{}'{:02}'", now.format("%Y%m%d%H%M%S"), format_args!("{sign}{:02}", minutes / 60), minutes % 60)
}

/// Converts `D:YYYYMMDDHHmmSS[Z|+HH'mm']` to local `yyyy-MM-dd HH:mm:ss`. Invalid input gives "".
pub fn pdf_date_to_local(text: &str) -> String {
    parse_pdf_date(text).map(|utc| utc.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default()
}

pub(crate) fn parse_pdf_date(text: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let rest = text.strip_prefix("D:")?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let tail = &rest[digits.len()..];
    let field = |from: usize, len: usize, default: u32| -> Option<u32> {
        if digits.len() >= from + len {
            digits[from..from + len].parse().ok()
        } else if digits.len() <= from {
            Some(default)
        } else {
            None
        }
    };
    let year = field(0, 4, 0)? as i32;
    let (month, day) = (field(4, 2, 1)?, field(6, 2, 1)?);
    let (hour, minute, second) = (field(8, 2, 0)?, field(10, 2, 0)?, field(12, 2, 0)?);
    if !(1971..=3000).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 24 || minute > 60 || second > 60 {
        return None;
    }

    let mut offset_seconds = 0i32;
    let mut chars = tail.chars();
    match chars.next() {
        Some(sign @ ('+' | '-')) => {
            let numbers: Vec<u32> = chars
                .as_str()
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.parse().ok())
                .collect();
            let (h, m) = (numbers.first().copied().unwrap_or(0), numbers.get(1).copied().unwrap_or(0));
            let total = (h * 3600 + m * 60) as i32;
            offset_seconds = if sign == '-' { -total } else { total };
        }
        _ => {}
    }

    let naive = NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_opt(hour.min(23), minute.min(59), second.min(59))?;
    let zone = FixedOffset::east_opt(offset_seconds)?;
    Some(zone.from_local_datetime(&naive).single()?.with_timezone(&chrono::Utc))
}

impl Pdf {
    /// Revisions with a non-empty date, in the order they were saved.
    pub fn revisions(&self) -> Vec<Revision> {
        let Ok(catalog) = self.doc.catalog() else { return Vec::new() };
        let Ok(entry) = catalog.get(CATALOG_KEY) else { return Vec::new() };
        let Object::Array(items) = self.resolve(entry) else { return Vec::new() };
        items
            .iter()
            .filter_map(|item| {
                let Object::Dictionary(record) = self.resolve(item) else { return None };
                let text = |key: &[u8]| -> String {
                    record.get(key).ok().and_then(|o| o.as_str().ok()).map(decode_text).unwrap_or_default()
                };
                let date = text(b"Date");
                if date.is_empty() {
                    return None;
                }
                Some(Revision {
                    label: text(b"Label"),
                    date,
                    file_offset: text(b"Length").trim().parse().unwrap_or(0),
                })
            })
            .collect()
    }

    /// The original enables "Open Revision" only when at least two dated revisions exist.
    pub fn can_open_revision(&self) -> bool {
        self.revisions().len() >= 2
    }

    /// Returns the new file contents: `base_bytes` plus one incremental update that contains any
    /// pending edits of this document and the new revision record.
    ///
    /// `base_bytes` is the file as last saved. Write the result over that file.
    pub fn save_revision(&self, base_bytes: &[u8], label: &str, date: &str) -> Result<Vec<u8>> {
        validate_label(label).map_err(|m| Error::Other(m.to_string()))?;
        if self.doc.is_encrypted() {
            return Err(Error::Unsupported("revisions are not supported for encrypted documents".into()));
        }
        // The record's Length is the size of the file it ends, which is only known once the update
        // is written: it starts as a fixed-width placeholder and is patched afterwards. The update is
        // written twice, with the placeholder all zeros and then all ones, so the bytes that differ
        // are exactly where the writer put the digits. Searching the update for the placeholder
        // instead could hit page text that happens to hold the same characters.
        let mut bytes = self.update_with_record(base_bytes, label, date, b'0')?;
        let other = self.update_with_record(base_bytes, label, date, b'1')?;
        let differing: Vec<usize> = (0..bytes.len().min(other.len())).filter(|&i| bytes[i] != other[i]).collect();
        let at = match differing[..] {
            [first, .., last] if other.len() == bytes.len()
                && last - first + 1 == LENGTH_DIGITS
                && differing.len() == LENGTH_DIGITS
                && first >= base_bytes.len() =>
            {
                first
            }
            _ => return Err(Error::Other("could not locate the revision length field".into())),
        };
        let digits = format!("{:0width$}", bytes.len(), width = LENGTH_DIGITS);
        if digits.len() != LENGTH_DIGITS {
            return Err(Error::Other("the file is too large for a revision record".into()));
        }
        bytes[at..at + LENGTH_DIGITS].copy_from_slice(digits.as_bytes());
        Ok(bytes)
    }

    /// `base_bytes` plus an update holding this document's pending edits and a new revision record
    /// whose Length is `digit` repeated.
    fn update_with_record(&self, base_bytes: &[u8], label: &str, date: &str, digit: u8) -> Result<Vec<u8>> {
        let mut next = self.clone();
        let mut records: Vec<Object> = match next.doc.catalog()?.get(CATALOG_KEY) {
            Ok(entry) => match next.resolve(entry) {
                Object::Array(existing) => existing,
                _ => Vec::new(),
            },
            Err(_) => Vec::new(),
        };
        records.push(Object::Dictionary(dictionary! {
            "Label" => Object::String(encode_text(label), StringFormat::Hexadecimal),
            "Date" => Object::String(date.as_bytes().to_vec(), StringFormat::Literal),
            "Length" => Object::String(vec![digit; LENGTH_DIGITS], StringFormat::Literal),
        }));
        next.doc.catalog_mut()?.set(CATALOG_KEY.to_vec(), Object::Array(records));
        next.incremental_update(base_bytes)
    }
}

/// The file as it was when `revision` was saved, ready to open as a separate document.
pub fn open_revision_bytes(file_bytes: &[u8], revision: &Revision) -> Result<Vec<u8>> {
    // An offset of zero means no position was stored.
    if revision.file_offset == 0 {
        return Err(Error::Other("this revision has no stored position".into()));
    }
    let end = revision.file_offset as usize;
    if end > file_bytes.len() {
        return Err(Error::Other("the file is shorter than the revision's recorded size".into()));
    }
    let prefix = file_bytes[..end].to_vec();
    Pdf::from_bytes(&prefix)?; // must be a complete, loadable document
    Ok(prefix)
}

/// Window title for an opened revision: source name, then the label if any, then the date.
pub fn revision_display_name(source_name: &str, revision: &Revision) -> String {
    let base = source_name.strip_suffix(".pdf").unwrap_or(source_name);
    let time = revision.display_time();
    if revision.label.is_empty() {
        format!("{base} - {time}")
    } else {
        format!("{base} - {} - {time}", revision.label)
    }
}

/// PDF text string (PDFDocEncoding subset or UTF-16BE) to `String`.
pub(crate) fn decode_text(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let units: Vec<u16> = bytes[2..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&units)
    } else if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(&bytes[3..]).into_owned()
    } else {
        bytes.iter().map(|&b| b as char).collect()
    }
}

/// `String` to a PDF text string: ASCII as is, anything else as UTF-16BE with a byte order mark.
pub(crate) fn encode_text(text: &str) -> Vec<u8> {
    if text.is_ascii() {
        return text.as_bytes().to_vec();
    }
    let mut out = vec![0xFE, 0xFF];
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, Timelike};

    #[test]
    fn labels_are_trimmed_only_for_the_emptiness_check() {
        assert_eq!(validate_label("  \t "), Err(EMPTY_LABEL_MESSAGE));
        assert!(validate_label(" draft 2 ").is_ok());
    }

    #[test]
    fn pdf_dates_convert_and_validate() {
        // UTC input converts to the machine's local zone, so compare through the same conversion.
        let utc = chrono::Utc.with_ymd_and_hms(2026, 3, 4, 5, 6, 7).unwrap();
        let expected = utc.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S").to_string();
        assert_eq!(pdf_date_to_local("D:20260304050607Z"), expected);
        assert_eq!(pdf_date_to_local("D:20260304070607+02'00'"), expected);
        assert_eq!(pdf_date_to_local("D:20260303210607-08'00'"), expected);
        for bad in ["", "20260304050607", "D:19700101000000Z", "D:20261301000000Z", "D:20260132000000Z", "D:2026030425"] {
            assert_eq!(pdf_date_to_local(bad), "", "{bad:?}");
        }
        let now = pdf_date_now();
        assert!(!pdf_date_to_local(&now).is_empty(), "{now}");
        let local = Local::now();
        assert!(pdf_date_to_local(&now).starts_with(&format!("{}-", local.year())));
        let _ = local.hour();
    }

    #[test]
    fn text_strings_round_trip() {
        for s in ["plain", "Überarbeitung 修正", ""] {
            assert_eq!(decode_text(&encode_text(s)), s);
        }
    }
}
