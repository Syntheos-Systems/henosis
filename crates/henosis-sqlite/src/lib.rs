#![deny(missing_docs)]
//! Race-resistant SQLite file opening for Henosis services.
//!
//! Configured database names are always treated as literal filesystem paths. Mutable databases
//! are opened without SQLite URI interpretation. Immutable databases use an internally generated,
//! percent-encoded URI so callers cannot inject a scheme or query parameter.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::Arc;

#[cfg(windows)]
use std::collections::HashMap;
#[cfg(windows)]
use std::sync::{Mutex, OnceLock, Weak};

use cap_primitives::ambient_authority;
use cap_primitives::fs::{create_dir, open_ambient_dir, open_dir_nofollow, DirOptions};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior};

/// A failure while validating, preparing, or opening a disk database.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The configured value is not a safe literal disk path.
    #[error("invalid database path {}: {reason}", path.display())]
    InvalidPath {
        /// The rejected path.
        path: PathBuf,
        /// A stable explanation of the rejected invariant.
        reason: &'static str,
    },
    /// A filesystem operation failed while binding the database path.
    #[error("{operation} database path {}: {source}", path.display())]
    Filesystem {
        /// The failed operation.
        operation: &'static str,
        /// The path associated with the failed operation.
        path: PathBuf,
        /// The operating-system error.
        #[source]
        source: std::io::Error,
    },
    /// SQLite rejected the already validated database file.
    #[error("open SQLite database {}: {source}", path.display())]
    Sqlite {
        /// The literal path passed to SQLite.
        path: PathBuf,
        /// The SQLite open error.
        #[source]
        source: rusqlite::Error,
    },
}

/// Retained filesystem handles that keep approved path objects live with the connection.
#[derive(Clone)]
struct DatabasePathGuard {
    /// Approved parent and database handles remain live until every sharing store is dropped.
    _handles: Arc<[File]>,
}

/// A SQLite connection that owns every guard required by its backing storage.
pub struct OpenedDatabase {
    /// The opened SQLite connection is dropped before the guard declared after it.
    connection: Connection,
    /// Disk handles are present for protected files and absent for in-memory databases.
    _guard: Option<DatabasePathGuard>,
}

/// Exposes the connection without allowing its path guards to be split off.
impl OpenedDatabase {
    /// Create an in-memory connection with the same ownership shape as a disk connection.
    pub fn open_in_memory() -> Result<Self, rusqlite::Error> {
        Ok(Self {
            connection: Connection::open_in_memory()?,
            _guard: None,
        })
    }

    /// Borrow the underlying SQLite connection.
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    /// Begin a deferred transaction without exposing a replaceable mutable connection reference.
    pub fn transaction(&mut self) -> Result<Transaction<'_>, rusqlite::Error> {
        self.connection.transaction()
    }

    /// Begin a transaction with explicit behavior while retaining every path guard.
    pub fn transaction_with_behavior(
        &mut self,
        behavior: TransactionBehavior,
    ) -> Result<Transaction<'_>, rusqlite::Error> {
        self.connection.transaction_with_behavior(behavior)
    }
}

/// Makes guarded connections usable anywhere a shared SQLite connection is expected.
impl Deref for OpenedDatabase {
    /// The shared SQLite connection type exposed by immutable dereferencing.
    type Target = Connection;

    /// Borrow the SQLite connection without releasing its guard.
    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

/// Selects whether the opener may create protected storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenMode {
    /// Create missing state and open the database read-write.
    Mutable,
    /// Require existing state and prevent SQLite from creating sidecars.
    Immutable,
}

/// A database path whose complete syntax has been accepted and bound once.
struct ValidatedPath {
    /// The normalized absolute literal path supplied to filesystem operations.
    path: PathBuf,
    /// The directory containing the database leaf.
    parent: PathBuf,
}

/// Validates literal path syntax before any filesystem operation can occur.
impl ValidatedPath {
    /// Reject SQLite controls, traversal, ambiguous terminals, and unsafe platform prefixes.
    fn new(path: &Path) -> Result<Self, OpenError> {
        if path.as_os_str().is_empty() {
            return Err(invalid_path(path, "the path is empty"));
        }
        if path == Path::new(":memory:") {
            return Err(invalid_path(
                path,
                "the in-memory sentinel is not a disk database",
            ));
        }
        if has_embedded_nul(path) {
            return Err(invalid_path(path, "the path contains an embedded NUL"));
        }
        if has_sqlite_file_prefix(path) {
            return Err(invalid_path(path, "SQLite file URI syntax is not accepted"));
        }
        validate_platform_path_encoding(path)?;
        if has_directory_terminal(path) {
            return Err(invalid_path(
                path,
                "the path syntactically ends at a directory",
            ));
        }
        if path.file_name().is_none() {
            return Err(invalid_path(path, "the path has no database file name"));
        }
        if path.has_root() && !path.is_absolute() {
            return Err(invalid_path(
                path,
                "rooted paths without an absolute volume are not accepted",
            ));
        }

        for component in path.components() {
            match component {
                Component::ParentDir => {
                    return Err(invalid_path(path, "parent traversal is not accepted"));
                }
                Component::Prefix(prefix) => validate_prefix(path, prefix.kind())?,
                Component::Normal(name) => validate_normal_component(path, name)?,
                Component::RootDir | Component::CurDir => {}
            }
        }
        if path
            .components()
            .next()
            .is_some_and(|component| matches!(component, Component::Prefix(_)))
            && !path.is_absolute()
        {
            return Err(invalid_path(path, "drive-relative paths are not accepted"));
        }

        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|source| filesystem_error("bind current directory", path, source))?
                .join(path)
        };
        let bound_path = normalize_accepted_path(&absolute)?;
        let parent = bound_path
            .parent()
            .filter(|candidate| !candidate.as_os_str().is_empty())
            .ok_or_else(|| invalid_path(path, "the path has no database parent"))?
            .to_path_buf();
        Ok(Self {
            path: bound_path,
            parent,
        })
    }
}

/// Open one literal disk database and retain every handle that approves its path.
pub fn open_database(path: impl AsRef<Path>) -> Result<OpenedDatabase, OpenError> {
    let original = path.as_ref();
    let validated = ValidatedPath::new(original)?;
    let opened = open_validated_database(validated, OpenMode::Mutable)?;
    opened.ok_or_else(|| invalid_path(original, "mutable database storage disappeared"))
}

/// Open an existing disk database without modifying it or creating SQLite sidecars.
pub fn open_database_read_only(
    path: impl AsRef<Path>,
) -> Result<Option<OpenedDatabase>, OpenError> {
    let validated = ValidatedPath::new(path.as_ref())?;
    open_validated_database(validated, OpenMode::Immutable)
}

/// Create or validate one private service state directory without reclaiming insecure state.
pub fn ensure_private_directory(path: impl AsRef<Path>) -> Result<(), OpenError> {
    let path = path.as_ref();
    if path.as_os_str().is_empty() {
        return Err(invalid_path(path, "the private directory path is empty"));
    }
    if has_embedded_nul(path) {
        return Err(invalid_path(path, "the path contains an embedded NUL"));
    }
    if has_sqlite_file_prefix(path) {
        return Err(invalid_path(path, "SQLite file URI syntax is not accepted"));
    }

    let synthetic_leaf = path.join(".henosis-private-directory-probe");
    let validated = ValidatedPath::new(&synthetic_leaf)?;
    let handles = prepare_parent(&validated.parent, OpenMode::Mutable)?
        .ok_or_else(|| invalid_path(path, "the private directory disappeared"))?;
    let directory = handles
        .last()
        .ok_or_else(|| invalid_path(path, "the private directory was not retained"))?;
    validate_managed_directory(directory, &validated.parent)
}

