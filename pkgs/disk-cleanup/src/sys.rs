//! Isolated POSIX system calls for Linux (`x86_64-linux`) and macOS (`aarch64-darwin`).
//!
//! All `unsafe` FFI interactions live in this module so the rest of the crate stays
//! safe Rust with strongly typed boundaries.

use std::ffi::{CStr, CString, OsStr, OsString};
use std::fmt;
use std::fs;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::ptr;

use crate::constants::{
    FORBIDDEN_DELETE_PATHS, PASSWD_BUF_BYTES_INITIAL, PASSWD_BUF_BYTES_MAX, RMDIR_BATCH_MAX,
    TREE_WALK_ENTRIES_MAX, USER_GROUPS_MAX, WALK_DEPTH_MAX,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswdEntry {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilesystemStat {
    pub device_id: u64,
    pub fragment_size_bytes: u64,
    pub blocks_total: u64,
    pub blocks_free: u64,
    pub blocks_available: u64,
}

#[derive(Debug)]
pub enum RmtreeError {
    UnsafePath(String),
    Io(io::Error),
}

impl fmt::Display for RmtreeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsafePath(message) => formatter.write_str(message),
            Self::Io(err) => err.fmt(formatter),
        }
    }
}

impl From<io::Error> for RmtreeError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Exclusive advisory lockfile held via `flock(LOCK_EX | LOCK_NB)`.
///
/// Unlocks and closes the underlying file descriptor automatically on drop.
pub struct AdvisoryLock {
    fd: OwnedFd,
    path: PathBuf,
}

impl AdvisoryLock {
    /// Attempt to acquire a non-blocking exclusive advisory lock on `lock_path`.
    ///
    /// Returns `Ok(Some(lock))` when acquired, `Ok(None)` when another process
    /// holds the lock, or `Err(err)` if the lockfile cannot be created or locked.
    pub fn try_acquire(lock_path: &Path) -> io::Result<Option<Self>> {
        assert!(lock_path.is_absolute(), "lock_path must be absolute");

        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let c_path = path_to_cstring(lock_path)?;
        let flags = libc::O_CREAT | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        let mode: libc::c_uint = 0o600;

        // SAFETY: `c_path` is a valid NUL-terminated path; `mode` is promoted `c_uint` for varargs.
        let raw_fd = unsafe { libc::open(c_path.as_ptr(), flags, mode) };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        assert!(raw_fd >= 0, "open returned negative fd");

        // SAFETY: `raw_fd` is a freshly opened owned file descriptor.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

        // SAFETY: `fd` is a valid open file descriptor.
        let lock_rc = unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if lock_rc == 0 {
            return Ok(Some(Self {
                fd,
                path: lock_path.to_path_buf(),
            }));
        }

        let err = io::Error::last_os_error();
        let raw_errno = err.raw_os_error();
        if raw_errno == Some(libc::EWOULDBLOCK) || raw_errno == Some(libc::EAGAIN) {
            return Ok(None);
        }
        Err(err)
    }
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        // SAFETY: `self.fd` is a valid open file descriptor owned by `self`.
        let unlock_rc = unsafe { libc::flock(self.fd.as_raw_fd(), libc::LOCK_UN) };
        if unlock_rc != 0 {
            let err = io::Error::last_os_error();
            eprintln!(
                "disk-cleanup: warning: failed to unlock {}: {err}",
                self.path.display()
            );
        }
    }
}

/// Outcome of probing whether a file is held by an exclusive `flock`.
#[derive(Debug)]
pub enum LockProbe {
    Unlocked,
    Locked,
    /// Unexpected `flock` error (e.g., `ENOLCK`); caller logs warning and fails closed.
    UnexpectedError(io::Error),
}

