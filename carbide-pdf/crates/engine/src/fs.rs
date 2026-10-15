//! Writing files on disk without ever leaving one half written, and without replacing a file
//! unless that was asked for.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

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
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
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
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("no free name next to {}", path.display()),
        ))
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
    let taken = || {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} already exists", path.display()),
        )
    };
    match fs::hard_link(temp, path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(taken()),
        // A file system without hard links: rename, in a way that fails if the name is taken.
        // Looking first and renaming after would replace a file made in between.
        Err(_) => match rename_no_replace(temp, path) {
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(taken()),
            // Nor that (FAT and exFAT drives, for one): claim the name with an empty file made
            // only if nothing has it, then rename over that claim, which is ours to replace.
            Err(e) if e.kind() == io::ErrorKind::Unsupported => {
                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                {
                    Ok(claim) => drop(claim),
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(taken()),
                    Err(e) => return Err(e),
                }
                fs::rename(temp, path).inspect_err(|_| {
                    let _ = fs::remove_file(path);
                })
            }
            other => other,
        },
    }
}

/// Renames `from` to `to` in one step, and fails with `AlreadyExists` instead of replacing
/// whatever is at `to`, a link included. Where the system or the file system cannot do that, it
/// fails with `Unsupported` rather than risk replacing a file.
pub fn rename_no_replace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let c_path = |path: &Path| {
            CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path holds a NUL"))
        };
        let (from_c, to_c) = (c_path(from)?, c_path(to)?);
        #[cfg(target_os = "macos")]
        // SAFETY: both are NUL-terminated paths that live through the call.
        let result = unsafe { libc::renamex_np(from_c.as_ptr(), to_c.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        // SAFETY: both are NUL-terminated paths that live through the call. RENAME_NOREPLACE is 1.
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                from_c.as_ptr(),
                libc::AT_FDCWD,
                to_c.as_ptr(),
                1 as libc::c_uint,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        Err(match error.raw_os_error() {
            Some(code) if code == libc::ENOTSUP || code == libc::EINVAL || code == libc::ENOSYS => {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "{} cannot be given a new name without risk of replacing a file",
                        to.display()
                    ),
                )
            }
            _ => error,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (from, to);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this system cannot give a file a new name without risk of replacing another",
        ))
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
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a file name"),
        ));
    }
    let base = std::env::temp_dir();
    for _ in 0..100 {
        let mut random = [0u8; PRIVATE_DIGITS / 2];
        getrandom::fill(&mut random)
            .map_err(|e| io::Error::other(format!("no randomness available: {e}")))?;
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
    Err(io::Error::other(
        "could not create a private temporary folder",
    ))
}

/// True for a file [`private_temp_file`] made, also when `path` reaches the temporary folder
/// another way (through a link, such as `/private/var` for `/var` on macOS).
pub fn is_private_temp_file(path: &Path) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    let named_so = dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix(PRIVATE_PREFIX))
        .is_some_and(|digits| {
            digits.len() == PRIVATE_DIGITS
                && digits
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        });
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let temp = std::env::temp_dir();
    named_so
        && dir
            .parent()
            .is_some_and(|parent| parent == temp || canonical(parent) == canonical(&temp))
}

/// Removes a file [`private_temp_file`] made, and its folder. Any other path is refused.
pub fn remove_private_temp_file(path: &Path) -> io::Result<()> {
    if !is_private_temp_file(path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a private temporary file", path.display()),
        ));
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
/// new file gets what decides who may use the one it replaces: its group (and owner, when root
/// saves), its permission bits, and exactly its access control list (not the entries its folder
/// gives new files), on macOS also its extended attributes (Finder tags, where it was downloaded
/// from) and its hidden and no-backup flags. All but the permission bits are given before anything
/// is written into it. Where the access control list cannot be given to the new file, or the group
/// cannot and that would change who may do what with it, nothing is replaced. A target that cannot
/// be written (a read-only file) is refused instead of being replaced, as writing to it directly
/// would be, and so is a file of another user, as the new file would belong to whoever saves it.
/// The temporary file is removed again on any error.
///
/// On Windows only the read-only attribute is carried over, not the file's access control list.
/// Other hard links to the file keep the old contents: replacing the file is what makes the write
/// all-or-nothing, and writing into it in place would give that up.
pub fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    replace_with(path, |file| file.write_all(bytes), |_| Ok(()))
}