/// Require exact current-user-only Unix mode for an installer-managed state directory.
#[cfg(unix)]
fn validate_managed_directory(directory: &File, path: &Path) -> Result<(), OpenError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = directory
        .metadata()
        .map_err(|source| filesystem_error("inspect private directory", path, source))?;
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid || metadata.mode() & 0o777 != 0o700 {
        return Err(invalid_path(
            path,
            "the private directory is not current-user-owned with mode 0700",
        ));
    }
    Ok(())
}

/// Windows parent preparation already enforces the exact owner and protected DACL.
#[cfg(windows)]
fn validate_managed_directory(_directory: &File, _path: &Path) -> Result<(), OpenError> {
    Ok(())
}

/// Other targets require the managed path to remain a directory.
#[cfg(not(any(unix, windows)))]
fn validate_managed_directory(directory: &File, path: &Path) -> Result<(), OpenError> {
    if !directory
        .metadata()
        .map_err(|source| filesystem_error("inspect private directory", path, source))?
        .is_dir()
    {
        return Err(invalid_path(
            path,
            "the private state path is not a directory",
        ));
    }
    Ok(())
}

/// Dispatch a validated path to its platform-specific approval boundary.
fn open_validated_database(
    validated: ValidatedPath,
    mode: OpenMode,
) -> Result<Option<OpenedDatabase>, OpenError> {
    #[cfg(windows)]
    {
        open_windows_database(validated, mode)
    }
    #[cfg(not(windows))]
    {
        open_standard_database(validated, mode)
    }
}

/// Open a validated database on platforms that do not need approval reuse.
#[cfg(not(windows))]
fn open_standard_database(
    validated: ValidatedPath,
    mode: OpenMode,
) -> Result<Option<OpenedDatabase>, OpenError> {
    let Some(mut handles) = prepare_parent(&validated.parent, mode)? else {
        return Ok(None);
    };
    let Some(leaf) = open_database_leaf(&validated.path, mode)? else {
        return Ok(None);
    };
    handles.push(leaf);
    let shared_handles: Arc<[File]> = handles.into();
    let connection = open_sqlite_connection(&validated, mode)?;
    Ok(Some(OpenedDatabase {
        connection,
        _guard: Some(DatabasePathGuard {
            _handles: shared_handles,
        }),
    }))
}

/// Open SQLite after the filesystem boundary has accepted and retained the path.
fn open_sqlite_connection(
    validated: &ValidatedPath,
    mode: OpenMode,
) -> Result<Connection, OpenError> {
    let (sqlite_path, flags) = match mode {
        OpenMode::Mutable => (validated.path.clone(), mutable_database_flags()),
        OpenMode::Immutable => (
            immutable_database_uri(&validated.path)?,
            immutable_database_flags(),
        ),
    };
    Connection::open_with_flags(&sqlite_path, flags).map_err(|source| OpenError::Sqlite {
        path: validated.path.clone(),
        source,
    })
}

/// Return mutable SQLite flags that reject linked leaves and URI reinterpretation.
fn mutable_database_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
}

/// Return immutable SQLite flags that cannot create journals, WAL, or shared memory.
fn immutable_database_flags() -> OpenFlags {
    OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
}

/// Traverse and optionally create the parent through retained no-follow directory handles.
fn prepare_parent(parent: &Path, mode: OpenMode) -> Result<Option<Vec<File>>, OpenError> {
    let (anchor, components) = split_parent(parent)?;
    let anchor_handle = open_ambient_dir(&anchor, ambient_authority())
        .map_err(|source| filesystem_error("open anchor", &anchor, source))?;
    let mut handles = vec![anchor_handle];
    if let Some(extra_guard) = validate_directory(
        handles
            .last()
            .ok_or_else(|| invalid_path(parent, "the path anchor was not retained"))?,
        &anchor,
        components.is_empty(),
        true,
        mode,
    )? {
        handles.push(extra_guard);
    }

    let mut display_path = anchor;
    for (index, name) in components.iter().enumerate() {
        let current = handles
            .last()
            .ok_or_else(|| invalid_path(parent, "directory traversal lost its current handle"))?;
        let relative = Path::new(name);
        let final_parent = index + 1 == components.len();
        let candidate_path = display_path.join(name);
        let next = match open_dir_nofollow(current, relative) {
            Ok(directory) => directory,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound && mode == OpenMode::Immutable =>
            {
                return Ok(None);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match create_parent_component(current, relative, &candidate_path, final_parent) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(source) => {
                        return Err(filesystem_error(
                            "create directory",
                            &candidate_path,
                            source,
                        ));
                    }
                }
                open_dir_nofollow(current, relative).map_err(|source| {
                    filesystem_error("open created directory", &candidate_path, source)
                })?
            }
            Err(source) => {
                return Err(filesystem_error(
                    "open directory without following links",
                    &candidate_path,
                    source,
                ));
            }
        };
        display_path.push(name);
        if let Some(extra_guard) =
            validate_directory(&next, &display_path, final_parent, false, mode)?
        {
            handles.push(extra_guard);
        }
        handles.push(next);
    }
    Ok(Some(handles))
}

/// Separate an absolute validated parent into one ambient anchor and normal components.
fn split_parent(parent: &Path) -> Result<(PathBuf, Vec<OsString>), OpenError> {
    let mut components = parent.components().peekable();
    let mut anchor = PathBuf::new();
    while matches!(
        components.peek(),
        Some(Component::Prefix(_) | Component::RootDir)
    ) {
        if let Some(component) = components.next() {
            anchor.push(component.as_os_str());
        }
    }
    if anchor.as_os_str().is_empty() {
        return Err(invalid_path(
            parent,
            "the bound path has no absolute anchor",
        ));
    }

    let mut names = Vec::new();
    for component in components {
        match component {
            Component::Normal(name) => names.push(name.to_os_string()),
            Component::CurDir => {}
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                return Err(invalid_path(
                    parent,
                    "the bound parent contains an unexpected component",
                ));
            }
        }
    }
    Ok((anchor, names))
}

/// Remove accepted current-directory components after the relative path is bound.
fn normalize_accepted_path(path: &Path) -> Result<PathBuf, OpenError> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(invalid_path(path, "parent traversal is not accepted"));
            }
        }
    }
    if !normalized.is_absolute() {
        return Err(invalid_path(path, "the bound path is not absolute"));
    }
    Ok(normalized)
}

/// Create a parent component with private Unix mode bits where supported.
#[cfg(not(windows))]
fn create_parent_component(
    current: &File,
    relative: &Path,
    _display_path: &Path,
    _final_parent: bool,
) -> std::io::Result<()> {
    create_dir(current, relative, &private_directory_options())
}

/// Create a final Windows state directory with a protected DACL from its first instant.
#[cfg(windows)]
fn create_parent_component(
    current: &File,
    relative: &Path,
    display_path: &Path,
    final_parent: bool,
) -> std::io::Result<()> {
    if final_parent {
        create_private_windows_directory(display_path)
    } else {
        create_dir(current, relative, &private_directory_options())
    }
}

