//! Validated domain value types parsed once at process boundaries.
//!
//! Every type in this module enforces its invariant at construction time so
//! downstream code can rely on the type instead of repeating runtime assertions.

use std::ffi::{CStr, CString, OsStr};
use std::fmt;
use std::io;
use std::num::NonZeroU64;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::constants::{
    AGE_DAYS_MAX, AGE_DURATION_NS_MAX, AGE_HOURS_MAX, ERROR_MESSAGE_BYTES_MAX, ERRORS_MAX,
    FORBIDDEN_DELETE_PATHS, NANOS_PER_SECOND_U128, NOW_NS_MAX, PATH_COMPONENTS_MAX,
    SECONDS_PER_DAY, SECONDS_PER_DAY_U128, SECONDS_PER_HOUR, SECONDS_PER_HOUR_U128,
};

/// Whether a cleanup run mutates the filesystem or only estimates and logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupMode {
    DryRun,
    Apply,
}

impl CleanupMode {
    pub const fn as_ipc_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry_run",
            Self::Apply => "apply",
        }
    }

    pub fn from_ipc_str(raw: &str) -> Result<Self, String> {
        match raw {
            "dry_run" => Ok(Self::DryRun),
            "apply" => Ok(Self::Apply),
            other => Err(format!("invalid user phase mode: {other:?}")),
        }
    }

    pub const fn is_dry_run(self) -> bool {
        matches!(self, Self::DryRun)
    }
}

/// Validated non-empty POSIX username with no interior NULs or path separators.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct UserName(String);

impl UserName {
    pub fn try_new(raw: String) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("user name must be non-empty".to_string());
        }
        if raw.contains('\0') {
            return Err("user name must not contain NUL bytes".to_string());
        }
        if raw.contains('/') || raw == "." || raw == ".." {
            return Err(format!("invalid user name: {raw:?}"));
        }
        Ok(Self(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_cstring(&self) -> CString {
        CString::new(self.0.as_bytes()).expect("UserName contains no interior NUL bytes")
    }
}

impl fmt::Display for UserName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for UserName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl AsRef<OsStr> for UserName {
    fn as_ref(&self) -> &OsStr {
        OsStr::new(&self.0)
    }
}

/// Non-empty single POSIX directory entry name (no `/`, no interior NUL, not `.` or `..`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntryName(CString);

impl EntryName {
    pub fn try_from_bytes(bytes: &[u8]) -> io::Result<Self> {
        if bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory entry name must be non-empty",
            ));
        }
        if bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory entry name must be a single path component",
            ));
        }
        let cstr = CString::new(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory entry name contains interior NUL byte",
            )
        })?;
        Ok(Self(cstr))
    }

    pub fn try_from_os_str(name: &OsStr) -> io::Result<Self> {
        Self::try_from_bytes(name.as_bytes())
    }

    pub fn try_from_cstr(name: &CStr) -> io::Result<Self> {
        Self::try_from_bytes(name.to_bytes())
    }

    pub fn as_cstr(&self) -> &CStr {
        &self.0
    }

    pub fn as_os_str(&self) -> &OsStr {
        OsStr::from_bytes(self.0.as_bytes())
    }

    pub fn to_str(&self) -> Option<&str> {
        std::str::from_utf8(self.0.as_bytes()).ok()
    }
}

impl fmt::Display for EntryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0.to_string_lossy())
    }
}

/// Lexically normalized, non-empty absolute POSIX path containing no interior NULs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AbsPath(PathBuf);

impl AbsPath {
    /// Normalize any relative or absolute path without resolving symlinks.
    pub fn normalize(path: &Path) -> io::Result<Self> {
        let raw_bytes = path.as_os_str().as_bytes();
        if raw_bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path must not be empty",
            ));
        }
        if raw_bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains interior NUL byte",
            ));
        }
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        Self::normalize_absolute_components(&joined)
    }

    /// Validate and lexically normalize a path that must already be absolute.
    pub fn from_absolute(path: &Path) -> io::Result<Self> {
        let raw_bytes = path.as_os_str().as_bytes();
        if raw_bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path must not be empty",
            ));
        }
        if raw_bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path contains interior NUL byte",
            ));
        }
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("path must be absolute: {}", path.display()),
            ));
        }
        Self::normalize_absolute_components(path)
    }

    fn normalize_absolute_components(path: &Path) -> io::Result<Self> {
        let mut normalized = PathBuf::new();
        let mut components_count: usize = 0;
        for component in path.components() {
            components_count = components_count.saturating_add(1);
            if components_count > PATH_COMPONENTS_MAX {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("path exceeds maximum {PATH_COMPONENTS_MAX} components"),
                ));
            }
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
        if !normalized.is_absolute() || normalized.as_os_str().as_bytes().contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "normalized path is not a valid absolute path",
            ));
        }
        Ok(Self(normalized))
    }

    pub fn root() -> Self {
        Self(PathBuf::from("/"))
    }

    pub fn is_root(&self) -> bool {
        self.0 == Path::new("/")
    }

    pub fn is_forbidden_delete_path(&self) -> bool {
        FORBIDDEN_DELETE_PATHS
            .iter()
            .any(|forbidden| self.0 == Path::new(forbidden))
    }

    pub fn join_entry(&self, name: &EntryName) -> io::Result<NonRootAbsPath> {
        let joined = self.0.join(name.as_os_str());
        let abs = Self::from_absolute(&joined)?;
        NonRootAbsPath::try_from_abs(abs)
            .map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))
    }

    pub fn join_relative(&self, rel: &Path) -> io::Result<Self> {
        if rel.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "join_relative requires a relative suffix",
            ));
        }
        Self::from_absolute(&self.0.join(rel))
    }

    pub fn to_cstring(&self) -> CString {
        CString::new(self.0.as_os_str().as_bytes()).expect("AbsPath contains no interior NUL bytes")
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn as_os_str(&self) -> &OsStr {
        self.0.as_os_str()
    }

    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }
}

