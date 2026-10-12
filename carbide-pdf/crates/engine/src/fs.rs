//! Replacing a file on disk without ever leaving it half written.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Writes `bytes` to `path` through a temporary file in the same directory that is then renamed
/// over the target, so the file is either the old one or the complete new one, never a torn write.
///
/// A symbolic link is followed: the file it points to is replaced and the link itself stays. The
/// new file gets the permission bits of the one it replaces. A target that cannot be written (a
/// read-only file, or one owned by someone else) is refused instead of being replaced, as writing
/// to it directly would be. The temporary file is removed again on any error.
pub fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let target = resolve_links(path)?;
    let existing = match fs::metadata(&target) {
        Ok(metadata) => Some(metadata),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if let Some(metadata) = &existing {
        if !metadata.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a file", target.display())));
        }
        // Renaming over the file would succeed even where writing to it is not allowed.
        if let Err(e) = OpenOptions::new().write(true).open(&target) {
            return Err(io::Error::new(e.kind(), format!("{} is read-only or cannot be changed ({e})", target.display())));
        }
    }

    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "document".into());
    // A new file gets the usual default permissions; a replacement stays private until it has
    // the permissions of the file it replaces.
    let (temp, mut file) = create_temp(&dir, &name, if existing.is_some() { 0o600 } else { 0o666 })?;
    let written = (|| {
        file.write_all(bytes)?;
        if let Some(metadata) = &existing {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &target)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    // Makes the rename itself durable. Not every platform can sync a directory, so this is best effort.
    if let Ok(handle) = File::open(&dir) {
        let _ = handle.sync_all();
    }
    Ok(())
}

/// The file `path` finally refers to after following symbolic links, even when that file does not
/// exist yet.
fn resolve_links(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..40 {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let link = fs::read_link(&current)?;
                current = match current.parent() {
                    Some(dir) => dir.join(link),
                    None => link,
                };
            }
            Ok(_) => return Ok(current),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(current),
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other(format!("too many symbolic links at {}", path.display())))
}

/// Creates a new, uniquely named file in `dir`. `mode` is the Unix permission mode it is created
/// with (before the umask).
fn create_temp(dir: &Path, name: &str, mode: u32) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut attempts = 0;
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!(".{name}.{}-{n}.carbide-tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
        #[cfg(not(unix))]
        let _ = mode;
        match options.open(&temp) {
            Ok(file) => return Ok((temp, file)),
            // Left over from another process, or a name someone else picked: try the next one.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempts < 100 => attempts += 1,
            Err(e) => return Err(e),
        }
    }
}