/// Configure private mode bits for every directory created by the protected opener.
#[cfg(unix)]
fn private_directory_options() -> DirOptions {
    use cap_primitives::fs::DirBuilderExt;

    let mut options = DirOptions::new();
    options.mode(0o700);
    options
}

/// Use platform defaults where Unix mode bits are unavailable.
#[cfg(not(unix))]
fn private_directory_options() -> DirOptions {
    DirOptions::new()
}

/// Reject Unix ancestors that an untrusted account could replace after validation.
#[cfg(unix)]
fn validate_directory(
    directory: &File,
    path: &Path,
    final_parent: bool,
    _is_anchor: bool,
    _open_mode: OpenMode,
) -> Result<Option<File>, OpenError> {
    use std::os::unix::fs::MetadataExt;

    let metadata = directory
        .metadata()
        .map_err(|source| filesystem_error("inspect directory", path, source))?;
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid && metadata.uid() != 0 {
        return Err(invalid_path(
            path,
            "a database directory is not owned by the service user or root",
        ));
    }
    let mode = metadata.mode();
    let untrusted_writable = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    if untrusted_writable && (final_parent || !sticky) {
        return Err(invalid_path(
            path,
            "a database directory is writable by an untrusted account",
        ));
    }
    Ok(None)
}

/// Other non-Windows targets rely on retained no-follow descriptors.
#[cfg(not(any(unix, windows)))]
fn validate_directory(
    _directory: &File,
    _path: &Path,
    _final_parent: bool,
    _is_anchor: bool,
    _open_mode: OpenMode,
) -> Result<Option<File>, OpenError> {
    Ok(None)
}

/// Open a Unix database leaf atomically and reject insecure existing permissions.
#[cfg(unix)]
fn open_database_leaf(path: &Path, mode: OpenMode) -> Result<Option<File>, OpenError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

    let mut created = false;
    let file = match mode {
        OpenMode::Mutable => {
            let exclusive = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path);
            match exclusive {
                Ok(file) => {
                    created = true;
                    file
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
                        .open(path)
                        .map_err(|source| {
                            filesystem_error("open existing database leaf", path, source)
                        })?
                }
                Err(source) => {
                    return Err(filesystem_error(
                        "create private database leaf",
                        path,
                        source,
                    ));
                }
            }
        }
        OpenMode::Immutable => match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(filesystem_error(
                    "open immutable database leaf",
                    path,
                    source,
                ));
            }
        },
    };
    if created {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|source| filesystem_error("finalize private database mode", path, source))?;
    }

    let metadata = file
        .metadata()
        .map_err(|source| filesystem_error("inspect database leaf", path, source))?;
    if !metadata.is_file() {
        return Err(invalid_path(
            path,
            "the database leaf is not a regular file",
        ));
    }
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(invalid_path(
            path,
            "the database file is not owned by the service user",
        ));
    }
    let unsafe_bits = match mode {
        OpenMode::Mutable => metadata.mode() & 0o077,
        OpenMode::Immutable => metadata.mode() & 0o022,
    };
    if unsafe_bits != 0 {
        return Err(invalid_path(
            path,
            "the existing database file has unsafe permission bits",
        ));
    }
    Ok(Some(file))
}

/// Open a regular database leaf on targets without Unix or Windows extensions.
#[cfg(not(any(unix, windows)))]
fn open_database_leaf(path: &Path, mode: OpenMode) -> Result<Option<File>, OpenError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    match mode {
        OpenMode::Mutable => {
            options.write(true).create(true);
        }
        OpenMode::Immutable => {}
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound && mode == OpenMode::Immutable =>
        {
            return Ok(None);
        }
        Err(source) => return Err(filesystem_error("open database leaf", path, source)),
    };
    if !file
        .metadata()
        .map_err(|source| filesystem_error("inspect database leaf", path, source))?
        .is_file()
    {
        return Err(invalid_path(
            path,
            "the database leaf is not a regular file",
        ));
    }
    Ok(Some(file))
}

/// Detect an embedded NUL without first converting a platform string.
#[cfg(unix)]
fn has_embedded_nul(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.as_os_str().as_bytes().contains(&0)
}

/// Detect an embedded NUL in native Windows code units.
#[cfg(windows)]
fn has_embedded_nul(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str().encode_wide().any(|unit| unit == 0)
}

/// Detect an embedded NUL on targets without raw path access.
#[cfg(not(any(unix, windows)))]
fn has_embedded_nul(path: &Path) -> bool {
    path.to_string_lossy().contains('\0')
}

/// Reject a case-insensitive raw `file:` prefix, including non-UTF-8 Unix paths.
#[cfg(unix)]
fn has_sqlite_file_prefix(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    starts_with_ascii_case_insensitive(path.as_os_str().as_bytes(), b"file:")
}

/// Reject a case-insensitive raw `file:` prefix in Windows code units.
#[cfg(windows)]
fn has_sqlite_file_prefix(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    let units: Vec<u16> = path.as_os_str().encode_wide().take(5).collect();
    units.len() == 5
        && units
            .iter()
            .zip([b'f', b'i', b'l', b'e', b':'])
            .all(|(unit, expected)| {
                (*unit as u32) < 128 && (*unit as u8).eq_ignore_ascii_case(&expected)
            })
}

/// Reject a case-insensitive `file:` prefix on other targets.
#[cfg(not(any(unix, windows)))]
fn has_sqlite_file_prefix(path: &Path) -> bool {
    starts_with_ascii_case_insensitive(path.to_string_lossy().as_bytes(), b"file:")
}

/// Reject ill-formed Windows UTF-16 before registry keys or URI conversion can be lossy.
#[cfg(windows)]
fn validate_platform_path_encoding(path: &Path) -> Result<(), OpenError> {
    use std::os::windows::ffi::OsStrExt;

    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    String::from_utf16(&units)
        .map(|_| ())
        .map_err(|_| invalid_path(path, "the Windows path is not valid Unicode"))
}

/// Unix and other targets preserve their native path representation without conversion.
#[cfg(not(windows))]
fn validate_platform_path_encoding(_path: &Path) -> Result<(), OpenError> {
    Ok(())
}

/// Compare one raw byte prefix using ASCII case folding only.
fn starts_with_ascii_case_insensitive(value: &[u8], prefix: &[u8]) -> bool {
    value.len() >= prefix.len()
        && value[..prefix.len()]
            .iter()
            .zip(prefix)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

/// Reject a trailing Unix separator or terminal current-directory component.
#[cfg(unix)]
fn has_directory_terminal(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let value = path.as_os_str().as_bytes();
    value.ends_with(b"/") || value == b"." || value.ends_with(b"/.")
}

/// Reject a trailing Windows separator or terminal current-directory component.
#[cfg(windows)]
fn has_directory_terminal(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    let value: Vec<u16> = path.as_os_str().encode_wide().collect();
    let separator = |unit: u16| unit == b'/' as u16 || unit == b'\\' as u16;
    value.last().is_some_and(|unit| separator(*unit))
        || value == [b'.' as u16]
        || (value.len() >= 2
            && value[value.len() - 1] == b'.' as u16
            && separator(value[value.len() - 2]))
}

/// Reject obvious directory terminals on targets without raw path access.
#[cfg(not(any(unix, windows)))]
fn has_directory_terminal(path: &Path) -> bool {
    let value = path.to_string_lossy();
    value.ends_with('/') || value == "." || value.ends_with("/.")
}

/// Construct an internal immutable SQLite URI from raw Unix path bytes.
#[cfg(unix)]
fn immutable_database_uri(path: &Path) -> Result<PathBuf, OpenError> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    let mut uri = b"file:".to_vec();
    append_percent_encoded(&mut uri, path.as_os_str().as_bytes(), false);
    uri.extend_from_slice(b"?immutable=1&mode=ro");
    Ok(PathBuf::from(OsString::from_vec(uri)))
}

