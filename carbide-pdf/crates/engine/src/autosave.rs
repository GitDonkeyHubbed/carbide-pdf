//! Auto Save / Auto Recovery.
//!
//! Behaviour: a global enable flag and interval (1 to 120 minutes,
//! default on / 10), one repeating timer per modified document, a full recovery copy written to an
//! application folder, an XML index that survives restarts, and a recovery list shown at launch.
//! Passwords are never written to disk, and neither is a password-protected document: its recovery
//! copy would hold its content unencrypted. The folder and everything in it are private to the user.
//!
//! The module is deliberately UI-free: the application owns the timers and calls into
//! [`AutoSaveStore`] and the policy functions below.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::pdf::Pdf;

pub const MIN_INTERVAL_MINUTES: u32 = 1;
pub const MAX_INTERVAL_MINUTES: u32 = 120;
pub const DEFAULT_INTERVAL_MINUTES: u32 = 10;
pub const INDEX_FILE_NAME: &str = "AutoRecovery.xml";

/// User-facing messages.
pub mod messages {
    pub const PREFERENCE_TITLE: &str = "Auto Save";
    pub const PREFERENCE_CHECKBOX: &str = "Keep a recovery copy of open documents, updated every";
    pub const PREFERENCE_UNIT: &str = "minutes";
    pub const INTERVAL_RANGE: &str = "Enter a whole number from 1 to 120.";
    pub const PANEL_TITLE: &str = "Auto Recovery";
    pub const MENU_LABEL: &str = "Auto Recovery";
    pub const MENU_HELP: &str = "Show the documents recovered automatically.";
    pub const CLEAR_ALL: &str = "Clear all";
    pub const DELETE_ALL: &str = "Delete every recovered document?";
    pub const TOOLTIP_ORIGINAL: &str = "The file as you last saved it.";
    pub const TOOLTIP_RECOVERED: &str = "A copy kept automatically by Auto Recovery.";
    pub const LAST_SAVED_BY_USER: &str = "Saved by you";
    pub const LAST_SAVED_AUTO: &str = "Saved automatically";
    pub const APP_CLOSE: &str = "Some recovered documents have not been opened yet. Choose Yes to keep them for later, \
or No to discard them if your work is already saved.";

    pub fn cannot_open_recovered(reason: &str) -> String {
        format!("This recovered document could not be opened ({reason}).")
    }
    pub fn delete_recovered(name: &str) -> String {
        format!("Delete the recovered copy of {name}?")
    }
    pub fn document_close(name: &str) -> String {
        format!(
            "Auto Recovery kept a copy of '{name}' that may hold unsaved changes. \
Save the recovered copy? Your original file is not affected."
        )
    }
}

// ---- settings and timer policy -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoSaveSettings {
    pub enabled: bool,
    pub interval_minutes: u32,
}

impl Default for AutoSaveSettings {
    fn default() -> Self {
        Self { enabled: true, interval_minutes: DEFAULT_INTERVAL_MINUTES }
    }
}

impl AutoSaveSettings {
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_minutes as u64 * 60)
    }

    /// Validates text typed into the interval field: digits only, 1 to 120.
    pub fn parse_interval(text: &str) -> std::result::Result<u32, &'static str> {
        let trimmed = text.trim();
        if trimmed.is_empty() || !trimmed.chars().all(|c| c.is_ascii_digit()) {
            return Err(messages::INTERVAL_RANGE);
        }
        match trimmed.parse::<u32>() {
            Ok(v) if (MIN_INTERVAL_MINUTES..=MAX_INTERVAL_MINUTES).contains(&v) => Ok(v),
            _ => Err(messages::INTERVAL_RANGE),
        }
    }

    /// Applies the preference page. With the checkbox off the interval is not validated and the
    /// stored value is left unchanged; with it on, the typed value must be valid.
    pub fn apply_page(&mut self, enabled: bool, interval_text: &str) -> std::result::Result<(), &'static str> {
        if enabled {
            self.interval_minutes = Self::parse_interval(interval_text)?;
        }
        self.enabled = enabled;
        Ok(())
    }
}

