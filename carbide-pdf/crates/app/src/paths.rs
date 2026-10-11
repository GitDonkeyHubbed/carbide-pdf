//! Per-user data locations and the small settings file.

use std::path::PathBuf;

use carbide_engine::autosave::{AutoSaveSettings, DEFAULT_INTERVAL_MINUTES};

/// `~/Library/Application Support/Carbide PDF` (macOS), `%APPDATA%\Carbide PDF` (Windows) or
/// `~/.local/share/carbide-pdf` (elsewhere).
pub fn data_dir() -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    if cfg!(target_os = "macos") {
        home().join("Library/Application Support/Carbide PDF")
    } else if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(home).join("Carbide PDF")
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home().join(".local/share"))
            .join("carbide-pdf")
    }
}

pub fn autosave_dir() -> PathBuf {
    data_dir().join("AutoSave")
}

fn settings_file() -> PathBuf {
    data_dir().join("settings.ini")
}

/// Domain "ZAutoSave", keys `EnableAutoSave` and `AutoSaveInterval` as in the original.
pub fn load_autosave_settings() -> AutoSaveSettings {
    let mut settings = AutoSaveSettings::default();
    let Ok(text) = std::fs::read_to_string(settings_file()) else { return settings };
    for line in text.lines() {
        match line.split_once('=') {
            Some(("EnableAutoSave", v)) => settings.enabled = v.trim() != "0",
            Some(("AutoSaveInterval", v)) => {
                if let Ok(Ok(minutes)) = v.trim().parse::<u32>().map(|m| AutoSaveSettings::parse_interval(&m.to_string())) {
                    settings.interval_minutes = minutes;
                }
            }
            _ => {}
        }
    }
    if settings.interval_minutes == 0 {
        settings.interval_minutes = DEFAULT_INTERVAL_MINUTES;
    }
    settings
}

pub fn save_autosave_settings(settings: &AutoSaveSettings) {
    let _ = std::fs::create_dir_all(data_dir());
    let text = format!(
        "[ZAutoSave]\nEnableAutoSave={}\nAutoSaveInterval={}\n",
        if settings.enabled { 1 } else { 0 },
        settings.interval_minutes
    );
    let _ = std::fs::write(settings_file(), text);
}

/// Certificates the user has chosen to trust (DER files).
pub fn trusted_dir() -> PathBuf {
    data_dir().join("trusted")
}

pub fn load_trust_store() -> carbide_engine::signature::TrustStore {
    let mut store = carbide_engine::signature::TrustStore::default();
    if let Ok(entries) = std::fs::read_dir(trusted_dir()) {
        for entry in entries.flatten() {
            if let Ok(bytes) = std::fs::read(entry.path()) {
                store.certificates.push(bytes);
            }
        }
    }
    store
}

/// Adds a PEM or DER certificate file to the trusted list. Returns how many were added.
pub fn trust_certificate_file(path: &std::path::Path) -> std::io::Result<usize> {
    use base64::Engine;
    let bytes = std::fs::read(path)?;
    let mut ders: Vec<Vec<u8>> = Vec::new();
    if let Ok(text) = std::str::from_utf8(&bytes) {
        for block in text.split("-----BEGIN CERTIFICATE-----").skip(1) {
            if let Some(body) = block.split("-----END CERTIFICATE-----").next() {
                let cleaned: String = body.chars().filter(|c| !c.is_whitespace()).collect();
                if let Ok(der) = base64::engine::general_purpose::STANDARD.decode(cleaned) {
                    ders.push(der);
                }
            }
        }
    }
    if ders.is_empty() {
        ders.push(bytes);
    }
    std::fs::create_dir_all(trusted_dir())?;
    let count = ders.len();
    for der in ders {
        let name = format!("{:016x}.der", der.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3)));
        std::fs::write(trusted_dir().join(name), der)?;
    }
    Ok(count)
}

// ---- recent files and start-screen locations ----------------------------------------------

/// How many recently opened files are remembered.
pub const MAX_RECENT: usize = 30;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentFile {
    pub path: PathBuf,
    /// Unix seconds of the last time the file was opened here.
    pub opened: i64,
}

fn recent_file() -> PathBuf {
    data_dir().join("recent.txt")
}