/// Construct an internal immutable SQLite URI from a validated Windows path.
#[cfg(windows)]
fn immutable_database_uri(path: &Path) -> Result<PathBuf, OpenError> {
    if path.components().next().is_some_and(|component| {
        matches!(
            component,
            Component::Prefix(prefix) if matches!(prefix.kind(), Prefix::UNC(_, _))
        )
    }) {
        return Err(invalid_path(
            path,
            "immutable UNC databases are not accepted",
        ));
    }
    let text = path
        .to_str()
        .ok_or_else(|| invalid_path(path, "the Windows path is not valid Unicode"))?;
    let normalized = text.replace('\\', "/");
    let mut uri = b"file:".to_vec();
    if !normalized.starts_with('/') {
        uri.push(b'/');
    }
    append_percent_encoded(&mut uri, normalized.as_bytes(), true);
    uri.extend_from_slice(b"?immutable=1&mode=ro");
    String::from_utf8(uri)
        .map(PathBuf::from)
        .map_err(|_| invalid_path(path, "the immutable URI could not be encoded"))
}

/// Construct an internal immutable SQLite URI on targets with UTF-8 path conversion.
#[cfg(not(any(unix, windows)))]
fn immutable_database_uri(path: &Path) -> Result<PathBuf, OpenError> {
    let text = path
        .to_str()
        .ok_or_else(|| invalid_path(path, "the database path is not valid Unicode"))?;
    let mut uri = b"file:".to_vec();
    append_percent_encoded(&mut uri, text.as_bytes(), false);
    uri.extend_from_slice(b"?immutable=1&mode=ro");
    String::from_utf8(uri)
        .map(PathBuf::from)
        .map_err(|_| invalid_path(path, "the immutable URI could not be encoded"))
}

/// Append URI-safe bytes while escaping every caller-controlled query delimiter.
fn append_percent_encoded(output: &mut Vec<u8>, input: &[u8], preserve_colon: bool) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    for byte in input {
        let unreserved = byte.is_ascii_alphanumeric()
            || matches!(*byte, b'/' | b'-' | b'.' | b'_' | b'~')
            || (preserve_colon && *byte == b':');
        if unreserved {
            output.push(*byte);
        } else {
            output.push(b'%');
            output.push(HEX[(byte >> 4) as usize]);
            output.push(HEX[(byte & 0x0f) as usize]);
        }
    }
}

/// Accept only ordinary drive and UNC filesystem prefixes.
fn validate_prefix(path: &Path, prefix: Prefix<'_>) -> Result<(), OpenError> {
    match prefix {
        Prefix::Disk(_) | Prefix::UNC(_, _) => Ok(()),
        Prefix::VerbatimDisk(_)
        | Prefix::VerbatimUNC(_, _)
        | Prefix::Verbatim(_)
        | Prefix::DeviceNS(_) => Err(invalid_path(
            path,
            "device and verbatim prefixes are not accepted",
        )),
    }
}

/// Reject invalid Windows characters, ambiguous suffixes, and reserved device names.
#[cfg(windows)]
fn validate_normal_component(path: &Path, name: &OsStr) -> Result<(), OpenError> {
    use std::os::windows::ffi::OsStrExt;

    let units: Vec<u16> = name.encode_wide().collect();
    let value = String::from_utf16(&units)
        .map_err(|_| invalid_path(path, "a Windows path component is not valid Unicode"))?;
    if value.chars().any(|character| {
        character <= '\u{1f}' || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
    }) {
        return Err(invalid_path(
            path,
            "a Windows path component contains an invalid character",
        ));
    }
    if value.ends_with([' ', '.']) {
        return Err(invalid_path(
            path,
            "Windows trailing spaces and dots are not accepted",
        ));
    }

    let stem = value.split('.').next().unwrap_or_default();
    let upper = stem.to_uppercase();
    let numbered_device = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
        .is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        });
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$" | "CONIN$" | "CONOUT$"
    ) || numbered_device
    {
        return Err(invalid_path(
            path,
            "Windows reserved device names are not accepted",
        ));
    }
    Ok(())
}

/// Unix and other non-Windows components require no additional lexical policy.
#[cfg(not(windows))]
fn validate_normal_component(_path: &Path, _name: &OsStr) -> Result<(), OpenError> {
    Ok(())
}

/// Construct one stable invalid-path error.
fn invalid_path(path: &Path, reason: &'static str) -> OpenError {
    OpenError::InvalidPath {
        path: path.to_path_buf(),
        reason,
    }
}

