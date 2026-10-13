//! Replacing a file on disk without ever leaving it half written.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Writes `bytes` to `path` through a temporary file in the same directory that is then renamed
/// over the target, so the file is either the old one or the complete new one, never a torn write.
///
/// A symbolic link is followed: the file it points to is replaced and the link itself stays. The
/// new file gets the permission bits of the one it replaces, and on macOS its extended attributes
/// (Finder tags, where it was downloaded from) and access control list too. A target that cannot
/// be written (a read-only file, or one owned by someone else) is refused instead of being
/// replaced, as writing to it directly would be. The temporary file is removed again on any error.
///
/// Other hard links to the file keep the old contents: replacing the file is what makes the write
/// all-or-nothing, and writing into it in place would give that up.
pub fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace_with(path, |file| file.write_all(bytes))
}

/// `write_replacing` with the new contents written by `write`.
fn replace_with(path: &Path, write: impl FnOnce(&mut File) -> io::Result<()>) -> io::Result<()> {
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
    // A new file gets the usual default permissions; a replacement stays private until it has
    // the permissions of the file it replaces.
    let (temp, mut file) = create_temp(&dir, if existing.is_some() { 0o600 } else { 0o666 }).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("{} does not allow new files, which saving needs in order to replace {} safely ({e})", dir.display(), target.display()),
        )
    })?;
    let written = (|| {
        write(&mut file)?;
        if let Some(metadata) = &existing {
            copy_metadata(&target, &file);
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

/// Copies the extended attributes and access control list of `from` onto `to`. Best effort: the
/// contents matter, these do not have to come along.
#[cfg(target_os = "macos")]
fn copy_metadata(from: &Path, to: &File) {
    use std::os::fd::AsRawFd;
    let Ok(source) = File::open(from) else { return };
    // SAFETY: both descriptors are open for the duration of the call, and a null state is allowed.
    unsafe {
        libc::fcopyfile(source.as_raw_fd(), to.as_raw_fd(), std::ptr::null_mut(), libc::COPYFILE_XATTR | libc::COPYFILE_ACL);
    }
}

#[cfg(not(target_os = "macos"))]
fn copy_metadata(_from: &Path, _to: &File) {}

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
/// with (before the umask). The name is short, so it fits wherever the target's name does.
fn create_temp(dir: &Path, mode: u32) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut attempts = 0;
    loop {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = dir.join(format!(".carbide-{}-{n}.tmp", std::process::id()));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("carbide-fs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    #[test]
    fn a_failed_write_leaves_the_file_and_no_temporary_file() {
        let dir = scratch("failed");
        let path = dir.join("doc.pdf");
        fs::write(&path, b"original").unwrap();
        let result = replace_with(&path, |file| {
            file.write_all(b"half")?;
            Err(io::Error::other("disk full"))
        });
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(names(&dir), ["doc.pdf"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_writers_never_share_a_temporary_file() {
        let dir = scratch("concurrent");
        let path = dir.join("doc.pdf");
        fs::write(&path, b"original").unwrap();
        let contents: Vec<Vec<u8>> = (0..8).map(|i| vec![b'a' + i as u8; 64 * 1024]).collect();
        std::thread::scope(|scope| {
            for content in &contents {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..10 {
                        write_replacing(path, content).unwrap();
                    }
                });
            }
        });
        // Whole, from one writer, never a mix.
        assert!(contents.contains(&fs::read(&path).unwrap()));
        assert_eq!(names(&dir), ["doc.pdf"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn long_file_names_can_still_be_saved() {
        let dir = scratch("long");
        let path = dir.join(format!("{}.pdf", "n".repeat(240)));
        fs::write(&path, b"old").unwrap();
        write_replacing(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_takes_no_new_files_is_named_in_the_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("locked-folder");
        let path = dir.join("doc.pdf");
        fs::write(&path, b"original").unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o555)).unwrap();
        let result = write_replacing(&path, b"new");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        // Root can write anywhere; there is nothing to check then.
        if let Err(e) = result {
            assert!(e.to_string().contains("does not allow new files"), "{e}");
            assert_eq!(fs::read(&path).unwrap(), b"original");
            assert_eq!(names(&dir), ["doc.pdf"]);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn extended_attributes_come_along() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let dir = scratch("xattr");
        let path = dir.join("tagged.pdf");
        fs::write(&path, b"old").unwrap();
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        let name = CString::new("com.example.carbide-test").unwrap();
        let value = b"kept";
        // SAFETY: valid NUL-terminated strings and a buffer of the given length.
        let set = unsafe { libc::setxattr(c_path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) };
        assert_eq!(set, 0);
        write_replacing(&path, b"new").unwrap();
        let mut buffer = [0u8; 16];
        // SAFETY: as above, with a writable buffer.
        let read = unsafe { libc::getxattr(c_path.as_ptr(), name.as_ptr(), buffer.as_mut_ptr().cast(), buffer.len(), 0, 0) };
        assert_eq!(&buffer[..read.max(0) as usize], value);
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let _ = fs::remove_dir_all(&dir);
    }
}