impl AsRef<Path> for AbsPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

/// Lexically normalized absolute path guaranteed not to be `/`, so both
/// `parent()` and `file_name()` always exist.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NonRootAbsPath(AbsPath);

impl NonRootAbsPath {
    pub fn try_from_abs(path: AbsPath) -> Result<Self, String> {
        if path.is_root() {
            return Err("path cannot be /".to_string());
        }
        if path.as_path().parent().is_none() || path.as_path().file_name().is_none() {
            return Err(format!(
                "path has no parent or final component: {}",
                path.display()
            ));
        }
        Ok(Self(path))
    }

    pub fn normalize(path: &Path) -> io::Result<Self> {
        let abs = AbsPath::normalize(path)?;
        Self::try_from_abs(abs).map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))
    }

    pub fn from_absolute(path: &Path) -> io::Result<Self> {
        let abs = AbsPath::from_absolute(path)?;
        Self::try_from_abs(abs).map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))
    }

    pub fn parent(&self) -> AbsPath {
        let parent = self
            .0
            .as_path()
            .parent()
            .expect("NonRootAbsPath always has a parent");
        AbsPath(parent.to_path_buf())
    }

    pub fn file_name(&self) -> EntryName {
        let name = self
            .0
            .as_path()
            .file_name()
            .expect("NonRootAbsPath always has a file_name");
        EntryName::try_from_os_str(name)
            .expect("normalized NonRootAbsPath file_name is a valid EntryName")
    }

    pub fn join_entry(&self, name: &EntryName) -> io::Result<NonRootAbsPath> {
        self.0.join_entry(name)
    }

    pub fn join_relative(&self, rel: &Path) -> io::Result<NonRootAbsPath> {
        let abs = self.0.join_relative(rel)?;
        Self::try_from_abs(abs).map_err(|msg| io::Error::new(io::ErrorKind::InvalidInput, msg))
    }

    pub fn as_abs(&self) -> &AbsPath {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        self.0.as_path()
    }

    pub fn as_os_str(&self) -> &OsStr {
        self.0.as_os_str()
    }

    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }
}

impl AsRef<Path> for NonRootAbsPath {
    fn as_ref(&self) -> &Path {
        self.0.as_path()
    }
}

/// Absolute path verified not to be any protected system root in `FORBIDDEN_DELETE_PATHS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafeProjectsDir(NonRootAbsPath);

impl SafeProjectsDir {
    pub fn try_from_abs(path: AbsPath) -> Result<Self, String> {
        if path.is_forbidden_delete_path() {
            return Err(format!("cannot be system root {}", path.display()));
        }
        let non_root = NonRootAbsPath::try_from_abs(path)?;
        Ok(Self(non_root))
    }

    pub fn as_abs(&self) -> &AbsPath {
        self.0.as_abs()
    }

    pub fn as_path(&self) -> &Path {
        self.0.as_path()
    }

    pub fn as_os_str(&self) -> &OsStr {
        self.0.as_os_str()
    }

    pub fn display(&self) -> std::path::Display<'_> {
        self.0.display()
    }
}

impl AsRef<Path> for SafeProjectsDir {
    fn as_ref(&self) -> &Path {
        self.0.as_path()
    }
}

/// Disk-usage trigger threshold in `[1, 100]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ThresholdPercent(u8);

impl ThresholdPercent {
    pub const DEFAULT: Self = Self(85);