/// Construct one contextual filesystem error.
fn filesystem_error(operation: &'static str, path: &Path, source: std::io::Error) -> OpenError {
    OpenError::Filesystem {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

/// One Windows registry value associates an approval mode with weak retained handles.
#[cfg(windows)]
struct WindowsRegistryEntry {
    /// Active immutable and mutable opens must not reuse incompatible share modes.
    mode: OpenMode,
    /// Weak retention lets stale approvals disappear when the last store is dropped.
    handles: Weak<[File]>,
}

/// Return the process-local Windows approval registry.
#[cfg(windows)]
fn windows_guard_registry() -> &'static Mutex<HashMap<PathBuf, WindowsRegistryEntry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, WindowsRegistryEntry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Use the exact normalized lexical path so case-sensitive Windows directories stay distinct.
#[cfg(windows)]
fn windows_registry_key(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Serialize Windows approval, reuse live guards, and publish only after SQLite opens.
#[cfg(windows)]
fn open_windows_database(
    validated: ValidatedPath,
    mode: OpenMode,
) -> Result<Option<OpenedDatabase>, OpenError> {
    let key = windows_registry_key(&validated.path);
    let mut registry = windows_guard_registry().lock().map_err(|_| {
        filesystem_error(
            "lock Windows database approval registry",
            &validated.path,
            std::io::Error::other("the approval registry is poisoned"),
        )
    })?;
    registry.retain(|_, entry| entry.handles.strong_count() != 0);
    if let Some(entry) = registry.get(&key) {
        if entry.mode != mode {
            return Err(invalid_path(
                &validated.path,
                "an incompatible database mode is already active",
            ));
        }
        if let Some(handles) = entry.handles.upgrade() {
            let connection = open_sqlite_connection(&validated, mode)?;
            return Ok(Some(OpenedDatabase {
                connection,
                _guard: Some(DatabasePathGuard { _handles: handles }),
            }));
        }
    }
    registry.remove(&key);

    let Some(mut handles) = prepare_parent(&validated.parent, mode)? else {
        return Ok(None);
    };
    let Some(leaf) = open_windows_database_leaf(&validated.path, mode)? else {
        return Ok(None);
    };
    handles.push(leaf);
    if mode == OpenMode::Mutable {
        validate_existing_windows_sidecars(&validated.path)?;
    }
    let shared_handles: Arc<[File]> = handles.into();
    let connection = open_sqlite_connection(&validated, mode)?;
    registry.insert(
        key,
        WindowsRegistryEntry {
            mode,
            handles: Arc::downgrade(&shared_handles),
        },
    );
    Ok(Some(OpenedDatabase {
        connection,
        _guard: Some(DatabasePathGuard {
            _handles: shared_handles,
        }),
    }))
}

/// Validate the final Windows parent without changing its owner or DACL.
#[cfg(windows)]
fn validate_directory(
    _directory: &File,
    path: &Path,
    final_parent: bool,
    is_anchor: bool,
    mode: OpenMode,
) -> Result<Option<File>, OpenError> {
    if !final_parent {
        return Ok(None);
    }
    if is_anchor {
        return Err(invalid_path(
            path,
            "a Windows database must be inside a dedicated state directory",
        ));
    }
    let share_mode = windows_share_mode(mode);
    let handle = open_windows_object(path, true, false, mode, share_mode)?;
    validate_private_windows_object(&handle, path, WindowsAclKind::Directory)?;
    Ok(Some(handle))
}

/// Describes the exact ACE flags accepted for a Windows object.
#[cfg(windows)]
#[derive(Clone, Copy)]
enum WindowsAclKind {
    /// A protected state directory has inheritable current-user access.
    Directory,
    /// A protected database leaf has non-inheritable current-user access.
    Leaf,
    /// A SQLite sidecar may inherit the sole current-user ACE from the protected parent.
    Sidecar,
}

/// Return the retained Windows sharing policy for one database mode.
#[cfg(windows)]
fn windows_share_mode(mode: OpenMode) -> u32 {
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

    match mode {
        OpenMode::Mutable => FILE_SHARE_READ | FILE_SHARE_WRITE,
        OpenMode::Immutable => FILE_SHARE_READ,
    }
}

/// Create one final Windows state directory with a current-user-only protected DACL.
#[cfg(windows)]
fn create_private_windows_directory(path: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;

    let wide = nul_terminated_windows_path(path)?;
    with_private_windows_security_io(path, true, |attributes| {
        // SAFETY: `wide` is NUL terminated and `attributes` remains live for this call.
        if unsafe { CreateDirectoryW(wide.as_ptr(), attributes) } == 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    })
}

/// Atomically create or approve one Windows database leaf.
#[cfg(windows)]
fn open_windows_database_leaf(path: &Path, mode: OpenMode) -> Result<Option<File>, OpenError> {
    if mode == OpenMode::Immutable {
        return match approve_existing_windows_object(path, false, mode, WindowsAclKind::Leaf) {
            Ok(file) => Ok(Some(file)),
            Err(OpenError::Filesystem { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        };
    }

    match create_private_windows_file(path) {
        Ok(file) => {
            validate_windows_regular_leaf(&file, path)?;
            validate_private_windows_object(&file, path, WindowsAclKind::Leaf)?;
            Ok(Some(file))
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            approve_existing_windows_object(path, false, mode, WindowsAclKind::Leaf).map(Some)
        }
        Err(source) => Err(filesystem_error(
            "create private Windows database leaf",
            path,
            source,
        )),
    }
}

/// Approve one existing Windows object under an exclusive serialized check.
#[cfg(windows)]
fn approve_existing_windows_object(
    path: &Path,
    directory: bool,
    mode: OpenMode,
    acl_kind: WindowsAclKind,
) -> Result<File, OpenError> {
    let exclusive = open_windows_object(path, directory, false, mode, 0)?;
    if !directory {
        validate_windows_regular_leaf(&exclusive, path)?;
    }
    validate_private_windows_object(&exclusive, path, acl_kind)?;
    drop(exclusive);

    let retained = open_windows_object(path, directory, false, mode, windows_share_mode(mode))?;
    if !directory {
        validate_windows_regular_leaf(&retained, path)?;
    }
    validate_private_windows_object(&retained, path, acl_kind)?;
    Ok(retained)
}

/// Validate existing SQLite sidecars, then release them before SQLite manages their lifecycle.
#[cfg(windows)]
fn validate_existing_windows_sidecars(path: &Path) -> Result<(), OpenError> {
    for suffix in ["-journal", "-wal", "-shm"] {
        let mut sidecar_name = path.as_os_str().to_os_string();
        sidecar_name.push(suffix);
        let sidecar = PathBuf::from(sidecar_name);
        match approve_existing_windows_object(
            &sidecar,
            false,
            OpenMode::Mutable,
            WindowsAclKind::Sidecar,
        ) {
            Ok(file) => drop(file),
            Err(OpenError::Filesystem { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Open a Windows object without following a reparse point or allowing deletion.
#[cfg(windows)]
fn open_windows_object(
    path: &Path,
    directory: bool,
    create_new: bool,
    mode: OpenMode,
    share_mode: u32,
) -> Result<File, OpenError> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, READ_CONTROL,
    };

    let desired = match mode {
        OpenMode::Mutable if !directory => FILE_GENERIC_READ | FILE_GENERIC_WRITE | READ_CONTROL,
        _ => FILE_GENERIC_READ | READ_CONTROL,
    };
    let backup = if directory {
        FILE_FLAG_BACKUP_SEMANTICS
    } else {
        0
    };
    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .access_mode(desired)
        .share_mode(share_mode)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | backup);
    if create_new {
        options.create_new(true);
    }
    options
        .open(path)
        .map_err(|source| filesystem_error("open Windows database object", path, source))
}

/// Atomically create a Windows database leaf with a private security descriptor.
#[cfg(windows)]
fn create_private_windows_file(path: &Path) -> std::io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, READ_CONTROL,
    };

    let wide = nul_terminated_windows_path(path)?;
    with_private_windows_security_io(path, false, |attributes| {
        // SAFETY: the path and security descriptor remain valid for the duration of the call.
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_GENERIC_READ | FILE_GENERIC_WRITE | READ_CONTROL,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OPEN_REPARSE_POINT,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            Err(std::io::Error::last_os_error())
        } else {
            // SAFETY: a successful `CreateFileW` returns one uniquely owned live handle.
            Ok(unsafe { File::from_raw_handle(handle.cast()) })
        }
    })
}

/// Convert an accepted Windows path into a NUL-terminated UTF-16 buffer.
#[cfg(windows)]
fn nul_terminated_windows_path(path: &Path) -> std::io::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    if wide.contains(&0) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "the path contains an embedded NUL",
        ));
    }
    wide.push(0);
    Ok(wide)
}