/// Probe whether `lock_file` is locked via `flock(LOCK_EX | LOCK_NB)`.
///
/// Missing files report `Unlocked`. Symlinks and unreadable files fail closed
/// as `Locked`.
pub fn probe_exclusive_flock(lock_file: &Path) -> LockProbe {
    assert!(lock_file.is_absolute(), "lock_file must be absolute");

    let c_path = match path_to_cstring(lock_file) {
        Ok(value) => value,
        Err(_) => return LockProbe::Locked,
    };

    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `c_path` is a valid NUL-terminated path and `O_CREAT` is not set.
    let raw_fd = unsafe { libc::open(c_path.as_ptr(), flags) };
    if raw_fd < 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::NotFound {
            return LockProbe::Unlocked;
        }
        // Symlink (ELOOP) or unreadable lock file: fail closed.
        return LockProbe::Locked;
    }
    assert!(raw_fd >= 0, "open returned negative fd");

    // SAFETY: `raw_fd` is a freshly opened owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

    // SAFETY: `fd` is open and valid.
    let rc = unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        // SAFETY: `fd` holds the exclusive lock we just acquired.
        let unlock_rc = unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_UN) };
        if unlock_rc != 0 {
            let err = io::Error::last_os_error();
            eprintln!(
                "disk-cleanup: warning: failed to unlock probe on {}: {err}",
                lock_file.display()
            );
        }
        return LockProbe::Unlocked;
    }

    let err = io::Error::last_os_error();
    let raw_errno = err.raw_os_error();
    if raw_errno == Some(libc::EAGAIN)
        || raw_errno == Some(libc::EWOULDBLOCK)
        || raw_errno == Some(libc::EACCES)
    {
        return LockProbe::Locked;
    }
    LockProbe::UnexpectedError(err)
}

/// Read up to `buf.len()` bytes from a regular file without following symlinks.
///
/// Returns `Ok(None)` if the path does not exist, is a symlink, or is not a
/// regular file.
pub fn read_regular_prefix_nofollow(path: &Path, buf: &mut [u8]) -> io::Result<Option<usize>> {
    assert!(path.is_absolute(), "path must be absolute");
    assert!(!buf.is_empty(), "read buffer must be non-empty");

    let c_path = path_to_cstring(path)?;
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `c_path` is NUL-terminated and `O_CREAT` is not set.
    let raw_fd = unsafe { libc::open(c_path.as_ptr(), flags) };
    if raw_fd < 0 {
        let err = io::Error::last_os_error();
        let raw_errno = err.raw_os_error();
        if err.kind() == io::ErrorKind::NotFound
            || raw_errno == Some(libc::ELOOP)
            || raw_errno == Some(libc::EMLINK)
            || raw_errno == Some(libc::EISDIR)
        {
            return Ok(None);
        }
        return Err(err);
    }

    // SAFETY: `raw_fd` is a freshly opened owned descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let stat_buf = fstat_fd(fd.as_raw_fd())?;
    if (stat_buf.st_mode & libc::S_IFMT) != libc::S_IFREG {
        return Ok(None);
    }

    let mut bytes_read_total: usize = 0;
    // Bounded loop: at most `buf.len() * 2 + 1` iterations (each progress adds >= 1 byte,
    // allowing bounded EINTR retries).
    let iterations_max = buf.len().saturating_mul(2).saturating_add(16);
    for _ in 0..iterations_max {
        if bytes_read_total >= buf.len() {
            break;
        }
        let target_slice = match buf.get_mut(bytes_read_total..) {
            Some(slice) => slice,
            None => break,
        };
        // SAFETY: `fd` is valid and `target_slice` is valid writable memory.
        let read_rc = unsafe {
            libc::read(
                fd.as_raw_fd(),
                target_slice.as_mut_ptr().cast::<libc::c_void>(),
                target_slice.len(),
            )
        };
        if read_rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if read_rc == 0 {
            break;
        }
        let Ok(chunk_bytes) = usize::try_from(read_rc) else {
            panic!("non-negative read return fits usize");
        };
        let Some(next_total) = bytes_read_total.checked_add(chunk_bytes) else {
            panic!("bytes_read_total overflow");
        };
        bytes_read_total = next_total;
    }

    assert!(bytes_read_total <= buf.len());
    Ok(Some(bytes_read_total))
}

pub fn effective_uid() -> u32 {
    // SAFETY: `geteuid` has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

pub fn real_uid() -> u32 {
    // SAFETY: `getuid` has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// Look up a user account by name via reentrant `getpwnam_r`.
pub fn passwd_by_name(user_name: &str) -> io::Result<Option<PasswdEntry>> {
    assert!(!user_name.is_empty(), "user_name must be non-empty");
    let c_name = CString::new(user_name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "user name contains interior NUL byte",
        )
    })?;

    let mut buf_size = PASSWD_BUF_BYTES_INITIAL;
    for _ in 0..8 {
        assert!(buf_size <= PASSWD_BUF_BYTES_MAX);
        let mut pwd = MaybeUninit::<libc::passwd>::uninit();
        let mut storage = vec![0u8; buf_size];
        let mut result_ptr: *mut libc::passwd = ptr::null_mut();

        // SAFETY: `c_name`, `pwd`, `storage`, and `result_ptr` are valid pointers
        // with matching lengths.
        let rc = unsafe {
            libc::getpwnam_r(
                c_name.as_ptr(),
                pwd.as_mut_ptr(),
                storage.as_mut_ptr().cast::<libc::c_char>(),
                storage.len(),
                &mut result_ptr,
            )
        };

        if rc == libc::ERANGE {
            let next_size = buf_size.saturating_mul(2);
            if next_size > PASSWD_BUF_BYTES_MAX {
                return Err(io::Error::from_raw_os_error(libc::ERANGE));
            }
            buf_size = next_size;
            continue;
        }
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        if result_ptr.is_null() {
            return Ok(None);
        }

        // SAFETY: `result_ptr` is non-null and points to `pwd` populated by `getpwnam_r`.
        let entry = unsafe { extract_passwd_entry(&*result_ptr)? };
        return Ok(Some(entry));
    }

    Err(io::Error::from_raw_os_error(libc::ERANGE))
}

