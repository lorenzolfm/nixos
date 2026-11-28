//! Compile-time bounds, signatures, timeouts, and safety constants.

/// Cache Directory Tagging Specification signature (https://bford.info/cachedir/).
pub const CACHEDIR_TAG_SIGNATURE_BYTES: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

pub const SKIP_WALK_DIRS: &[&str] = &["node_modules", "__pycache__", "Pods", "DerivedData"];
pub const TARGET_SKIP_MTIME_CHILDREN: &[&str] = &["deps", "examples"];
pub const NIX_TMP_PREFIXES: &[&str] = &[
    "nix-shell.",
    "nix-shell-",
    "nix-develop-",
    "nix-build-",
    "nixos-rebuild.",
];

// Time unit conversion constants.
pub const NANOS_PER_SECOND: i128 = 1_000_000_000;
pub const NANOS_PER_SECOND_U128: u128 = 1_000_000_000;
pub const SECONDS_PER_HOUR: f64 = 3600.0;
pub const SECONDS_PER_DAY: f64 = 86_400.0;
pub const SECONDS_PER_HOUR_U128: u128 = 3600;
pub const SECONDS_PER_DAY_U128: u128 = 86_400;
pub const NOW_NS_MAX: i128 = 10_000_000_000_000 * NANOS_PER_SECOND;

// Fixed upper bounds on walks, collections, I/O, and wall-clock time.
pub const NOTIFY_FREED_BYTES_MIN: u64 = 1024 * 1024; // 1 MiB
pub const AGE_DAYS_MAX: u32 = 3650; // 10 years
pub const AGE_HOURS_MAX: u32 = AGE_DAYS_MAX * 24;
pub const AGE_DURATION_NS_MAX: u128 = 3650 * SECONDS_PER_DAY_U128 * NANOS_PER_SECOND_U128;
pub const CLI_ARGS_MAX: usize = 128;
pub const PATH_ENTRIES_MAX: usize = 256;
pub const PATH_COMPONENTS_MAX: usize = 256;
pub const ENV_VARS_MAX: usize = 4096;
pub const WALK_DEPTH_MAX: usize = 64;
pub const TARGET_MTIME_DEPTH_MAX: usize = 3;
pub const PROJECTS_WALK_ENTRIES_MAX: usize = 500_000;
pub const TREE_WALK_ENTRIES_MAX: usize = 2_000_000;
pub const TMP_DIR_ENTRIES_MAX: usize = 100_000;
pub const RMDIR_BATCH_MAX: usize = 4096;
pub const INODES_TRACKED_MAX: usize = 1_000_000;
pub const CANDIDATES_MAX: usize = 10_000;
pub const USER_PROFILES_MAX: usize = 8;
pub const WATCH_PATHS_MAX: usize = 16;
pub const ERRORS_MAX: usize = 256;
pub const ERROR_MESSAGE_BYTES_MAX: usize = 512;
pub const NIX_ENV_LOG_LINES_MAX: usize = 200;
pub const DEAD_PATHS_COUNT_MAX: u64 = 10_000_000;
pub const SUBPROCESS_OUTPUT_BYTES_MAX: usize = 1_048_576; // 1 MiB cap for buffered IPC payloads
pub const SUBPROCESS_STDERR_BYTES_MAX: usize = 65_536; // 64 KiB diagnostic tail cap
pub const SUBPROCESS_LINE_BYTES_MAX: usize = 65_536;
pub const SUBPROCESS_READ_ITERATIONS_MAX: usize = 10_000_000;
pub const WAIT_ITERATIONS_MAX: usize = 400_000;
pub const PIPE_READ_BYTES: usize = 8192;
pub const USER_GROUPS_MAX: usize = 1024;
pub const PASSWD_BUFFER_BYTES_INITIAL: usize = 16 * 1024;
pub const PASSWD_BUFFER_BYTES_MAX: usize = 1024 * 1024;