/// A document may be auto-saved when it is modified and the host is not busy.
pub fn can_do_autosave(is_modified: bool, host_busy: bool) -> bool {
    is_modified && !host_busy
}

/// Refusal for a password-protected document, whose recovery copy would be unencrypted.
pub const PROTECTED: &str = "password-protected documents are not kept for recovery";

/// The per-document timer starts only if every condition holds.
pub fn should_start_timer(
    settings: &AutoSaveSettings,
    timer_running: bool,
    can_autosave: bool,
    record_needs_save: bool,
) -> bool {
    settings.enabled && !timer_running && can_autosave && !record_needs_save
}

// ---- records and the store -----------------------------------------------------------------

pub type RecordId = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginalSlot {
    pub path: PathBuf,
    pub file_name: String,
    /// Seconds since the Unix epoch.
    pub save_time: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveredSlot {
    pub path: PathBuf,
    pub save_time: i64,
    pub flags: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub id: RecordId,
    pub original: Option<OriginalSlot>,
    pub recovered: Option<RecoveredSlot>,
    /// The document was never saved to disk.
    pub is_new_doc: bool,
    /// Read from the index at launch (as opposed to created during this session).
    pub loaded: bool,
}

impl Record {
    /// A loaded record that still holds a recovery copy.
    pub fn needs_save(&self) -> bool {
        self.loaded && self.recovered.is_some()
    }

    fn display_name(&self) -> String {
        self.original
            .as_ref()
            .map(|o| o.file_name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| "Untitled".to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowKind {
    Original,
    Recovered,
}

/// One row of the Auto Recovery panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelRow {
    pub record: RecordId,
    pub kind: RowKind,
    /// `<name> (saved)` or `<name> (recovered)`.
    pub name: String,
    pub tooltip: &'static str,
    pub last_saved: &'static str,
    pub save_time: i64,
}

pub struct AutoSaveStore {
    dir: PathBuf,
    records: Vec<Record>,
    next_id: RecordId,
}

impl AutoSaveStore {
    /// Opens (creating if needed) the data directory and loads the index. A missing index, or one
    /// whose root element is not `AutoSave`, yields an empty list.
    pub fn open(dir: impl Into<PathBuf>) -> Result<AutoSaveStore> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        private(&dir, 0o700);
        let mut store = AutoSaveStore { dir, records: Vec::new(), next_id: 1 };
        store.load_index();
        Ok(store)
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }

    pub fn records(&self) -> &[Record] {
        &self.records
    }

    pub fn record(&self, id: RecordId) -> Option<&Record> {
        self.records.iter().find(|r| r.id == id)
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE_NAME)
    }

    /// True if any loaded record still holds a recovery copy (quit and launch prompts).
    pub fn any_needs_save(&self) -> bool {
        self.records.iter().any(Record::needs_save)
    }

    pub fn panel_rows(&self) -> Vec<PanelRow> {
        let mut rows = Vec::new();
        for record in self.records.iter().filter(|r| r.loaded) {
            let name = record.display_name();
            if let Some(original) = &record.original {
                rows.push(PanelRow {
                    record: record.id,
                    kind: RowKind::Original,
                    name: format!("{name} (saved)"),
                    tooltip: messages::TOOLTIP_ORIGINAL,
                    last_saved: messages::LAST_SAVED_BY_USER,
                    save_time: original.save_time,
                });
            }
            if let Some(recovered) = &record.recovered {
                rows.push(PanelRow {
                    record: record.id,
                    kind: RowKind::Recovered,
                    name: format!("{name} (recovered)"),
                    tooltip: messages::TOOLTIP_RECOVERED,
                    last_saved: messages::LAST_SAVED_AUTO,
                    save_time: recovered.save_time,
                });
            }
        }
        rows
    }

    // ---- session API -----------------------------------------------------------------------

    /// Registers an open document. `original` is its path on disk, if it has one.
    pub fn begin_document(&mut self, original: Option<&Path>, now: i64) -> RecordId {
        let id = self.next_id;
        self.next_id += 1;
        let original_slot = original.map(|p| OriginalSlot {
            path: p.to_path_buf(),
            file_name: p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            save_time: now,
        });
        self.records.push(Record {
            id,
            original: original_slot,
            recovered: None,
            is_new_doc: original.is_none(),
            loaded: false,
        });
        id
    }

    /// Writes a full recovery copy of `pdf` and updates the index. A document that was opened
    /// from an encrypted file is refused: the caller has to skip protected documents.
    pub fn autosave(&mut self, id: RecordId, pdf: &mut Pdf, now: i64) -> Result<()> {
        if pdf.doc.was_encrypted() || pdf.doc.is_encrypted() {
            return Err(Error::Other(PROTECTED.into()));
        }
        let bytes = pdf.to_bytes()?;
        let existing = self.record(id).ok_or_else(|| Error::Other("unknown auto-save record".into()))?;
        let path = match &existing.recovered {
            Some(slot) => slot.path.clone(),
            None => self.reserve_file()?,
        };
        let temp = path.with_extension("asd.tmp");
        write_private(&temp, &bytes)?;
        fs::rename(&temp, &path)?;

        let record = self.records.iter_mut().find(|r| r.id == id).expect("record exists");
        match &mut record.recovered {
            Some(slot) => slot.save_time = now,
            None => record.recovered = Some(RecoveredSlot { path, save_time: now, flags: 0 }),
        }
        self.write_index()
    }

    /// The user saved the document normally.
    pub fn document_saved(&mut self, id: RecordId) -> Result<()> {
        let Some(position) = self.records.iter().position(|r| r.id == id) else { return Ok(()) };
        if self.records[position].loaded {
            // A recovered document was saved: drop the recovery copy, keep tracking the original.
            let slot = self.records[position].recovered.take();
            if let Some(slot) = slot {
                self.delete_file(&slot.path);
            }
            self.records[position].loaded = false;
            if self.records[position].original.is_none() {
                self.records.remove(position);
            }
        } else {
            self.remove_record_files(position);
            self.records.remove(position);
        }
        self.write_index()
    }

    /// The document was closed. Session records lose their recovery copy; recovered ones stay in
    /// the list so the user can decide later.
    pub fn document_closed(&mut self, id: RecordId) -> Result<()> {
        let Some(position) = self.records.iter().position(|r| r.id == id) else { return Ok(()) };
        if !self.records[position].loaded {
            self.remove_record_files(position);
            self.records.remove(position);
            self.write_index()?;
        }
        Ok(())
    }

    // ---- panel actions ---------------------------------------------------------------------

    /// Removes the original row. If a recovered copy exists only the original slot is cleared.
    pub fn remove_original(&mut self, id: RecordId) -> Result<()> {
        let Some(position) = self.records.iter().position(|r| r.id == id) else { return Ok(()) };
        if self.records[position].recovered.is_some() {
            self.records[position].original = None;
        } else {
            self.remove_record_files(position);
            self.records.remove(position);
        }
        self.write_index()
    }

    /// Removes the recovered row. If an original exists only the recovered files are deleted.
    pub fn remove_recovered(&mut self, id: RecordId) -> Result<()> {
        let Some(position) = self.records.iter().position(|r| r.id == id) else { return Ok(()) };
        if self.records[position].original.is_some() {
            if let Some(slot) = self.records[position].recovered.take() {
                self.delete_file(&slot.path);
            }
        } else {
            self.remove_record_files(position);
            self.records.remove(position);
        }
        self.write_index()
    }

    pub fn remove_record(&mut self, id: RecordId) -> Result<()> {
        if let Some(position) = self.records.iter().position(|r| r.id == id) {
            self.remove_record_files(position);
            self.records.remove(position);
        }
        self.write_index()
    }

    /// "Clear all": removes every loaded record. Returns how many were removed.
    pub fn remove_all_loaded(&mut self) -> Result<usize> {
        let before = self.records.len();
        let mut index = 0;
        while index < self.records.len() {
            if self.records[index].loaded {
                self.remove_record_files(index);
                self.records.remove(index);
            } else {
                index += 1;
            }
        }
        self.write_index()?;
        Ok(before - self.records.len())
    }

    // ---- opening ---------------------------------------------------------------------------

    /// Loads the recovered copy. The title shown for it is `<name> (recovered)`.
    pub fn open_recovered(&self, id: RecordId, password: Option<&str>) -> Result<(Pdf, String)> {
        let record = self.record(id).ok_or_else(|| Error::Other("unknown auto-save record".into()))?;
        let slot = record
            .recovered
            .as_ref()
            .filter(|s| s.path.exists())
            .ok_or_else(|| Error::Other(messages::cannot_open_recovered("the recovery file is missing")))?;
        let bytes = fs::read(&slot.path)?;
        let pdf = match password {
            Some(p) => Pdf::from_bytes_with_password(&bytes, p),
            None => Pdf::from_bytes(&bytes),
        }
        .map_err(|e| Error::Other(messages::cannot_open_recovered(&e.to_string())))?;
        Ok((pdf, format!("{} (recovered)", record.display_name())))
    }

    // ---- files and index -------------------------------------------------------------------

    /// Creates a unique `ZASxxxxxx.asd` placeholder (mkstemp style) and returns its path.
    fn reserve_file(&self) -> Result<PathBuf> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
        for _ in 0..32 {
            let mut random = [0u8; 6];
            getrandom::fill(&mut random).map_err(|e| Error::Other(format!("no randomness available: {e}")))?;
            let suffix: String = random.iter().map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char).collect();
            let path = self.dir.join(format!("ZAS{suffix}.asd"));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            if options.open(&path).is_ok() {
                return Ok(path);
            }
        }
        Err(Error::Other("could not create an auto-save file".into()))
    }

    /// Only files inside the data directory are ever deleted: never the user's originals.
    fn delete_file(&self, path: &Path) {
        if path.parent() == Some(self.dir.as_path()) {
            let _ = fs::remove_file(path);
        }
    }

    fn remove_record_files(&self, position: usize) {
        if let Some(slot) = &self.records[position].recovered {
            self.delete_file(&slot.path);
        }
    }

    fn write_index(&self) -> Result<()> {
        let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<AutoSave version=\"1.0\">\n");
        for record in &self.records {
            xml.push_str("  <AutoSaveItem");
            if let Some(o) = &record.original {
                attribute(&mut xml, "OriginalFile", &o.path.to_string_lossy());
                attribute(&mut xml, "OriginalSaveTime", &o.save_time.to_string());
                attribute(&mut xml, "OriginalFileName", &o.file_name);
            }
            if let Some(r) = &record.recovered {
                attribute(&mut xml, "RecoveredFile", &r.path.to_string_lossy());
                attribute(&mut xml, "RecoveredSaveTime", &r.save_time.to_string());
                attribute(&mut xml, "RecoveredFlags", &r.flags.to_string());
            }
            attribute(&mut xml, "IsNewDoc", if record.is_new_doc { "1" } else { "0" });
            xml.push_str("/>\n");
        }
        xml.push_str("</AutoSave>\n");

        let temp = self.index_path().with_extension("xml.tmp");
        write_private(&temp, xml.as_bytes())?;
        fs::rename(&temp, self.index_path())?;
        Ok(())
    }

    fn load_index(&mut self) {
        let Ok(text) = fs::read_to_string(self.index_path()) else { return };
        let Ok(document) = roxmltree::Document::parse(&text) else { return };
        let root = document.root_element();
        if root.tag_name().name() != "AutoSave" {
            return;
        }
        for item in root.children().filter(|n| n.has_tag_name("AutoSaveItem")) {
            let attr = |name: &str| item.attribute(name).filter(|v| !v.is_empty());
            let original = attr("OriginalFile").map(|path| OriginalSlot {
                path: PathBuf::from(path),
                file_name: attr("OriginalFileName").unwrap_or_default().to_string(),
                save_time: attr("OriginalSaveTime").and_then(|v| v.parse().ok()).unwrap_or(0),
            });
            // A recovery file that no longer exists cannot be opened, so the slot is dropped.
            let recovered = attr("RecoveredFile")
                .map(PathBuf::from)
                .filter(|p| p.exists())
                .map(|path| RecoveredSlot {
                    path,
                    save_time: attr("RecoveredSaveTime").and_then(|v| v.parse().ok()).unwrap_or(0),
                    flags: attr("RecoveredFlags").and_then(|v| v.parse().ok()).unwrap_or(0),
                });
            if original.is_none() && recovered.is_none() {
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            self.records.push(Record {
                id,
                original,
                recovered,
                is_new_doc: attr("IsNewDoc") == Some("1"),
                loaded: true,
            });
        }
    }
}

