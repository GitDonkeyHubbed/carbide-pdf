//! Writing files on disk without ever leaving one half written, and without replacing a file
//! unless that was asked for.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// What writing a new file does when a file is already at its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    /// Writes nothing and fails with `AlreadyExists`.
    Refuse,
    /// Replaces it in one step, as [`write_replacing`] does.
    Replace,
    /// Keeps it, and writes the new file under the first free name: `name (2).png`, `name (3).png`...
    KeepBoth,
}

/// Writes `bytes` to `path` the way `existing` says, and returns the path written, which differs
/// from `path` only with [`Existing::KeepBoth`].
pub fn write_file(path: &Path, bytes: &[u8], existing: Existing) -> io::Result<PathBuf> {
    match existing {
        Existing::Replace => write_replacing(path, bytes).map(|()| path.to_path_buf()),
        Existing::Refuse => write_first_free(path, bytes, 1),
        Existing::KeepBoth => write_first_free(path, bytes, MAX_NUMBER),
    }
}

/// The highest number [`Existing::KeepBoth`] tries before giving up.
const MAX_NUMBER: usize = 9_999;

/// `path` itself for `n` 1, otherwise `path` with ` (n)` added to its name before the extension.
fn numbered(path: &Path, n: usize) -> PathBuf {
    if n <= 1 {
        return path.to_path_buf();
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let name = match path.extension() {
        Some(extension) => format!("{stem} ({n}).{}", extension.to_string_lossy()),
        None => format!("{stem} ({n})"),
    };
    path.with_file_name(name)
}

/// Writes `bytes` to a new file at `path`, never touching a file that is already there: the new
/// file appears whole or not at all. Fails with `AlreadyExists` when the path is taken.
pub fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_first_free(path, bytes, 1).map(|_| ())
}

/// Writes `bytes` once, to a temporary file next to `path`, and gives it the first of the names
/// `path` numbered 1 to `last` (see [`numbered`]) that nothing has yet. Names already taken are
/// passed over without writing anything again. Returns the name it got.
fn write_first_free(path: &Path, bytes: &[u8], last: usize) -> io::Result<PathBuf> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let (temp, mut file) = create_temp(&dir, 0o666)?;
    let named = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        for n in 1..=last {
            let candidate = numbered(path, n);
            if n < last && fs::symlink_metadata(&candidate).is_ok() {
                continue;
            }
            match link_new(&temp, &candidate) {
                Ok(()) => return Ok(candidate),
                // Taken since it was looked at: on to the next number.
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < last => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("no free name next to {}", path.display())))
    })();
    let _ = fs::remove_file(&temp);
    let named = named?;
    if let Ok(handle) = File::open(&dir) {
        let _ = handle.sync_all();
    }
    Ok(named)
}

/// Gives the finished file `temp` the name `path`, unless something already has that name.
fn link_new(temp: &Path, path: &Path) -> io::Result<()> {
    let taken = || io::Error::new(io::ErrorKind::AlreadyExists, format!("{} already exists", path.display()));
    match fs::hard_link(temp, path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(taken()),
        // A file system without hard links: look first, then rename, which leaves a short window.
        Err(_) => {
            if fs::symlink_metadata(path).is_ok() {
                return Err(taken());
            }
            fs::rename(temp, path)
        }
    }
}

/// Folders [`private_temp_file`] makes are named this, followed by [`PRIVATE_DIGITS`] random
/// hexadecimal digits.
const PRIVATE_PREFIX: &str = "carbide-copy-";
const PRIVATE_DIGITS: usize = 16;

/// Writes `bytes` to a new file called `name` inside a new folder of its own in the system's
/// temporary folder, and returns the file's path. Only the user can read the folder and the file,
/// and neither existed before, so nothing else can have put a file or a link there.
pub fn private_temp_file(name: &str, bytes: &[u8]) -> io::Result<PathBuf> {
    if Path::new(name).file_name().is_none_or(|n| n != name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{name:?} is not a file name")));
    }
    let base = std::env::temp_dir();
    for _ in 0..100 {
        let mut random = [0u8; PRIVATE_DIGITS / 2];
        getrandom::fill(&mut random).map_err(|e| io::Error::other(format!("no randomness available: {e}")))?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let dir = base.join(format!("{PRIVATE_PREFIX}{suffix}"));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        match builder.create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
        let path = dir.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let written = options.open(&path).and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        });
        if let Err(e) = written {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_dir(&dir);
            return Err(e);
        }
        return Ok(path);
    }
    Err(io::Error::other("could not create a private temporary folder"))
}