    pub fn try_from_i64(value: i64) -> Result<Self, String> {
        u8::try_from(value)
            .ok()
            .filter(|v| (1..=100).contains(v))
            .map(Self)
            .ok_or_else(|| "--threshold must be between 1 and 100".to_string())
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

impl fmt::Display for ThresholdPercent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Observed filesystem usage percentage in `[0, 100]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DiskPercent(u8);

impl DiskPercent {
    pub const FULL: Self = Self(100);

    pub fn try_new(value: u8) -> Result<Self, &'static str> {
        if value <= 100 {
            Ok(Self(value))
        } else {
            Err("disk percentage must be in 0..=100")
        }
    }

    pub const fn is_below(self, threshold: ThresholdPercent) -> bool {
        self.0 < threshold.get()
    }
}

impl fmt::Display for DiskPercent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Exact non-negative age duration in nanoseconds, bounded by `AGE_DAYS_MAX` (10 years).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AgeDurationNs(u128);

impl AgeDurationNs {
    pub const DEFAULT_RESULT: Self = Self(7 * SECONDS_PER_DAY_U128 * NANOS_PER_SECOND_U128);
    pub const DEFAULT_TARGET: Self = Self(24 * SECONDS_PER_HOUR_U128 * NANOS_PER_SECOND_U128);
    pub const DEFAULT_TMP: Self = Self(24 * SECONDS_PER_HOUR_U128 * NANOS_PER_SECOND_U128);

    pub fn try_from_ns(ns: u128) -> Result<Self, String> {
        if ns <= AGE_DURATION_NS_MAX {
            Ok(Self(ns))
        } else {
            Err(format!("age duration exceeds maximum {AGE_DAYS_MAX} days"))
        }
    }

    pub fn from_days_f64(flag: &str, days: f64) -> Result<Self, String> {
        if !days.is_finite() || !(0.0..=f64::from(AGE_DAYS_MAX)).contains(&days) {
            return Err(format!(
                "{flag} must be a finite number in [0, {AGE_DAYS_MAX}]"
            ));
        }
        let ns = finite_non_negative_seconds_to_ns(days * SECONDS_PER_DAY);
        Self::try_from_ns(ns.min(AGE_DURATION_NS_MAX))
    }

    pub fn from_hours_f64(flag: &str, hours: f64) -> Result<Self, String> {
        if !hours.is_finite() || !(0.0..=f64::from(AGE_HOURS_MAX)).contains(&hours) {
            return Err(format!(
                "{flag} must be a finite number in [0, {AGE_HOURS_MAX}]"
            ));
        }
        let ns = finite_non_negative_seconds_to_ns(hours * SECONDS_PER_HOUR);
        Self::try_from_ns(ns.min(AGE_DURATION_NS_MAX))
    }

    pub const fn as_ns(self) -> u128 {
        self.0
    }

    pub fn cutoff_before(self, now: TimestampNs) -> i128 {
        let delta = i128::try_from(self.0).expect("AGE_DURATION_NS_MAX fits i128");
        now.as_i128() - delta
    }
}

/// Convert a validated finite non-negative number of seconds (<= 10 years) to nanoseconds.
pub fn finite_non_negative_seconds_to_ns(seconds: f64) -> u128 {
    assert!(seconds.is_finite(), "seconds must be finite");
    assert!(seconds >= 0.0, "seconds must be non-negative");
    assert!(seconds <= 1.0e13, "seconds exceeds safe conversion bound");

    let whole_seconds = seconds.trunc();
    let frac_seconds = seconds - whole_seconds;
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let whole_u128 = whole_seconds as u128;
    #[allow(
        clippy::as_conversions,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let frac_ns = (frac_seconds * 1_000_000_000.0).round() as u128;

    whole_u128 * NANOS_PER_SECOND_U128 + frac_ns
}

/// Nix generation retention window in whole days, bounded by `[0, AGE_DAYS_MAX]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct NixGcAgeDays(u32);

impl NixGcAgeDays {
    pub const DEFAULT: Self = Self(14);

    pub fn try_from_i64(value: i64) -> Result<Self, String> {
        u32::try_from(value)
            .ok()
            .filter(|v| *v <= AGE_DAYS_MAX)
            .map(Self)
            .ok_or_else(|| format!("--nix-gc-age-days must be an integer in [0, {AGE_DAYS_MAX}]"))
    }
}

impl fmt::Display for NixGcAgeDays {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Positive Unix timestamp in nanoseconds, bounded by `[1, NOW_NS_MAX]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimestampNs(i128);

impl TimestampNs {
    pub fn try_new(ns: i128) -> Result<Self, String> {
        if (1..=NOW_NS_MAX).contains(&ns) {
            Ok(Self(ns))
        } else {
            Err(format!("timestamp must be an integer in [1, {NOW_NS_MAX}]"))
        }
    }

    pub fn now() -> Self {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock must be after UNIX_EPOCH");
        let nanos = i128::try_from(duration.as_nanos()).expect("system time fits i128");
        Self::try_new(nanos).expect("system clock must be in [1, NOW_NS_MAX]")
    }

    pub const fn as_i128(self) -> i128 {
        self.0
    }

    pub fn age_since(self, mtime_ns: i128) -> u128 {
        let diff = self.0.saturating_sub(mtime_ns);
        if diff <= 0 {
            0
        } else {
            u128::try_from(diff).expect("positive i128 fits u128")
        }
    }
}

impl fmt::Display for TimestampNs {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Non-empty UTF-8 error message guaranteed to fit within `ERROR_MESSAGE_BYTES_MAX`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorMessage(String);

impl ErrorMessage {
    pub fn try_new(raw: String) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("error message must be non-empty".to_string());
        }
        if raw.len() > ERROR_MESSAGE_BYTES_MAX {
            return Err("error message exceeded ERROR_MESSAGE_BYTES_MAX".to_string());
        }
        Ok(Self(raw))
    }

    pub fn from_truncated(raw: String) -> Result<Self, String> {
        if raw.is_empty() {
            return Err("error message must be non-empty".to_string());
        }
        Ok(Self(truncate_utf8_bytes(raw, ERROR_MESSAGE_BYTES_MAX)))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ErrorMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for ErrorMessage {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Bounded collection of non-empty error messages (`<= ERRORS_MAX`) with an explicit
/// overflow counter so truncated phase errors are never silently lost.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BoundedErrors {
    items: Vec<ErrorMessage>,
    dropped_count: u64,
}

impl BoundedErrors {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_parts(items: Vec<ErrorMessage>, dropped_count: u64) -> Result<Self, String> {
        if items.len() > ERRORS_MAX {
            return Err("errors list exceeded ERRORS_MAX".to_string());
        }
        if dropped_count > 0 && items.len() < ERRORS_MAX {
            return Err("errors_dropped_count must be 0 when errors list is not full".to_string());
        }
        Ok(Self {
            items,
            dropped_count,
        })
    }

    pub fn push(&mut self, message: ErrorMessage) {
        if self.items.len() < ERRORS_MAX {
            self.items.push(message);
        } else {
            self.dropped_count = self.dropped_count.saturating_add(1);
        }
    }

    pub fn push_truncated(&mut self, raw: impl Into<String>) {
        let text = raw.into();
        let message =
            ErrorMessage::from_truncated(text).expect("internal error message must be non-empty");
        self.push(message);
    }

    pub fn extend_from(&mut self, other: &Self) {
        for item in &other.items {
            self.push(item.clone());
        }
        if other.dropped_count > 0 {
            self.dropped_count = self.dropped_count.saturating_add(other.dropped_count);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.dropped_count == 0
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub const fn dropped_count(&self) -> u64 {
        self.dropped_count
    }

    pub fn iter(&self) -> std::slice::Iter<'_, ErrorMessage> {
        self.items.iter()
    }
}

/// Count and reclaimed bytes for a cleanup category, making `count == 0 && freed_bytes > 0`
/// unrepresentable by construction.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum RemovedBatch {
    #[default]
    None,
    Some {
        count: NonZeroU64,
        freed_bytes: u64,
    },
}

impl RemovedBatch {
    pub fn from_parts(count: u64, freed_bytes: u64) -> Result<Self, &'static str> {
        match NonZeroU64::new(count) {
            None if freed_bytes == 0 => Ok(Self::None),
            None => Err("freed_bytes must be 0 when removed count is 0"),
            Some(nonzero) => Ok(Self::Some {
                count: nonzero,
                freed_bytes,
            }),
        }
    }

    pub fn record(&mut self, freed_bytes: u64) {
        *self = match *self {
            Self::None => Self::Some {
                count: NonZeroU64::MIN,
                freed_bytes,
            },
            Self::Some {
                count,
                freed_bytes: existing,
            } => Self::Some {
                count: count.saturating_add(1),
                freed_bytes: existing.saturating_add(freed_bytes),
            },
        };
    }

    pub const fn count(self) -> u64 {
        match self {
            Self::None => 0,
            Self::Some { count, .. } => count.get(),
        }
    }

    pub const fn freed_bytes(self) -> u64 {
        match self {
            Self::None => 0,
            Self::Some { freed_bytes, .. } => freed_bytes,
        }
    }
}

fn truncate_utf8_bytes(mut text: String, max_bytes: usize) -> String {
    assert!(max_bytes >= 3, "max_bytes must fit ellipsis");
    if text.len() <= max_bytes {
        return text;
    }
    let mut boundary = max_bytes - 3;
    // A UTF-8 scalar value is at most 4 bytes, so at most 3 decrements reach a boundary.
    for _ in 0..4 {
        if text.is_char_boundary(boundary) {
            break;
        }
        boundary = boundary.saturating_sub(1);
    }
    assert!(text.is_char_boundary(boundary));
    text.truncate(boundary);
    text.push_str("...");
    assert!(text.len() <= max_bytes);
    text
}