/// Writes `bytes` to a file only the user can read, replacing one that is there.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let _ = fs::remove_file(path);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(bytes)?;
    Ok(())
}

/// Restricts `path` to the user. Best effort: a folder that cannot be changed is used as it is.
fn private(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

fn attribute(xml: &mut String, name: &str, value: &str) {
    xml.push(' ');
    xml.push_str(name);
    xml.push_str("=\"");
    for c in value.chars() {
        match c {
            '&' => xml.push_str("&amp;"),
            '<' => xml.push_str("&lt;"),
            '>' => xml.push_str("&gt;"),
            '"' => xml.push_str("&quot;"),
            '\n' => xml.push_str("&#10;"),
            other => xml.push(other),
        }
    }
    xml.push('"');
}

/// `yyyy-MM-dd HH:mm:ss` in local time, for the recovery panel's "saved" column.
pub fn format_time(unix: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(unix, 0)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

/// Seconds since the Unix epoch.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_validation_matches_the_original() {
        assert_eq!(AutoSaveSettings::parse_interval("10"), Ok(10));
        assert_eq!(AutoSaveSettings::parse_interval("1"), Ok(1));
        assert_eq!(AutoSaveSettings::parse_interval("120"), Ok(120));
        for bad in ["0", "121", "", "abc", "-5", "1.5", "10 min"] {
            assert_eq!(AutoSaveSettings::parse_interval(bad), Err(messages::INTERVAL_RANGE), "{bad:?}");
        }
    }

    #[test]
    fn preference_page_semantics() {
        let mut settings = AutoSaveSettings::default();
        assert_eq!((settings.enabled, settings.interval_minutes), (true, 10));
        // Checkbox off: the typed text is not validated and the interval stays.
        settings.apply_page(false, "garbage").unwrap();
        assert_eq!((settings.enabled, settings.interval_minutes), (false, 10));
        // On with a bad value: rejected and nothing changes.
        assert!(settings.apply_page(true, "500").is_err());
        assert!(!settings.enabled);
        settings.apply_page(true, "25").unwrap();
        assert_eq!((settings.enabled, settings.interval_minutes), (true, 25));
        assert_eq!(settings.interval(), Duration::from_secs(1500));
    }

    #[test]
    fn timer_start_conditions() {
        let on = AutoSaveSettings::default();
        let off = AutoSaveSettings { enabled: false, ..on };
        assert!(should_start_timer(&on, false, true, false));
        assert!(!should_start_timer(&off, false, true, false));
        assert!(!should_start_timer(&on, true, true, false), "already running");
        assert!(!should_start_timer(&on, false, false, false), "not dirty");
        assert!(!should_start_timer(&on, false, true, true), "recovery copy pending");
        assert!(can_do_autosave(true, false));
        assert!(!can_do_autosave(true, true));
        assert!(!can_do_autosave(false, false));
    }
}