/// Run one creation call with a current-user-only protected security descriptor.
#[cfg(windows)]
fn with_private_windows_security_io<T>(
    path: &Path,
    inherit: bool,
    action: impl FnOnce(*const windows_sys::Win32::Security::SECURITY_ATTRIBUTES) -> std::io::Result<T>,
) -> std::io::Result<T> {
    with_current_user_sid_io(path, |sid| {
        use std::ptr::{null, null_mut};
        use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
        use windows_sys::Win32::Security::Authorization::{
            SetEntriesInAclW, EXPLICIT_ACCESS_W, SET_ACCESS, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
            TRUSTEE_W,
        };
        use windows_sys::Win32::Security::{
            InitializeSecurityDescriptor, SetSecurityDescriptorControl, SetSecurityDescriptorDacl,
            SetSecurityDescriptorOwner, ACL, CONTAINER_INHERIT_ACE, OBJECT_INHERIT_ACE,
            SECURITY_ATTRIBUTES, SECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
        };
        use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
        use windows_sys::Win32::System::SystemServices::SECURITY_DESCRIPTOR_REVISION;

        let inheritance = if inherit {
            CONTAINER_INHERIT_ACE | OBJECT_INHERIT_ACE
        } else {
            0
        };
        let trustee = TRUSTEE_W {
            pMultipleTrustee: null_mut(),
            MultipleTrusteeOperation: 0,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: sid.cast(),
        };
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: inheritance,
            Trustee: trustee,
        };
        let mut acl: *mut ACL = null_mut();
        // SAFETY: `access` and `acl` describe one valid ACL construction request.
        let status = unsafe { SetEntriesInAclW(1, &access, null(), &mut acl) };
        if status != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(status as i32));
        }

        let result = (|| {
            let mut descriptor = SECURITY_DESCRIPTOR::default();
            // SAFETY: `descriptor` is writable storage for an absolute security descriptor.
            if unsafe {
                InitializeSecurityDescriptor(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    SECURITY_DESCRIPTOR_REVISION,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `sid` remains live until the creation action returns.
            if unsafe {
                SetSecurityDescriptorOwner(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    sid,
                    0,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `acl` remains allocated and valid throughout the creation call.
            if unsafe {
                SetSecurityDescriptorDacl(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    1,
                    acl,
                    0,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: the initialized descriptor is live and both masks are valid control flags.
            if unsafe {
                SetSecurityDescriptorControl(
                    (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                    SE_DACL_PROTECTED,
                    SE_DACL_PROTECTED,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: (&mut descriptor as *mut SECURITY_DESCRIPTOR).cast(),
                bInheritHandle: 0,
            };
            action(&attributes)
        })();

        // SAFETY: `SetEntriesInAclW` allocated `acl` with `LocalAlloc`.
        unsafe {
            LocalFree(acl.cast());
        }
        result
    })
}

/// Run one Windows operation while the current token user's SID remains live.
#[cfg(windows)]
fn with_current_user_sid_io<T>(
    _path: &Path,
    action: impl FnOnce(windows_sys::Win32::Security::PSID) -> std::io::Result<T>,
) -> std::io::Result<T> {
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let mut token: HANDLE = null_mut();
    // SAFETY: the pseudo-process handle is valid and `token` points to writable storage.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let result = (|| {
        let mut required = 0_u32;
        // SAFETY: a null buffer intentionally queries the required token information size.
        unsafe {
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut required);
        }
        if required == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let word_size = std::mem::size_of::<usize>();
        let mut token_data = vec![0_usize; (required as usize).div_ceil(word_size)];
        // SAFETY: the aligned vector has at least `required` writable bytes.
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                token_data.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the successful query initialized `TOKEN_USER` at the buffer start.
        let user = unsafe { &*(token_data.as_ptr().cast::<TOKEN_USER>()) };
        action(user.User.Sid)
    })();
    // SAFETY: `token` was initialized by the successful `OpenProcessToken` call.
    unsafe {
        CloseHandle(token);
    }
    result
}

/// Validate one Windows object as current-user-owned with exactly one private ACE.
#[cfg(windows)]
fn validate_private_windows_object(
    file: &File,
    path: &Path,
    kind: WindowsAclKind,
) -> Result<(), OpenError> {
    use std::os::windows::io::AsRawHandle;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_SUCCESS};
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        EqualSid, GetAce, GetSecurityDescriptorControl, ACCESS_ALLOWED_ACE, CONTAINER_INHERIT_ACE,
        DACL_SECURITY_INFORMATION, INHERITED_ACE, OBJECT_INHERIT_ACE, OWNER_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    with_current_user_sid_io(path, |current_sid| {
        let mut owner: PSID = null_mut();
        let mut dacl = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        // SAFETY: the live handle grants `READ_CONTROL`; all requested output pointers are valid.
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut dacl,
                null_mut(),
                &mut descriptor,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(status as i32));
        }

        let result = (|| {
            if owner.is_null() || unsafe { EqualSid(owner, current_sid) } == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Windows object is not owned by the current user",
                ));
            }
            if dacl.is_null() || unsafe { (*dacl).AceCount } != 1 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Windows object does not have exactly one private ACE",
                ));
            }
            let mut ace_pointer = null_mut();
            // SAFETY: the validated ACL contains exactly one ACE at index zero.
            if unsafe { GetAce(dacl, 0, &mut ace_pointer) } == 0 || ace_pointer.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: the ACE type is checked before fields specific to `ACCESS_ALLOWED_ACE` matter.
            let ace = unsafe { &*(ace_pointer.cast::<ACCESS_ALLOWED_ACE>()) };
            if ace.Header.AceType != ACCESS_ALLOWED_ACE_TYPE as u8 || ace.Mask != FILE_ALL_ACCESS {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Windows object ACE is not current-user full control",
                ));
            }
            let ace_sid = (&ace.SidStart as *const u32).cast_mut().cast();
            // SAFETY: an access-allowed ACE stores its SID beginning at `SidStart`.
            if unsafe { EqualSid(ace_sid, current_sid) } == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Windows object ACE belongs to another identity",
                ));
            }

            let allowed_flags = match kind {
                WindowsAclKind::Directory => OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE,
                WindowsAclKind::Leaf => 0,
                WindowsAclKind::Sidecar => {
                    OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE | INHERITED_ACE
                }
            };
            if u32::from(ace.Header.AceFlags) & !allowed_flags != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Windows object ACE has unexpected flags",
                ));
            }
            if matches!(kind, WindowsAclKind::Directory)
                && u32::from(ace.Header.AceFlags) != OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Windows state directory ACE is not inheritable",
                ));
            }
            if matches!(kind, WindowsAclKind::Leaf) && ace.Header.AceFlags != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Windows database leaf ACE is not exact",
                ));
            }

            let mut control = 0_u16;
            let mut revision = 0_u32;
            // SAFETY: `descriptor` is the live security descriptor returned by `GetSecurityInfo`.
            if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0
            {
                return Err(std::io::Error::last_os_error());
            }
            if !matches!(kind, WindowsAclKind::Sidecar) && control & SE_DACL_PROTECTED == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "the Windows object DACL is not protected",
                ));
            }
            Ok(())
        })();

        // SAFETY: `GetSecurityInfo` allocated `descriptor` with `LocalAlloc`.
        unsafe {
            LocalFree(descriptor.cast());
        }
        result
    })
    .map_err(|source| filesystem_error("validate private Windows security", path, source))
}

/// Reject a Windows database leaf that is a directory or reparse point.
#[cfg(windows)]
fn validate_windows_regular_leaf(file: &File, path: &Path) -> Result<(), OpenError> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

    let metadata = file
        .metadata()
        .map_err(|source| filesystem_error("inspect Windows database leaf", path, source))?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid_path(
            path,
            "the database leaf is not a regular non-reparse file",
        ));
    }
    Ok(())
}