/// [`write_replacing`], but only while the file still holds `expected`: it is read again once the
/// new contents are ready, just before they take its place, and left alone if it changed. This narrows the time in which
/// another program's save could be lost to that last moment; nothing short of every program
/// taking part in a lock can close it.
pub fn write_replacing_if_unchanged(path: &Path, expected: &[u8], bytes: &[u8]) -> io::Result<()> {
    replace_with(
        path,
        |file| file.write_all(bytes),
        |target| match fs::read(target) {
            Ok(current) if current == expected => Ok(()),
            Ok(_) => Err(io::Error::other(format!(
                "{} was changed by another program",
                target.display()
            ))),
            Err(e) => Err(e),
        },
    )
}

/// `write_replacing` with the new contents written by `write`, and `ready` asked about the file
/// just before it is replaced.
fn replace_with(
    path: &Path,
    write: impl FnOnce(&mut File) -> io::Result<()>,
    ready: impl FnOnce(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let target = resolve_links(path)?;
    let existing = match fs::metadata(&target) {
        Ok(metadata) => Some(metadata),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if let Some(metadata) = &existing {
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a file", target.display()),
            ));
        }
        // Renaming over the file would succeed even where writing to it is not allowed.
        if let Err(e) = OpenOptions::new().write(true).open(&target) {
            return Err(io::Error::new(
                e.kind(),
                format!(
                    "{} is read-only or cannot be changed ({e})",
                    target.display()
                ),
            ));
        }
        #[cfg(unix)]
        check_owner(&target, metadata)?;
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
        // The contents go into a file whose access is already the old file's, not one with the
        // entries its folder gives new files.
        if let Some(metadata) = &existing {
            keep_access(&target, metadata, &file)?;
        }
        write(&mut file)?;
        // After writing, which takes the set-user and set-group bits off.
        if let Some(metadata) = &existing {
            keep_mode(metadata, &file)?;
        }
        file.sync_all()?;
        drop(file);
        ready(&target)?;
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

/// Refuses to replace `target` when it belongs to another user. The new file belongs to whoever
/// writes it, so a file of someone else that this user may change (through its group, say) would
/// change hands. Root gives the new file back to the owner, so it may.
#[cfg(unix)]
fn check_owner(target: &Path, existing: &fs::Metadata) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let user = unsafe { libc::geteuid() };
    if existing.uid() != user && user != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} belongs to another user, and replacing it would make it yours, so it was left as it is. Save a copy instead.", target.display()),
        ));
    }
    Ok(())
}

/// Gives `staged`, the new file about to replace `target`, what decides who may use `target` (see
/// [`write_replacing`]) but its permission bits, which [`keep_mode`] gives. Fails, so that nothing
/// is replaced, where the new file would otherwise let someone do more than the old one did.
#[cfg(unix)]
fn keep_access(target: &Path, existing: &fs::Metadata, staged: &File) -> io::Result<()> {
    use std::os::unix::fs::{fchown, MetadataExt};
    let created = staged.metadata()?;
    // Only root can give a file away; `check_owner` has made sure that any other user owns it.
    let owner = Some(existing.uid()).filter(|&uid| uid != created.uid());
    let group = Some(existing.gid()).filter(|&gid| gid != created.gid());
    // Before the permission bits: a new owner or group takes the set-user and set-group bits off.
    if owner.is_some() || group.is_some() {
        if let Err(e) = fchown(staged, owner, group) {
            if owner.is_some() {
                return Err(io::Error::new(
                    e.kind(),
                    format!("the new file could not be given to the owner of {} ({e}), so it was left as it is", target.display()),
                ));
            }
            if group_differs(existing.mode()) {
                return Err(io::Error::new(
                    e.kind(),
                    format!(
                        "{} gives its group other access than everyone else, and the new file could not be given that group ({e}), so it was left as it is: the new file would have given another group that access, and the old group everyone else's.",
                        target.display()
                    ),
                ));
            }
        }
    }
    copy_platform_metadata(target, staged, existing)
}