// Wall-clock deadlines (seconds). TOTAL_RUN_TIMEOUT_SECONDS fits inside
// Linux's 1h systemd TimeoutStartSec and the hourly schedule on macOS.
pub const TOTAL_RUN_TIMEOUT_SECONDS: u64 = 3300;
pub const USER_PHASE_TIMEOUT_SECONDS: u64 = 1800;
pub const NIX_ENV_TIMEOUT_SECONDS: u64 = 600;
pub const NIX_GC_TIMEOUT_SECONDS: u64 = 3000;
pub const NOTIFY_TIMEOUT_SECONDS: u64 = 15;

pub const FORBIDDEN_DELETE_PATHS: &[&str] = &[
    "/",
    "/tmp",
    "/private/tmp",
    "/var/tmp",
    "/private/var/tmp",
    "/nix",
    "/home",
    "/Users",
];

// Compile-time invariants on constant relationships.
const _: () = assert!(NANOS_PER_SECOND == 1_000_000_000);
const _: () = assert!(NOW_NS_MAX > 0);
const _: () = assert!(TARGET_MTIME_DEPTH_MAX > 0);
const _: () = assert!(WALK_DEPTH_MAX >= TARGET_MTIME_DEPTH_MAX);
const _: () = assert!(CLI_ARGS_MAX > 0);
const _: () = assert!(PATH_ENTRIES_MAX > 0);
const _: () = assert!(PATH_COMPONENTS_MAX > 0);
const _: () = assert!(ENV_VARS_MAX > 0);
const _: () = assert!(PROJECTS_WALK_ENTRIES_MAX > 0);
const _: () = assert!(TREE_WALK_ENTRIES_MAX > 0);
const _: () = assert!(TMP_DIR_ENTRIES_MAX > 0);
const _: () = assert!(RMDIR_BATCH_MAX > 0);
const _: () = assert!(INODES_TRACKED_MAX > 0);
const _: () = assert!(CANDIDATES_MAX > 0);
const _: () = assert!(USER_PROFILES_MAX >= 2);
const _: () = assert!(WATCH_PATHS_MAX > 0);
const _: () = assert!(ERRORS_MAX > 0);
const _: () = assert!(ERROR_MESSAGE_BYTES_MAX >= 16);
// Worst-case JSON escaping expands each byte up to 6 bytes (`\u00xx`) plus quotes/comma.
// Prove at compile time that any valid `UserPhaseStats` fits inside `SUBPROCESS_OUTPUT_BYTES_MAX`.
const _: () =
    assert!(ERRORS_MAX * (ERROR_MESSAGE_BYTES_MAX * 6 + 3) + 512 <= SUBPROCESS_OUTPUT_BYTES_MAX);
const _: () = assert!(AGE_DAYS_MAX > 0);
const _: () = assert!(AGE_HOURS_MAX == AGE_DAYS_MAX * 24);
const _: () = assert!(NOTIFY_FREED_BYTES_MIN > 0);
const _: () = assert!(NIX_GC_TIMEOUT_SECONDS < TOTAL_RUN_TIMEOUT_SECONDS);
const _: () = assert!(USER_PHASE_TIMEOUT_SECONDS < TOTAL_RUN_TIMEOUT_SECONDS);
const _: () = assert!(NIX_ENV_TIMEOUT_SECONDS < USER_PHASE_TIMEOUT_SECONDS);
const _: () = assert!(CACHEDIR_TAG_SIGNATURE_BYTES.len() == 43);
const _: () = assert!(PASSWD_BUFFER_BYTES_INITIAL <= PASSWD_BUFFER_BYTES_MAX);
const _: () =
    assert!(PASSWD_BUFFER_BYTES_INITIAL.saturating_mul(1 << 7) >= PASSWD_BUFFER_BYTES_MAX);
const _: () = assert!(USER_GROUPS_MAX > 0);
const _: () = assert!(WAIT_ITERATIONS_MAX > 0);
const _: () = assert!(PIPE_READ_BYTES > 0);