/// True for a file [`private_temp_file`] made, also when `path` reaches the temporary folder
/// another way (through a link, such as `/private/var` for `/var` on macOS).
pub fn is_private_temp_file(path: &Path) -> bool {
    let Some(dir) = path.parent() else { return false };
    let named_so = dir.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix(PRIVATE_PREFIX)).is_some_and(|digits| {
        digits.len() == PRIVATE_DIGITS && digits.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let temp = std::env::temp_dir();
    named_so && dir.parent().is_some_and(|parent| parent == temp || canonical(parent) == canonical(&temp))
}

/// Removes a file [`private_temp_file`] made, and its folder. Any other path is refused.
pub fn remove_private_temp_file(path: &Path) -> io::Result<()> {
    if !is_private_temp_file(path) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a private temporary file", path.display())));
    }
    fs::remove_file(path)?;
    match path.parent() {
        Some(dir) => fs::remove_dir(dir),
        None => Ok(()),
    }
}

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
    fn new_files_never_replace_one_that_is_there() {
        let dir = scratch("new");
        let path = dir.join("page-001.png");
        write_new(&path, b"first").unwrap();
        let refused = write_new(&path, b"second").unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(write_file(&path, b"second", Existing::Refuse).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"first");

        assert_eq!(write_file(&path, b"second", Existing::KeepBoth).unwrap(), dir.join("page-001 (2).png"));
        assert_eq!(write_file(&path, b"third", Existing::KeepBoth).unwrap(), dir.join("page-001 (3).png"));
        assert_eq!(fs::read(&path).unwrap(), b"first");
        assert_eq!(write_file(&path, b"replaced", Existing::Replace).unwrap(), path);
        assert_eq!(fs::read(&path).unwrap(), b"replaced");
        assert_eq!(names(&dir), ["page-001 (2).png", "page-001 (3).png", "page-001.png"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn keep_both_writes_once_however_many_names_are_taken() {
        let dir = scratch("keep-both");
        let path = dir.join("page.png");
        for n in 1..=50 {
            fs::write(numbered(&path, n), b"taken").unwrap();
        }
        // A link that leads nowhere takes its name too.
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("nowhere"), numbered(&path, 51)).unwrap();
        #[cfg(not(unix))]
        fs::write(numbered(&path, 51), b"taken").unwrap();
        assert_eq!(write_file(&path, b"new", Existing::KeepBoth).unwrap(), dir.join("page (52).png"));
        assert_eq!(fs::read(dir.join("page (52).png")).unwrap(), b"new");
        assert!(!dir.join("nowhere").exists());
        assert!(!names(&dir).iter().any(|n| n.ends_with(".tmp")));
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn private_temporary_files_are_new_and_only_for_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let path = private_temp_file("revision - v1.pdf", b"copy").unwrap();
        let dir = path.parent().unwrap().to_path_buf();
        assert!(is_private_temp_file(&path));
        assert_eq!(fs::read(&path).unwrap(), b"copy");
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // Two copies never share a folder.
        let other = private_temp_file("revision - v1.pdf", b"copy").unwrap();
        assert_ne!(other.parent(), path.parent());

        // However the temporary folder is reached.
        assert!(is_private_temp_file(&path.canonicalize().unwrap()));

        assert!(private_temp_file("../escape.pdf", b"x").is_err());
        assert!(remove_private_temp_file(&std::env::temp_dir().join("not-ours.pdf")).is_err());
        assert!(!is_private_temp_file(&std::env::temp_dir().join("carbide-copy-notes").join("a.pdf")));
        remove_private_temp_file(&path).unwrap();
        remove_private_temp_file(&other).unwrap();
        assert!(!dir.exists());
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