#[cfg(not(unix))]
fn keep_access(_target: &Path, _existing: &fs::Metadata, _staged: &File) -> io::Result<()> {
    Ok(())
}

/// Gives `staged` the permission bits of the file it replaces (on Windows, its read-only
/// attribute).
fn keep_mode(existing: &fs::Metadata, staged: &File) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        staged.set_permissions(fs::Permissions::from_mode(existing.mode() & 0o7777))
    }
    #[cfg(not(unix))]
    staged.set_permissions(existing.permissions())
}

/// True when the group permission bits of `mode` differ from its bits for everyone else: a file
/// like that, given another group, changes what someone may do with it. Members of the new group
/// get the group's access, and members of the old one fall back to everyone else's, more than
/// they had where the group was kept out (`0604`).
#[cfg(unix)]
fn group_differs(mode: u32) -> bool {
    (mode >> 3) & 0o7 != mode & 0o7
}

/// Copies the access control list, extended attributes and the hidden and no-backup flags of
/// `target` onto `staged`. Attributes and flags come along as far as they can; the access control
/// list has to, exactly, or this fails.
#[cfg(target_os = "macos")]
fn copy_platform_metadata(target: &Path, staged: &File, existing: &fs::Metadata) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::macos::fs::MetadataExt;
    if let Ok(source) = File::open(target) {
        // SAFETY: both descriptors are open for the duration of the call, and a null state is allowed.
        unsafe {
            libc::fcopyfile(
                source.as_raw_fd(),
                staged.as_raw_fd(),
                std::ptr::null_mut(),
                libc::COPYFILE_XATTR,
            );
        }
    }
    // The list the old file has, its inherited entries included, in place of the one the new file
    // got from its folder: entries the folder gives every new file could let others in that the
    // old file kept out. A file system without access control lists has none on either file.
    acl::copy_exact(target, staged);
    if acl::entries(acl::of_path(target)) != acl::entries(acl::of_file(staged)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("the access control list of {} could not be given to the new file, so it was left as it is", target.display()),
        ));
    }
    let flags = existing.st_flags() & (libc::UF_HIDDEN | libc::UF_NODUMP);
    if flags != 0 {
        // SAFETY: the descriptor is open.
        unsafe {
            libc::fchflags(staged.as_raw_fd(), flags);
        }
    }
    Ok(())
}