fn locations_file() -> PathBuf {
    data_dir().join("locations.txt")
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Most recent first. One `seconds<TAB>path` pair per line.
pub fn load_recent() -> Vec<RecentFile> {
    let Ok(text) = std::fs::read_to_string(recent_file()) else { return Vec::new() };
    text.lines()
        .filter_map(|line| {
            let (secs, path) = line.split_once('\t')?;
            Some(RecentFile { path: PathBuf::from(path), opened: secs.trim().parse().ok()? })
        })
        .take(MAX_RECENT)
        .collect()
}

pub fn save_recent(list: &[RecentFile]) {
    let _ = std::fs::create_dir_all(data_dir());
    let text: String = list.iter().map(|r| format!("{}\t{}\n", r.opened, r.path.display())).collect();
    let _ = std::fs::write(recent_file(), text);
}

/// Folders pinned in the start screen's sidebar.
pub fn load_locations() -> Vec<PathBuf> {
    std::fs::read_to_string(locations_file())
        .map(|t| t.lines().filter(|l| !l.is_empty()).map(PathBuf::from).collect())
        .unwrap_or_default()
}

pub fn save_locations(list: &[PathBuf]) {
    let _ = std::fs::create_dir_all(data_dir());
    let text: String = list.iter().map(|p| format!("{}\n", p.display())).collect();
    let _ = std::fs::write(locations_file(), text);
}

// ---- cloud-synced folders -------------------------------------------------------------------

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// "GoogleDrive-me@example.com" becomes "Google Drive (me@example.com)", and the "-folder" suffix
/// some providers add is dropped.
fn cloud_storage_label(folder_name: &str) -> String {
    let trimmed = folder_name.strip_suffix("-folder").unwrap_or(folder_name);
    let (provider, account) = match trimmed.split_once('-') {
        Some((p, a)) => (p, Some(a)),
        None => (trimmed, None),
    };
    // "ProtonDrive" -> "Proton Drive"; "OneDrive" is the product's own spelling.
    let mut spaced = String::new();
    if provider != "OneDrive" {
        for (i, c) in provider.chars().enumerate() {
            if i > 0 && c.is_uppercase() {
                spaced.push(' ');
            }
            spaced.push(c);
        }
    } else {
        spaced.push_str(provider);
    }
    match account {
        Some(account) => format!("{spaced} ({account})"),
        None => spaced,
    }
}

/// Folders that a cloud service's desktop app keeps in sync on this computer, with display names.
/// These are ordinary folders as far as Carbide PDF is concerned: it does not sign in to any service.
pub fn cloud_locations() -> Vec<(String, PathBuf)> {
    let Some(home) = home_dir() else { return Vec::new() };
    let mut found: Vec<(String, PathBuf)> = Vec::new();
    let mut add = |label: String, path: PathBuf| {
        if path.is_dir() && !found.iter().any(|(_, p)| *p == path) {
            found.push((label, path));
        }
    };
    // macOS File Provider folders: Dropbox, Google Drive, OneDrive, Box and others.
    if let Ok(entries) = std::fs::read_dir(home.join("Library/CloudStorage")) {
        let mut names: Vec<_> = entries.flatten().collect();
        names.sort_by_key(|e| e.file_name());
        for entry in names {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') {
                add(cloud_storage_label(&name), entry.path());
            }
        }
    }
    add("iCloud Drive".into(), home.join("Library/Mobile Documents/com~apple~CloudDocs"));
    for name in ["Dropbox", "OneDrive", "Google Drive", "Box", "Nextcloud"] {
        add(name.to_string(), home.join(name));
    }
    found
}

/// How a pinned location is named in the start screen's sidebar.
pub fn location_label(path: &std::path::Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string());
    if path.parent().is_some_and(|p| p.ends_with("Library/CloudStorage")) {
        cloud_storage_label(&name)
    } else if path.ends_with("Library/Mobile Documents/com~apple~CloudDocs") {
        "iCloud Drive".into()
    } else {
        name
    }
}

#[cfg(test)]
mod tests {
    use super::cloud_storage_label;

    #[test]
    fn cloud_folder_names_become_readable_labels() {
        assert_eq!(cloud_storage_label("GoogleDrive-me@example.com"), "Google Drive (me@example.com)");
        assert_eq!(cloud_storage_label("OneDrive-Personal"), "OneDrive (Personal)");
        assert_eq!(cloud_storage_label("ProtonDrive-me@proton.me-folder"), "Proton Drive (me@proton.me)");
        assert_eq!(cloud_storage_label("Dropbox"), "Dropbox");
    }
}