/// Look up a user account by UID via reentrant `getpwuid_r`.
pub fn passwd_by_uid(uid: u32) -> io::Result<Option<PasswdEntry>> {
    let mut buf_size = PASSWD_BUF_BYTES_INITIAL;
    for _ in 0..8 {
        assert!(buf_size <= PASSWD_BUF_BYTES_MAX);
        let mut pwd = MaybeUninit::<libc::passwd>::uninit();
        let mut storage = vec![0u8; buf_size];
        let mut result_ptr: *mut libc::passwd = ptr::null_mut();

        // SAFETY: `pwd`, `storage`, and `result_ptr` are valid pointers with matching lengths.
        let rc = unsafe {
            libc::getpwuid_r(
                uid,
                pwd.as_mut_ptr(),
                storage.as_mut_ptr().cast::<libc::c_char>(),
                storage.len(),
                &mut result_ptr,
            )
        };

        if rc == libc::ERANGE {
            let next_size = buf_size.saturating_mul(2);
            if next_size > PASSWD_BUF_BYTES_MAX {
                return Err(io::Error::from_raw_os_error(libc::ERANGE));
            }
            buf_size = next_size;
            continue;
        }
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        if result_ptr.is_null() {
            return Ok(None);
        }

        // SAFETY: `result_ptr` is non-null and points to `pwd` populated by `getpwuid_r`.
        let entry = unsafe { extract_passwd_entry(&*result_ptr)? };
        return Ok(Some(entry));
    }

    Err(io::Error::from_raw_os_error(libc::ERANGE))
}

unsafe fn extract_passwd_entry(pwd: &libc::passwd) -> io::Result<PasswdEntry> {
    if pwd.pw_name.is_null() || pwd.pw_dir.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "passwd entry has null name or home directory",
        ));
    }
    // SAFETY: Checked non-null; `getpwnam_r`/`getpwuid_r` populate NUL-terminated C strings.
    let name_bytes = unsafe { CStr::from_ptr(pwd.pw_name) }.to_bytes();
    let dir_bytes = unsafe { CStr::from_ptr(pwd.pw_dir) }.to_bytes();
    let name = String::from_utf8(name_bytes.to_vec())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "passwd username is not UTF-8"))?;
    let home_dir = PathBuf::from(OsString::from_vec(dir_bytes.to_vec()));
    assert!(!name.is_empty(), "passwd name must be non-empty");
    Ok(PasswdEntry {
        name,
        uid: pwd.pw_uid,
        gid: pwd.pw_gid,
        home_dir,
    })
}