/// Copies the access control list of `target`, which Linux keeps in the extended attribute
/// `system.posix_acl_access`, onto `staged`. A file system without access control lists has
/// nothing to copy; one that has a list it cannot copy fails.
#[cfg(target_os = "linux")]
fn copy_platform_metadata(
    target: &Path,
    staged: &File,
    _existing: &fs::Metadata,
) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let name = c"system.posix_acl_access";
    let path = CString::new(target.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let refused = |e: io::Error| {
        io::Error::new(e.kind(), format!("the access control list of {} could not be given to the new file ({e}), so it was left as it is", target.display()))
    };
    // SAFETY: valid NUL-terminated strings; a null buffer of length 0 asks for the size.
    let size = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    if size < 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            // No list, so the new file may have none either, even one its folder's default list gave it.
            // SAFETY: the descriptor is open and the name a valid NUL-terminated string.
            Some(libc::ENODATA) => match unsafe {
                libc::fremovexattr(staged.as_raw_fd(), name.as_ptr())
            } {
                0 => Ok(()),
                _ => match io::Error::last_os_error() {
                    e if matches!(e.raw_os_error(), Some(libc::ENODATA) | Some(libc::ENOTSUP)) => {
                        Ok(())
                    }
                    e => Err(refused(e)),
                },
            },
            Some(libc::ENOTSUP) => Ok(()),
            _ => Err(refused(error)),
        };
    }
    let mut value = vec![0u8; size as usize];
    // SAFETY: as above, with a writable buffer of the given length.
    let read = unsafe {
        libc::getxattr(
            path.as_ptr(),
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if read < 0 {
        return Err(refused(io::Error::last_os_error()));
    }
    // SAFETY: the descriptor is open, and the value is a buffer of the given length.
    let set = unsafe {
        libc::fsetxattr(
            staged.as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast(),
            read as usize,
            0,
        )
    };
    if set != 0 {
        return Err(refused(io::Error::last_os_error()));
    }
    Ok(())
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn copy_platform_metadata(
    _target: &Path,
    _staged: &File,
    _existing: &fs::Metadata,
) -> io::Result<()> {
    Ok(())
}

/// Access control lists on macOS, which the libc crate has no bindings for.
#[cfg(target_os = "macos")]
mod acl {
    use std::ffi::{c_char, c_int, c_void, CStr, CString};
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    /// `ACL_TYPE_EXTENDED` from <sys/acl.h>, the kind of list macOS keeps.
    const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;

    extern "C" {
        fn acl_get_file(path: *const c_char, kind: c_int) -> *mut c_void;
        fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
        fn acl_set_fd_np(fd: c_int, acl: *mut c_void, kind: c_int) -> c_int;
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_to_text(acl: *mut c_void, length: *mut isize) -> *mut c_char;
        fn acl_free(object: *mut c_void) -> c_int;
    }

    /// Gives `file` the access control list of the file at `path` as it is, inherited entries
    /// included, or an empty one when that file has none. Whether that worked is for the caller
    /// to check by comparing the two.
    pub fn copy_exact(path: &Path, file: &std::fs::File) {
        let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
            return;
        };
        // SAFETY: a valid NUL-terminated path; a list that is returned is freed once, below.
        let mut acl = unsafe { acl_get_file(path.as_ptr(), ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            // SAFETY: no preconditions.
            acl = unsafe { acl_init(0) };
        }
        if acl.is_null() {
            return;
        }
        // SAFETY: the descriptor is open and `acl` is a list from the calls above, freed once.
        unsafe {
            acl_set_fd_np(file.as_raw_fd(), acl, ACL_TYPE_EXTENDED);
            acl_free(acl);
        }
    }

    /// The access control list of the file at `path`, as text; `None` when it has none.
    pub fn of_path(path: &Path) -> Option<String> {
        let path = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: a valid NUL-terminated path.
        text(unsafe { acl_get_file(path.as_ptr(), ACL_TYPE_EXTENDED) })
    }

    /// The access control list of an open file, as text; `None` when it has none.
    pub fn of_file(file: &std::fs::File) -> Option<String> {
        // SAFETY: the descriptor is open.
        text(unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) })
    }

    /// The entries of a list given as text, in order: every line but the header. No list has none.
    pub fn entries(text: Option<String>) -> Vec<String> {
        text.unwrap_or_default()
            .lines()
            .filter(|line| !line.starts_with("!#acl"))
            .map(str::to_owned)
            .collect()
    }

    /// `acl`, which this frees, as text.
    fn text(acl: *mut c_void) -> Option<String> {
        if acl.is_null() {
            return None;
        }
        // SAFETY: `acl` is a list one of the functions above returned; it and its text are each
        // freed once, the text after it is copied.
        unsafe {
            let text = acl_to_text(acl, std::ptr::null_mut());
            acl_free(acl);
            if text.is_null() {
                return None;
            }
            let owned = CStr::from_ptr(text).to_string_lossy().into_owned();
            acl_free(text.cast());
            Some(owned)
        }
    }
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
    Err(io::Error::other(format!(
        "too many symbolic links at {}",
        path.display()
    )))
}