#[cfg(test)]
/// Regression tests for validation, no-follow traversal, immutable reads, and lifetime guards.
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Monotonic suffixes keep test roots disjoint within one process.
    static NEXT_TEST_ROOT: AtomicU64 = AtomicU64::new(1);

    /// Create one unique absolute root reserved for the current test process.
    fn temporary_root() -> PathBuf {
        let suffix = NEXT_TEST_ROOT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("henosis-sqlite-{}-{suffix}", std::process::id()))
    }

    /// Nested parents and a private database leaf are created and opened.
    #[test]
    fn nested_database_is_created() {
        let root = temporary_root();
        let path = root.join("state").join("service.sqlite");
        let opened = open_database(&path).expect("open protected database");
        opened
            .execute_batch("CREATE TABLE proof (value INTEGER NOT NULL);")
            .expect("write protected database");
        assert!(path.is_file());

        drop(opened);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The installer primitive creates a private directory without a probe file.
    #[test]
    fn managed_directory_is_created_without_probe() {
        let root = temporary_root();
        let path = root.join("state");
        ensure_private_directory(&path).expect("create managed directory");
        assert!(path.is_dir());
        assert!(!path.join(".henosis-private-directory-probe").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The installer primitive refuses to reclaim an insecure existing Unix directory.
    #[cfg(unix)]
    #[test]
    fn managed_directory_does_not_reclaim_insecure_unix_state() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = temporary_root();
        std::fs::create_dir(&root).expect("create test root");
        let path = root.join("state");
        std::fs::create_dir(&path).expect("create insecure state");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("set insecure mode");

        assert!(ensure_private_directory(&path).is_err());
        assert_eq!(
            std::fs::metadata(&path).expect("inspect state").mode() & 0o777,
            0o755
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Parent traversal is rejected before its leading normal component can be created.
    #[test]
    fn parent_traversal_has_no_filesystem_side_effect() {
        let root = temporary_root();
        std::fs::create_dir(&root).expect("create traversal test root");
        let leading = root.join("leading");
        let path = leading.join("..").join("outside.sqlite");

        assert!(open_database(&path).is_err(), "parent traversal must fail");
        assert!(!leading.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// SQLite URI, in-memory, NUL, separator, and dot terminals are rejected before mutation.
    #[test]
    fn control_paths_have_no_filesystem_side_effect() {
        let root = temporary_root();
        for path in [
            PathBuf::from("file:/tmp/escaped.sqlite"),
            PathBuf::from(":memory:"),
            PathBuf::new(),
            root.join("nul\0.sqlite"),
            root.join("slash").join(""),
            root.join("dot").join("."),
        ] {
            assert!(open_database(path).is_err(), "control path must fail");
        }
        assert!(!root.exists());
    }

    /// A raw non-UTF-8 Unix URI prefix is rejected without lossy conversion.
    #[cfg(unix)]
    #[test]
    fn unix_non_utf8_file_prefix_is_rejected() {
        use std::os::unix::ffi::OsStringExt;

        let path = PathBuf::from(OsString::from_vec(vec![
            b'F', b'i', b'L', b'e', b':', 0xff, b'.', b'd', b'b',
        ]));
        assert!(open_database(path).is_err());
    }

    /// Relative paths are bound to one normalized absolute path before traversal.
    #[test]
    fn relative_path_is_bound_once() {
        let validated =
            ValidatedPath::new(Path::new("./state/./service.sqlite")).expect("validate path");
        assert!(validated.path.is_absolute());
        assert!(!validated
            .path
            .components()
            .any(|component| matches!(component, Component::CurDir)));
    }

    /// An absent immutable source returns `None` without creating its parent.
    #[test]
    fn absent_immutable_database_has_no_side_effect() {
        let root = temporary_root();
        let path = root.join("state").join("missing.sqlite");
        assert!(open_database_read_only(&path)
            .expect("check absent database")
            .is_none());
        assert!(!root.exists());
    }

    /// Immutable URI escaping keeps fragment syntax inside a platform-valid literal filename.
    #[test]
    fn immutable_uri_escapes_fragment_delimiter() {
        let root = temporary_root();
        let path = root.join("state").join("literal#fragment.sqlite");
        let opened = open_database(&path).expect("create literal database");
        opened
            .execute_batch("CREATE TABLE proof (value INTEGER); INSERT INTO proof VALUES (7);")
            .expect("seed literal database");
        drop(opened);

        let immutable = open_database_read_only(&path)
            .expect("open immutable database")
            .expect("database exists");
        let value: i64 = immutable
            .query_row("SELECT value FROM proof", [], |row| row.get(0))
            .expect("query literal database");
        assert_eq!(value, 7);
        assert!(immutable
            .execute("INSERT INTO proof VALUES (8)", [])
            .is_err());
        assert!(!PathBuf::from(format!("{}-journal", path.display())).exists());
        assert!(!PathBuf::from(format!("{}-wal", path.display())).exists());
        assert!(!PathBuf::from(format!("{}-shm", path.display())).exists());

        drop(immutable);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Unix URI construction percent-encodes a caller-controlled query delimiter.
    #[cfg(unix)]
    #[test]
    fn unix_immutable_uri_escapes_query_delimiter() {
        use std::os::unix::ffi::OsStrExt;

        let uri = immutable_database_uri(Path::new("/tmp/literal?mode=memory.sqlite"))
            .expect("construct immutable URI");
        assert_eq!(
            uri.as_os_str().as_bytes(),
            b"file:/tmp/literal%3Fmode%3Dmemory.sqlite?immutable=1&mode=ro"
        );
    }

    /// A Unix symbolic-link parent cannot redirect database creation.
    #[cfg(unix)]
    #[test]
    fn unix_symbolic_link_parent_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = temporary_root();
        let outside = temporary_root();
        std::fs::create_dir(&root).expect("create symlink test root");
        std::fs::create_dir(&outside).expect("create outside directory");
        symlink(&outside, root.join("linked")).expect("create directory symlink");

        assert!(open_database(root.join("linked").join("escaped.sqlite")).is_err());
        assert!(!outside.join("escaped.sqlite").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// A Unix symbolic-link leaf cannot redirect the SQLite connection.
    #[cfg(unix)]
    #[test]
    fn unix_symbolic_link_leaf_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = temporary_root();
        let outside = temporary_root();
        std::fs::create_dir(&root).expect("create leaf test root");
        std::fs::write(&outside, b"outside").expect("create outside file");
        let leaf = root.join("linked.sqlite");
        symlink(&outside, &leaf).expect("create database symlink");

        assert!(open_database(&leaf).is_err());
        assert_eq!(
            std::fs::read(&outside).expect("read outside file"),
            b"outside"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
    }

    /// An insecure existing Unix leaf is rejected without changing its mode.
    #[cfg(unix)]
    #[test]
    fn unix_insecure_existing_leaf_is_not_reclaimed() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let root = temporary_root();
        std::fs::create_dir(&root).expect("create leaf parent");
        let path = root.join("unsafe.sqlite");
        std::fs::write(&path, b"unsafe").expect("create unsafe leaf");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("set unsafe mode");

        assert!(open_database(&path).is_err());
        assert_eq!(
            std::fs::metadata(&path)
                .expect("inspect unsafe leaf")
                .mode()
                & 0o777,
            0o644
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A newly created Unix database leaf has exact private mode bits.
    #[cfg(unix)]
    #[test]
    fn unix_new_leaf_is_mode_0600() {
        use std::os::unix::fs::MetadataExt;

        let root = temporary_root();
        let path = root.join("state").join("private.sqlite");
        let opened = open_database(&path).expect("create private leaf");
        assert_eq!(
            std::fs::metadata(&path)
                .expect("inspect private leaf")
                .mode()
                & 0o777,
            0o600
        );

        drop(opened);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A Unix final parent writable by other accounts is rejected.
    #[cfg(unix)]
    #[test]
    fn unix_untrusted_writable_parent_is_rejected() {
        use std::os::unix::fs::PermissionsExt;

        let root = temporary_root();
        std::fs::create_dir(&root).expect("create unsafe parent");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777))
            .expect("make parent unsafe");

        assert!(open_database(root.join("unsafe.sqlite")).is_err());
        assert!(!root.join("unsafe.sqlite").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Invalid Windows components are rejected lexically before filesystem access.
    #[cfg(windows)]
    #[test]
    fn windows_invalid_names_are_rejected() {
        for name in [
            "bad?.sqlite",
            "trailing. ",
            "COM1.sqlite",
            "LPT².sqlite",
            "NUL.txt",
        ] {
            assert!(open_database(Path::new("state").join(name)).is_err());
        }
    }

    /// Repeated Windows opens reuse approved retained handles instead of self-conflicting.
    #[cfg(windows)]
    #[test]
    fn windows_repeated_open_reuses_guard() {
        let root = temporary_root();
        let path = root.join("state").join("service.sqlite");
        let first = open_database(&path).expect("open first database");
        let second = open_database(&path).expect("open repeated database");
        first
            .execute_batch("CREATE TABLE proof (value INTEGER);")
            .expect("write first connection");
        second
            .execute("INSERT INTO proof VALUES (1)", [])
            .expect("write second connection");

        drop(second);
        drop(first);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Active mutable and immutable Windows approvals cannot share incompatible handles.
    #[cfg(windows)]
    #[test]
    fn windows_incompatible_modes_fail_closed() {
        let root = temporary_root();
        let path = root.join("state").join("service.sqlite");
        let mutable = open_database(&path).expect("open mutable database");

        assert!(open_database_read_only(&path).is_err());

        drop(mutable);
        assert!(open_database_read_only(&path)
            .expect("open released immutable database")
            .is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Retained Windows parent and leaf handles block replacement until the guard drops.
    #[cfg(windows)]
    #[test]
    fn windows_guard_blocks_parent_and_leaf_rename() {
        let root = temporary_root();
        let parent = root.join("state");
        let path = parent.join("service.sqlite");
        let renamed_parent = root.join("renamed-state");
        let renamed_leaf = parent.join("renamed.sqlite");
        let opened = open_database(&path).expect("open protected database");

        assert!(std::fs::rename(&parent, &renamed_parent).is_err());
        assert!(std::fs::rename(&path, &renamed_leaf).is_err());

        drop(opened);
        std::fs::rename(&path, &renamed_leaf).expect("rename released leaf");
        std::fs::rename(&parent, &renamed_parent).expect("rename released parent");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// An insecure existing Windows state directory is rejected without DACL reclamation.
    #[cfg(windows)]
    #[test]
    fn windows_insecure_parent_is_not_reclaimed() {
        let root = temporary_root();
        let parent = root.join("state");
        std::fs::create_dir(&root).expect("create ordinary root");
        std::fs::create_dir(&parent).expect("create inherited state directory");
        let before = open_windows_object(
            &parent,
            true,
            false,
            OpenMode::Mutable,
            windows_share_mode(OpenMode::Mutable),
        )
        .expect("open ordinary directory");
        assert!(
            validate_private_windows_object(&before, &parent, WindowsAclKind::Directory).is_err()
        );
        drop(before);

        assert!(open_database(parent.join("service.sqlite")).is_err());
        assert!(!parent.join("service.sqlite").exists());

        let after = open_windows_object(
            &parent,
            true,
            false,
            OpenMode::Mutable,
            windows_share_mode(OpenMode::Mutable),
        )
        .expect("reopen ordinary directory");
        assert!(
            validate_private_windows_object(&after, &parent, WindowsAclKind::Directory).is_err()
        );
        drop(after);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A preheld Windows leaf prevents a fresh approval from being published.
    #[cfg(windows)]
    #[test]
    fn windows_preheld_leaf_fails_closed() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let root = temporary_root();
        let path = root.join("state").join("service.sqlite");
        drop(open_database(&path).expect("create protected database"));
        let preheld = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&path)
            .expect("prehold database leaf");

        assert!(open_database(&path).is_err());

        drop(preheld);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A Windows parent reparse point cannot redirect database creation.
    #[cfg(windows)]
    #[test]
    fn windows_parent_reparse_point_is_rejected() {
        use std::os::windows::fs::symlink_dir;

        let root = temporary_root();
        let outside = temporary_root();
        std::fs::create_dir(&root).expect("create symlink root");
        std::fs::create_dir(&outside).expect("create outside directory");
        let link = root.join("linked");
        if let Err(error) = symlink_dir(&outside, &link) {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_dir_all(&outside);
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create directory symlink: {error}");
        }

        assert!(open_database(link.join("escaped.sqlite")).is_err());
        assert!(!outside.join("escaped.sqlite").exists());

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    /// A Windows leaf reparse point cannot redirect the primary database open.
    #[cfg(windows)]
    #[test]
    fn windows_leaf_reparse_point_is_rejected() {
        use std::os::windows::fs::symlink_file;

        let root = temporary_root();
        let outside = temporary_root();
        let parent = root.join("state");
        ensure_private_directory(&parent).expect("create protected parent");
        std::fs::write(&outside, b"outside").expect("create outside file");
        let leaf = parent.join("service.sqlite");
        if let Err(error) = symlink_file(&outside, &leaf) {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_file(&outside);
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create leaf symlink: {error}");
        }

        assert!(open_database(&leaf).is_err());
        assert_eq!(
            std::fs::read(&outside).expect("read outside file"),
            b"outside"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
    }

    /// A Windows sidecar reparse point cannot redirect SQLite auxiliary access.
    #[cfg(windows)]
    #[test]
    fn windows_sidecar_reparse_point_is_rejected() {
        use std::os::windows::fs::symlink_file;

        let root = temporary_root();
        let outside = temporary_root();
        let path = root.join("state").join("service.sqlite");
        drop(open_database(&path).expect("create protected database"));
        std::fs::write(&outside, b"outside").expect("create outside file");
        let mut sidecar_name = path.as_os_str().to_os_string();
        sidecar_name.push("-wal");
        let sidecar = PathBuf::from(sidecar_name);
        if let Err(error) = symlink_file(&outside, &sidecar) {
            let _ = std::fs::remove_dir_all(&root);
            let _ = std::fs::remove_file(&outside);
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create sidecar symlink: {error}");
        }

        assert!(open_database(&path).is_err());
        assert_eq!(
            std::fs::read(&outside).expect("read outside file"),
            b"outside"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(&outside);
    }

    /// Sidecar validation releases its handle before SQLite needs to replace the file.
    #[cfg(windows)]
    #[test]
    fn windows_sidecar_validation_does_not_retain_handle() {
        let root = temporary_root();
        let path = root.join("state").join("service.sqlite");
        drop(open_database(&path).expect("create protected database"));
        let mut sidecar_name = path.as_os_str().to_os_string();
        sidecar_name.push("-journal");
        let sidecar = PathBuf::from(sidecar_name);
        std::fs::write(&sidecar, b"").expect("create inherited sidecar");

        validate_existing_windows_sidecars(&path).expect("validate sidecar");
        std::fs::remove_file(&sidecar).expect("remove released sidecar");

        let _ = std::fs::remove_dir_all(&root);
    }
}