/// Resolve supplementary group IDs for `user_name` via `getgrouplist`.
pub fn user_groups(user_name: &str, primary_gid: u32) -> io::Result<Vec<u32>> {
    assert!(!user_name.is_empty(), "user_name must be non-empty");
    let c_name = CString::new(user_name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "user name contains interior NUL byte",
        )
    })?;

    #[cfg(target_os = "macos")]
    type GroupElem = libc::c_int;
    #[cfg(not(target_os = "macos"))]
    type GroupElem = libc::gid_t;

    let base_gid: GroupElem = {
        #[cfg(target_os = "macos")]
        {
            libc::c_int::try_from(primary_gid).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "primary_gid exceeds c_int::MAX",
                )
            })?
        }
        #[cfg(not(target_os = "macos"))]
        {
            primary_gid
        }
    };

    let mut capacity: usize = 64;
    for _ in 0..2 {
        assert!(capacity > 0);
        assert!(capacity <= USER_GROUPS_MAX);
        let mut raw_groups: Vec<GroupElem> = vec![0; capacity];
        let Ok(mut count) = libc::c_int::try_from(capacity) else {
            panic!("capacity fits c_int");
        };

        // SAFETY: `c_name`, `raw_groups`, and `count` are valid and match `capacity`.
        let rc = unsafe {
            libc::getgrouplist(
                c_name.as_ptr(),
                base_gid,
                raw_groups.as_mut_ptr(),
                &mut count,
            )
        };

        if rc >= 0 {
            let actual_len = usize::try_from(count).unwrap_or(0).min(raw_groups.len());
            let mut result = Vec::with_capacity(actual_len);
            for raw_gid in raw_groups.into_iter().take(actual_len) {
                #[cfg(target_os = "macos")]
                {
                    if let Ok(gid_u32) = u32::try_from(raw_gid) {
                        result.push(gid_u32);
                    }
                }
                #[cfg(not(target_os = "macos"))]
                {
                    result.push(raw_gid);
                }
            }
            return Ok(result);
        }

        let needed = usize::try_from(count).unwrap_or(USER_GROUPS_MAX);
        capacity = needed.clamp(capacity.saturating_add(1), USER_GROUPS_MAX);
    }

    Err(io::Error::other(format!(
        "getgrouplist exceeded {USER_GROUPS_MAX} groups limit"
    )))
}