/// Creates a new file in `dir` for writing what is then renamed into place, under a name nobody
/// can guess: `.carbide-` and 16 random hexadecimal digits. `mode` is the Unix permission mode it
/// is created with (before the umask). The name is short, so it fits wherever the target's name
/// does. See [`create_exclusive`] for what is at a name already.
pub(crate) fn create_temp(dir: &Path, mode: u32) -> io::Result<(PathBuf, File)> {
    for _ in 0..100 {
        let mut random = [0u8; 8];
        getrandom::fill(&mut random)
            .map_err(|e| io::Error::other(format!("no randomness available: {e}")))?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let temp = dir.join(format!(".carbide-{suffix}.tmp"));
        match create_exclusive(&temp, mode) {
            Ok(file) => return Ok((temp, file)),
            // Left over from another process, or put there by someone else: try another name.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free temporary file name in {}", dir.display()),
    ))
}

/// Creates the file `path`, which must not exist: anything already there, a symbolic link
/// included (which is never followed), fails with `AlreadyExists` and is left as it is.
fn create_exclusive(path: &Path, mode: u32) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    #[cfg(not(unix))]
    let _ = mode;
    options.open(path)
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
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_failed_write_leaves_the_file_and_no_temporary_file() {
        let dir = scratch("failed");
        let path = dir.join("doc.pdf");
        fs::write(&path, b"original").unwrap();
        let result = replace_with(
            &path,
            |file| {
                file.write_all(b"half")?;
                Err(io::Error::other("disk full"))
            },
            |_| Ok(()),
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"original");
        assert_eq!(names(&dir), ["doc.pdf"]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_changed_while_its_replacement_was_written_is_left_alone() {
        let dir = scratch("changed");
        let path = dir.join("doc.pdf");
        fs::write(&path, b"original").unwrap();
        // Another program saves while the new contents are being written.
        let result = replace_with(
            &path,
            |file| {
                fs::write(&path, b"theirs")?;
                file.write_all(b"ours")
            },
            |target| match fs::read(target) {
                Ok(current) if current == b"original" => Ok(()),
                Ok(_) => Err(io::Error::other("changed")),
                Err(e) => Err(e),
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"theirs");
        assert_eq!(names(&dir), ["doc.pdf"]);
        write_replacing_if_unchanged(&path, b"theirs", b"ours").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"ours");
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
        assert_eq!(
            write_file(&path, b"second", Existing::Refuse)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&path).unwrap(), b"first");

        assert_eq!(
            write_file(&path, b"second", Existing::KeepBoth).unwrap(),
            dir.join("page-001 (2).png")
        );
        assert_eq!(
            write_file(&path, b"third", Existing::KeepBoth).unwrap(),
            dir.join("page-001 (3).png")
        );
        assert_eq!(fs::read(&path).unwrap(), b"first");
        assert_eq!(
            write_file(&path, b"replaced", Existing::Replace).unwrap(),
            path
        );
        assert_eq!(fs::read(&path).unwrap(), b"replaced");
        assert_eq!(
            names(&dir),
            ["page-001 (2).png", "page-001 (3).png", "page-001.png"]
        );
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
        assert_eq!(
            write_file(&path, b"new", Existing::KeepBoth).unwrap(),
            dir.join("page (52).png")
        );
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
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Two copies never share a folder.
        let other = private_temp_file("revision - v1.pdf", b"copy").unwrap();
        assert_ne!(other.parent(), path.parent());

        // However the temporary folder is reached.
        assert!(is_private_temp_file(&path.canonicalize().unwrap()));

        assert!(private_temp_file("../escape.pdf", b"x").is_err());
        assert!(remove_private_temp_file(&std::env::temp_dir().join("not-ours.pdf")).is_err());
        assert!(!is_private_temp_file(
            &std::env::temp_dir()
                .join("carbide-copy-notes")
                .join("a.pdf")
        ));
        remove_private_temp_file(&path).unwrap();
        remove_private_temp_file(&other).unwrap();
        assert!(!dir.exists());
    }

    #[cfg(unix)]
    #[test]
    fn staging_files_get_names_nobody_can_guess_and_never_follow_links() {
        use std::os::unix::fs::symlink;
        let dir = scratch("staging");
        let victim = dir.join("victim");
        fs::write(&victim, b"untouched").unwrap();
        // Links at every name a counter of this process would give.
        for n in 0..200 {
            symlink(
                &victim,
                dir.join(format!(".carbide-{}-{n}.tmp", std::process::id())),
            )
            .unwrap();
        }
        write_replacing(&dir.join("doc.pdf"), b"replaced").unwrap();
        write_replacing(&dir.join("doc.pdf"), b"replaced again").unwrap();
        write_new(&dir.join("new.pdf"), b"new").unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"untouched");
        assert_eq!(fs::read(dir.join("doc.pdf")).unwrap(), b"replaced again");

        // Every name is new, and owes nothing to the process.
        let (a, _) = create_temp(&dir, 0o600).unwrap();
        let (b, _) = create_temp(&dir, 0o600).unwrap();
        assert_ne!(a, b);
        let name = a.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            name.len() == ".carbide-.tmp".len() + 16
                && !name.contains(&std::process::id().to_string()),
            "{name}"
        );

        // A link at the name itself fails the attempt rather than being written through.
        let link = dir.join(".carbide-planted.tmp");
        symlink(&victim, &link).unwrap();
        assert_eq!(
            create_exclusive(&link, 0o600).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&victim).unwrap(), b"untouched");
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

    #[cfg(unix)]
    #[test]
    fn a_replaced_file_keeps_its_group() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = scratch("group");
        let path = dir.join("shared.pdf");
        fs::write(&path, b"old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let default = fs::metadata(&path).unwrap().gid();
        // Another group this user is in, which a new file in the folder would not get.
        let mut groups = vec![0 as libc::gid_t; 256];
        // SAFETY: a buffer of the length given.
        let count = unsafe { libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr()) };
        groups.truncate(count.max(0) as usize);
        let other = groups
            .into_iter()
            .filter(|&g| g != default)
            .find(|&g| std::os::unix::fs::chown(&path, None, Some(g)).is_ok());
        let Some(other) = other else {
            eprintln!("skipped: this user is in no other group");
            let _ = fs::remove_dir_all(&dir);
            return;
        };
        write_replacing(&path, b"new").unwrap();
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!((metadata.gid(), metadata.mode() & 0o777), (other, 0o640));
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_group_that_cannot_be_kept_and_was_kept_out_stops_the_replacement() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let dir = scratch("foreign-group");
        // A file made in /tmp gets its group (wheel on macOS), which this user may not be in.
        let made = Path::new("/tmp").join(format!("carbide-fs-foreign-{}", std::process::id()));
        if fs::write(&made, b"old").is_err() {
            eprintln!("skipped: /tmp cannot be written");
            return;
        }
        let path = dir.join("kept-out.pdf");
        let moved = fs::rename(&made, &path);
        let gid = fs::metadata(&path).map(|m| m.gid()).ok();
        let local = fs::metadata(&dir).unwrap().gid();
        // SAFETY: no preconditions; `getgroups` is given a buffer of the length passed.
        let member = gid.is_some_and(|gid| unsafe {
            let mut groups = vec![0 as libc::gid_t; 256];
            let count = libc::getgroups(groups.len() as libc::c_int, groups.as_mut_ptr());
            libc::getegid() == gid || groups[..count.max(0) as usize].contains(&gid)
        });
        if moved.is_err() || gid == Some(local) || member || unsafe { libc::geteuid() } == 0 {
            eprintln!("skipped: no group here that this user is not in");
            let _ = fs::remove_file(&made);
            let _ = fs::remove_dir_all(&dir);
            return;
        }
        // Everyone may read it but its group: given the folder's group, the old group could read it.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o604)).unwrap();
        let error = write_replacing(&path, b"new").unwrap_err();
        assert!(
            error.to_string().contains("could not be given that group"),
            "{error}"
        );
        assert_eq!(fs::read(&path).unwrap(), b"old");
        // With the same access for the group as for everyone else, nobody's access changes.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        write_replacing(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_group_with_access_of_its_own_is_told_apart() {
        // More than everyone else, or less: the group kept out of a file others may read.
        assert!(
            group_differs(0o640)
                && group_differs(0o664)
                && group_differs(0o674)
                && group_differs(0o604)
                && group_differs(0o606)
        );
        assert!(
            !group_differs(0o600)
                && !group_differs(0o644)
                && !group_differs(0o666)
                && !group_differs(0o755)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_access_control_list_comes_along() {
        let dir = scratch("acl");
        let path = dir.join("restricted.pdf");
        fs::write(&path, b"old").unwrap();
        let list = |path: &Path| {
            let output = std::process::Command::new("/bin/ls")
                .arg("-le")
                .arg(path)
                .output()
                .unwrap();
            String::from_utf8_lossy(&output.stdout).into_owned()
        };
        let added = std::process::Command::new("/bin/chmod")
            .args(["+a", "user:nobody deny read"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(added.success());
        assert!(list(&path).contains("user:nobody deny read"));
        write_replacing(&path, b"new").unwrap();
        assert!(
            list(&path).contains("user:nobody deny read"),
            "{}",
            list(&path)
        );
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let _ = fs::remove_dir_all(&dir);
    }

    /// What `ls -le` says about `path`: its mode and access control list entries.
    #[cfg(target_os = "macos")]
    fn listing(path: &Path) -> String {
        let output = std::process::Command::new("/bin/ls")
            .arg("-le")
            .arg(path)
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[cfg(target_os = "macos")]
    fn chmod(args: &[&str], path: &Path) {
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(args)
                .arg(path)
                .status()
                .unwrap()
                .success(),
            "chmod {args:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_replaced_file_gets_no_entries_its_folder_gives_new_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("acl-folder");
        // Every new file in the folder may be read by everyone; this one is private.
        chmod(&["+a", "everyone allow read,file_inherit"], &dir);
        let path = dir.join("private.pdf");
        fs::write(&path, b"old").unwrap();
        chmod(&["-N"], &path);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!listing(&path).contains("allow"), "{}", listing(&path));
        write_replacing(&path, b"new").unwrap();
        assert!(!listing(&path).contains("everyone"), "{}", listing(&path));
        assert_eq!(fs::read(&path).unwrap(), b"new");

        // With an entry of its own, that one alone comes along.
        chmod(&["+a", "user:nobody deny read"], &path);
        write_replacing(&path, b"newer").unwrap();
        let listed = listing(&path);
        assert!(
            listed.contains("user:nobody deny read") && !listed.contains("everyone"),
            "{listed}"
        );
        chmod(&["-N"], &dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_entry_the_folder_no_longer_gives_stays_with_the_file() {
        let dir = scratch("acl-stale");
        chmod(&["+a", "user:nobody allow read,file_inherit"], &dir);
        let path = dir.join("inherited.pdf");
        fs::write(&path, b"old").unwrap();
        assert!(
            listing(&path).contains("user:nobody inherited allow read"),
            "{}",
            listing(&path)
        );
        chmod(&["-N"], &dir);
        write_replacing(&path, b"new").unwrap();
        assert!(
            listing(&path).contains("user:nobody inherited allow read"),
            "{}",
            listing(&path)
        );
        assert_eq!(fs::read(&path).unwrap(), b"new");
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
        let set = unsafe {
            libc::setxattr(
                c_path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        assert_eq!(set, 0);
        write_replacing(&path, b"new").unwrap();
        let mut buffer = [0u8; 16];
        // SAFETY: as above, with a writable buffer.
        let read = unsafe {
            libc::getxattr(
                c_path.as_ptr(),
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
                0,
            )
        };
        assert_eq!(&buffer[..read.max(0) as usize], value);
        assert_eq!(fs::read(&path).unwrap(), b"new");
        let _ = fs::remove_dir_all(&dir);
    }
}