/// Configure a `Command` to drop privileges (`setgroups` -> `setgid` -> `setuid`)
/// atomically in `pre_exec` before `execve`.
pub fn configure_privilege_drop(
    cmd: &mut Command,
    user_uid: u32,
    group_gid: u32,
    extra_groups: &[u32],
) {
    assert!(extra_groups.len() <= USER_GROUPS_MAX);
    let groups: Vec<libc::gid_t> = extra_groups.to_vec();

    // SAFETY: The closure calls only async-signal-safe POSIX functions
    // (`setgroups`, `setgid`, `setuid`) and performs no heap allocations.
    // All three execute in canonical privilege-drop order before `execve`.
    unsafe {
        cmd.pre_exec(move || {
            #[cfg(target_os = "macos")]
            let groups_rc = {
                let len_cint = libc::c_int::try_from(groups.len())
                    .map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
                libc::setgroups(len_cint, groups.as_ptr())
            };
            #[cfg(not(target_os = "macos"))]
            let groups_rc = libc::setgroups(groups.len(), groups.as_ptr());

            if groups_rc != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setgid(group_gid) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setuid(user_uid) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Query filesystem block statistics and device ID for `path`.
pub fn statvfs_path(path: &Path) -> io::Result<FilesystemStat> {
    assert!(path.is_absolute(), "statvfs_path requires an absolute path");

    let metadata = fs::metadata(path)?;
    let device_id = std::os::unix::fs::MetadataExt::dev(&metadata);

    let c_path = path_to_cstring(path)?;
    let mut vfs = MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c_path` is NUL-terminated and `vfs` is a valid out-pointer.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), vfs.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `statvfs` returned 0, so `vfs` is initialized.
    let vfs = unsafe { vfs.assume_init() };

    // `fsblkcnt_t` is `u32` on macOS and `u64` on Linux; `u64::from` handles both without `as`.
    #[allow(clippy::useless_conversion)]
    let fragment_size_bytes = u64::from(vfs.f_frsize);
    #[allow(clippy::useless_conversion)]
    let blocks_total = u64::from(vfs.f_blocks);
    #[allow(clippy::useless_conversion)]
    let blocks_free = u64::from(vfs.f_bfree);
    #[allow(clippy::useless_conversion)]
    let blocks_available = u64::from(vfs.f_bavail);

    Ok(FilesystemStat {
        device_id,
        fragment_size_bytes,
        blocks_total,
        blocks_free,
        blocks_available,
    })
}

/// Check whether a process with `pid` is alive.
pub fn is_pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if Path::new("/proc").is_dir() {
        return PathBuf::from(format!("/proc/{pid}")).exists();
    }
    let Ok(pid_t) = libc::pid_t::try_from(pid) else {
        return false;
    };
    assert!(pid_t > 0, "pid_t must be positive before kill(pid, 0)");

    // SAFETY: `signal = 0` checks process existence without delivering a signal.
    let rc = unsafe { libc::kill(pid_t, 0) };
    if rc == 0 {
        return true;
    }
    let err = io::Error::last_os_error();
    // `EPERM` means the process exists but belongs to another user.
    err.raw_os_error() == Some(libc::EPERM)
}

/// Set `O_NONBLOCK` on a file descriptor.
pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    assert!(fd >= 0, "fd must be non-negative");
    // SAFETY: `fd` is a valid open descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is valid and `flags | O_NONBLOCK` preserves existing flags.
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Wait for readiness on up to 2 file descriptors using POSIX `poll`.
pub fn poll_fds(fds: &mut [libc::pollfd], timeout_ms: i32) -> io::Result<usize> {
    assert!(!fds.is_empty(), "poll_fds requires at least one fd");
    assert!(fds.len() <= 2, "poll_fds only supports up to 2 streams");
    assert!(timeout_ms >= 0, "timeout_ms must be non-negative");

    let Ok(nfds) = libc::nfds_t::try_from(fds.len()) else {
        panic!("pollfd count fits nfds_t");
    };
    // SAFETY: `fds` points to `nfds` valid `pollfd` entries.
    let rc = unsafe { libc::poll(fds.as_mut_ptr(), nfds, timeout_ms) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let Ok(ready) = usize::try_from(rc) else {
        panic!("non-negative poll return fits usize");
    };
    Ok(ready)
}

/// Non-blocking read from a raw file descriptor into `buf`.
pub fn read_fd_nonblocking(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    assert!(fd >= 0, "fd must be non-negative");
    assert!(!buf.is_empty(), "read buffer must be non-empty");

    // SAFETY: `fd` is valid and `buf` is valid writable memory.
    let rc = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let Ok(bytes_read) = usize::try_from(rc) else {
        panic!("non-negative read return fits usize");
    };
    Ok(bytes_read)
}

/// Format current local wall-clock time as `YYYY-MM-DD HH:MM:SS`.
pub fn format_local_timestamp() -> String {
    // SAFETY: `time(NULL)` returns current epoch seconds.
    let now: libc::time_t = unsafe { libc::time(ptr::null_mut()) };
    let mut tm = MaybeUninit::<libc::tm>::zeroed();
    // SAFETY: `now` and `tm` are valid pointers; `localtime_r` is thread-safe.
    let ptr = unsafe { libc::localtime_r(&now, tm.as_mut_ptr()) };
    if ptr.is_null() {
        return format!("{now}");
    }
    // SAFETY: `localtime_r` returned non-null, initializing `tm`.
    let tm = unsafe { tm.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}

/// Check whether `path` is one of the protected system roots.
pub fn is_forbidden_delete_path(path: &Path) -> bool {
    assert!(path.is_absolute(), "path must be absolute");
    FORBIDDEN_DELETE_PATHS
        .iter()
        .any(|forbidden| path == Path::new(forbidden))
}

/// Lexically normalize `path` to an absolute path without resolving symlinks
/// (equivalent to Python's `os.path.abspath`).
pub fn absolutize_path(path: &Path) -> io::Result<PathBuf> {
    assert!(!path.as_os_str().is_empty(), "path must not be empty");
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Component::RootDir.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }

    assert!(normalized.is_absolute(), "normalized path must be absolute");
    Ok(normalized)
}

/// Delete a real directory tree iteratively using `dirfd` and `O_NOFOLLOW`.
///
/// Never follows symlinks, never crosses filesystem mountpoints, never chmods
/// the parent directory of `root`, and bounds depth and total entry count.
pub fn safe_rmtree(root: &Path) -> Result<(), RmtreeError> {
    safe_rmtree_with_batch_max(root, RMDIR_BATCH_MAX)
}

struct RmDirFrame {
    name: CString,
    dir_fd: OwnedFd,
    depth: usize,
    chmod_done: bool,
    pending_subdirs: Vec<CString>,
}

/// Same as `safe_rmtree` with an explicit per-scan batch cap (bounded by `RMDIR_BATCH_MAX`).
pub fn safe_rmtree_with_batch_max(root: &Path, batch_max: usize) -> Result<(), RmtreeError> {
    assert!(root.is_absolute(), "safe_rmtree requires an absolute path");
    assert!(batch_max > 0, "batch_max must be positive");
    assert!(
        batch_max <= RMDIR_BATCH_MAX,
        "batch_max exceeds RMDIR_BATCH_MAX"
    );

    let parent_path = root.parent().expect("root cannot be filesystem root");
    assert_ne!(parent_path, root, "refusing to rmtree root directory");
    let root_name_os = root
        .file_name()
        .expect("safe_rmtree requires a path with a final component");
    assert!(
        !is_forbidden_delete_path(root),
        "refusing to rmtree system path: {}",
        root.display()
    );

    if let Ok(resolved_parent) = fs::canonicalize(parent_path) {
        let resolved_target = resolved_parent.join(root_name_os);
        assert!(
            !is_forbidden_delete_path(&resolved_target),
            "refusing to rmtree resolved system path: {}",
            resolved_target.display()
        );
    }

    let top_parent_fd = open_dir_path(parent_path)?;
    let root_name = os_str_to_cstring(root_name_os)?;
    let root_fd = open_subdir_nofollow(top_parent_fd.as_raw_fd(), &root_name, root)?;
    let root_stat = fstat_fd(root_fd.as_raw_fd())?;
    let root_device_id = dev_to_u64(root_stat.st_dev);

    let mut stack: Vec<RmDirFrame> = Vec::with_capacity(WALK_DEPTH_MAX);
    stack.push(RmDirFrame {
        name: root_name,
        dir_fd: root_fd,
        depth: 0,
        chmod_done: false,
        pending_subdirs: Vec::new(),
    });

    let mut entries_processed_count: usize = 0;
    let mut iterations_count: usize = 0;
    let iterations_max = TREE_WALK_ENTRIES_MAX.saturating_mul(2).saturating_add(1024);

    while !stack.is_empty() {
        iterations_count = iterations_count.saturating_add(1);
        if iterations_count > iterations_max {
            return Err(RmtreeError::UnsafePath(format!(
                "safe_rmtree exceeded iteration limit on {}",
                root.display()
            )));
        }
        assert!(
            stack.len() <= WALK_DEPTH_MAX,
            "safe_rmtree stack exceeded WALK_DEPTH_MAX"
        );

        let next_subdir = {
            let frame = stack.last_mut().expect("stack is non-empty");
            if !frame.chmod_done {
                ensure_dir_fd_writable(frame.dir_fd.as_raw_fd())?;
                frame.chmod_done = true;
            }
            frame.pending_subdirs.pop()
        };

        if let Some(subdir_name) = next_subdir {
            let (parent_dir_fd, next_depth) = {
                let frame = stack.last().expect("stack is non-empty");
                (frame.dir_fd.as_raw_fd(), frame.depth + 1)
            };
            if next_depth >= WALK_DEPTH_MAX {
                return Err(RmtreeError::UnsafePath(format!(
                    "safe_rmtree exceeded max depth {WALK_DEPTH_MAX} inside {}",
                    root.display()
                )));
            }

            let subdir_fd = match open_subdir_nofollow(
                parent_dir_fd,
                &subdir_name,
                Path::new(OsStr::from_bytes(subdir_name.as_bytes())),
            ) {
                Ok(fd) => fd,
                Err(RmtreeError::Io(err)) if err.kind() == io::ErrorKind::NotFound => continue,
                Err(err) => return Err(err),
            };
            let subdir_stat = fstat_fd(subdir_fd.as_raw_fd())?;
            if dev_to_u64(subdir_stat.st_dev) != root_device_id {
                return Err(RmtreeError::UnsafePath(format!(
                    "refusing to cross mountpoint during rmtree: {}",
                    subdir_name.to_string_lossy()
                )));
            }

            stack.push(RmDirFrame {
                name: subdir_name,
                dir_fd: subdir_fd,
                depth: next_depth,
                chmod_done: false,
                pending_subdirs: Vec::new(),
            });
            continue;
        }

        // Read one bounded batch with a duplicated DIR* stream and close it
        // BEFORE unlinking entries (safe on both ext4 and APFS).
        let current_fd = stack.last().expect("stack is non-empty").dir_fd.as_raw_fd();
        let (batch_files, batch_subdirs) = read_dir_batch(current_fd, root_device_id, batch_max)?;

        for file_name in &batch_files {
            entries_processed_count = entries_processed_count.saturating_add(1);
            if entries_processed_count > TREE_WALK_ENTRIES_MAX {
                return Err(RmtreeError::UnsafePath(format!(
                    "safe_rmtree exceeded {TREE_WALK_ENTRIES_MAX} entries limit on {}",
                    root.display()
                )));
            }
            unlinkat_entry(current_fd, file_name, 0)?;
        }

        if !batch_subdirs.is_empty() {
            let frame = stack.last_mut().expect("stack is non-empty");
            frame.pending_subdirs = batch_subdirs;
            continue;
        }

        if batch_files.is_empty() {
            let popped = stack.pop().expect("stack is non-empty");
            // Close the directory fd explicitly before rmdir on its parent.
            drop(popped.dir_fd);

            entries_processed_count = entries_processed_count.saturating_add(1);
            if entries_processed_count > TREE_WALK_ENTRIES_MAX {
                return Err(RmtreeError::UnsafePath(format!(
                    "safe_rmtree exceeded {TREE_WALK_ENTRIES_MAX} entries limit on {}",
                    root.display()
                )));
            }
            let parent_fd = match stack.last() {
                Some(parent_frame) => parent_frame.dir_fd.as_raw_fd(),
                None => top_parent_fd.as_raw_fd(),
            };
            rmdirat_entry(parent_fd, &popped.name)?;
        }
    }

    Ok(())
}

/// RAII wrapper around POSIX `DIR*` returned by `fdopendir`.
struct DirStream {
    ptr: *mut libc::DIR,
}

impl Drop for DirStream {
    fn drop(&mut self) {
        // SAFETY: `self.ptr` is a valid open `DIR*` owned exclusively by `self`.
        let rc = unsafe { libc::closedir(self.ptr) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            eprintln!("disk-cleanup: warning: closedir failed: {err}");
        }
    }
}

fn read_dir_batch(
    dir_fd: RawFd,
    root_device_id: u64,
    batch_max: usize,
) -> Result<(Vec<CString>, Vec<CString>), RmtreeError> {
    assert!(dir_fd >= 0);
    assert!(batch_max > 0);
    assert!(batch_max <= RMDIR_BATCH_MAX);

    // Duplicate the directory descriptor so `closedir` never closes `dir_fd`.
    // SAFETY: `dir_fd` is a valid open directory descriptor.
    let dup_fd = unsafe { libc::fcntl(dir_fd, libc::F_DUPFD_CLOEXEC, 0) };
    if dup_fd < 0 {
        return Err(RmtreeError::Io(io::Error::last_os_error()));
    }

    // Rewind the shared open file description before scanning again.
    // SAFETY: `dup_fd` is a valid directory fd; seeking to offset 0 rewinds directory stream.
    let seek_rc = unsafe { libc::lseek(dup_fd, 0, libc::SEEK_SET) };
    if seek_rc < 0 {
        let err = io::Error::last_os_error();
        // SAFETY: `dup_fd` is owned and not yet transferred to `fdopendir`.
        unsafe {
            libc::close(dup_fd);
        }
        return Err(RmtreeError::Io(err));
    }

    // SAFETY: `dup_fd` transfers ownership into `fdopendir` on success.
    let raw_dir = unsafe { libc::fdopendir(dup_fd) };
    if raw_dir.is_null() {
        let err = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so we still own `dup_fd`.
        unsafe {
            libc::close(dup_fd);
        }
        return Err(RmtreeError::Io(err));
    }
    let dir_stream = DirStream { ptr: raw_dir };

    let mut batch_files = Vec::new();
    let mut batch_subdirs = Vec::new();

    // Bounded loop: handles `.`, `..`, and concurrent ENOENT races while capping work.
    for _ in 0..TREE_WALK_ENTRIES_MAX {
        if batch_files.len().saturating_add(batch_subdirs.len()) >= batch_max {
            break;
        }
        clear_errno();
        // SAFETY: `dir_stream.ptr` is an open `DIR*`.
        let entry_ptr = unsafe { libc::readdir(dir_stream.ptr) };
        if entry_ptr.is_null() {
            let err = io::Error::last_os_error();
            if err.raw_os_error().unwrap_or(0) != 0 {
                return Err(RmtreeError::Io(err));
            }
            break;
        }

        // SAFETY: `entry_ptr` is non-null and `d_name` is NUL-terminated.
        let name_cstr = unsafe { CStr::from_ptr((*entry_ptr).d_name.as_ptr()) };
        let name_bytes = name_cstr.to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }

        let child_stat = match fstatat_nofollow(dir_fd, name_cstr) {
            Ok(stat_buf) => stat_buf,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(RmtreeError::Io(err)),
        };

        if (child_stat.st_mode & libc::S_IFMT) == libc::S_IFDIR {
            if dev_to_u64(child_stat.st_dev) != root_device_id {
                return Err(RmtreeError::UnsafePath(format!(
                    "refusing to cross mountpoint during rmtree: {}",
                    name_cstr.to_string_lossy()
                )));
            }
            batch_subdirs.push(name_cstr.to_owned());
        } else {
            batch_files.push(name_cstr.to_owned());
        }
    }

    drop(dir_stream);
    assert!(batch_files.len().saturating_add(batch_subdirs.len()) <= batch_max);
    Ok((batch_files, batch_subdirs))
}

fn open_dir_path(path: &Path) -> io::Result<OwnedFd> {
    assert!(path.is_absolute());
    let c_path = path_to_cstring(path)?;
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: `c_path` is NUL-terminated and `O_CREAT` is not set.
    let raw_fd = unsafe { libc::open(c_path.as_ptr(), flags) };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw_fd` is a freshly opened owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}

fn open_subdir_nofollow(
    parent_fd: RawFd,
    name: &CStr,
    display_path: &Path,
) -> Result<OwnedFd, RmtreeError> {
    assert!(parent_fd >= 0);
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: `parent_fd` is valid and `name` is NUL-terminated.
    let raw_fd = unsafe { libc::openat(parent_fd, name.as_ptr(), flags) };
    if raw_fd < 0 {
        let err = io::Error::last_os_error();
        let raw_errno = err.raw_os_error();
        if raw_errno == Some(libc::ELOOP)
            || raw_errno == Some(libc::ENOTDIR)
            || raw_errno == Some(libc::EMLINK)
        {
            return Err(RmtreeError::UnsafePath(format!(
                "refusing to rmtree non-directory or symlink: {}",
                display_path.display()
            )));
        }
        return Err(RmtreeError::Io(err));
    }
    // SAFETY: `raw_fd` is a freshly opened owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw_fd) })
}

fn ensure_dir_fd_writable(dir_fd: RawFd) -> io::Result<()> {
    assert!(dir_fd >= 0, "dir_fd must be non-negative");
    let dir_stat = fstat_fd(dir_fd)?;
    assert_eq!(
        dir_stat.st_mode & libc::S_IFMT,
        libc::S_IFDIR,
        "dir_fd must refer to a directory"
    );
    let required_bits = libc::S_IRUSR | libc::S_IWUSR | libc::S_IXUSR;
    if (dir_stat.st_mode & required_bits) != required_bits {
        // SAFETY: `dir_fd` is an open directory descriptor verified by `fstat`.
        let rc = unsafe { libc::fchmod(dir_fd, dir_stat.st_mode | required_bits) };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn fstat_fd(fd: RawFd) -> io::Result<libc::stat> {
    assert!(fd >= 0);
    let mut stat_buf = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `fd` is open and `stat_buf` is a valid out-pointer.
    let rc = unsafe { libc::fstat(fd, stat_buf.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstat` succeeded, initializing `stat_buf`.
    Ok(unsafe { stat_buf.assume_init() })
}

fn fstatat_nofollow(dir_fd: RawFd, name: &CStr) -> io::Result<libc::stat> {
    assert!(dir_fd >= 0);
    let mut stat_buf = MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `dir_fd` is open, `name` is NUL-terminated, and `stat_buf` is valid.
    let rc = unsafe {
        libc::fstatat(
            dir_fd,
            name.as_ptr(),
            stat_buf.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fstatat` succeeded, initializing `stat_buf`.
    Ok(unsafe { stat_buf.assume_init() })
}

fn unlinkat_entry(dir_fd: RawFd, name: &CStr, flags: libc::c_int) -> io::Result<()> {
    assert!(dir_fd >= 0);
    // SAFETY: `dir_fd` is open and `name` is NUL-terminated.
    let rc = unsafe { libc::unlinkat(dir_fd, name.as_ptr(), flags) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(err);
    }
    Ok(())
}

fn rmdirat_entry(dir_fd: RawFd, name: &CStr) -> io::Result<()> {
    assert!(dir_fd >= 0);
    // SAFETY: `dir_fd` is open and `name` is NUL-terminated.
    let rc = unsafe { libc::unlinkat(dir_fd, name.as_ptr(), libc::AT_REMOVEDIR) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn path_to_cstring(path: &Path) -> io::Result<CString> {
    os_str_to_cstring(path.as_os_str())
}

fn os_str_to_cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains interior NUL byte",
        )
    })
}

#[cfg(target_os = "macos")]
fn dev_to_u64(dev: libc::dev_t) -> u64 {
    u64::from(dev.cast_unsigned())
}

#[cfg(not(target_os = "macos"))]
#[allow(clippy::useless_conversion)]
fn dev_to_u64(dev: libc::dev_t) -> u64 {
    u64::from(dev)
}

#[cfg(target_os = "macos")]
fn clear_errno() {
    // SAFETY: `__error()` returns a valid thread-local errno pointer on macOS.
    unsafe {
        *libc::__error() = 0;
    }
}

#[cfg(not(target_os = "macos"))]
fn clear_errno() {
    // SAFETY: `__errno_location()` returns a valid thread-local errno pointer on Linux.
    unsafe {
        *libc::__errno_location() = 0;
    }
}
