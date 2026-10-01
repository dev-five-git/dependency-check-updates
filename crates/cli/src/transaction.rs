//! Recoverable multi-file updates. Each replacement is atomic, the batch is not
//! crash-atomic. A receipt and same-directory backups survive forced termination.
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::{Builder, TempPath};

const RECEIPT_PREFIX: &str = ".dcu-transaction-";
const TARGET_RECEIPT_PREFIX: &str = ".dcu-pending-";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetReceipt {
    receipt: PathBuf,
}

fn marker_path(path: &Path) -> Result<PathBuf, String> {
    let path = std::fs::canonicalize(path).map_err(|e| e.to_string())?;
    let name = path.file_name().ok_or("invalid marker target")?;
    Ok(path.with_file_name(format!(
        "{TARGET_RECEIPT_PREFIX}{}.json",
        digest(name.as_encoded_bytes())
    )))
}

fn read_marker(path: &Path) -> Result<Option<TargetReceipt>, String> {
    use std::io::Read;
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > 16 * 1024 {
        return Err("unsafe target recovery marker".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    decode_marker(&bytes).map(Some)
}

fn decode_marker(bytes: &[u8]) -> Result<TargetReceipt, String> {
    if bytes.len() > 16 * 1024 {
        return Err("target recovery marker exceeds size limit".into());
    }
    let marker: TargetReceipt =
        serde_json::from_slice(bytes).map_err(|_| "invalid target recovery marker")?;
    if !marker.receipt.is_absolute()
        || !marker.receipt.file_name().is_some_and(|n| {
            n.to_string_lossy().starts_with(RECEIPT_PREFIX)
                && n.to_string_lossy().ends_with(".json")
        })
    {
        return Err("invalid target receipt path".into());
    }
    Ok(marker)
}

/// Read-only lookup: notices interrupted writes regardless of the caller's cwd.
pub(crate) fn pending_for<'a>(
    paths: impl Iterator<Item = &'a Path>,
) -> Result<Vec<PathBuf>, String> {
    let mut receipts = Vec::new();
    for path in paths {
        if let Some(marker) = read_marker(&marker_path(path)?)?
            && marker.receipt.try_exists().map_err(|e| e.to_string())?
        {
            receipts.push(marker.receipt);
        }
    }
    receipts.sort();
    receipts.dedup();
    Ok(receipts)
}

fn target_markers(staged: &[Staged], receipt: &Path) -> Result<Vec<TempPath>, String> {
    let receipt = std::fs::canonicalize(receipt).map_err(|e| e.to_string())?;
    let mut markers = Vec::new();
    for target in staged {
        let path = marker_path(&target.change.path)?;
        if let Some(old) = read_marker(&path)? {
            if old.receipt.try_exists().map_err(|e| e.to_string())? {
                return Err(format!(
                    "pending update receipt {}; recover from its directory",
                    old.receipt.display()
                ));
            }
            // A completed transaction may crash between receipt and marker deletion.
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        let mut marker = Builder::new()
            .prefix(
                path.file_name()
                    .unwrap()
                    .to_str()
                    .ok_or("non-UTF-8 marker name")?,
            )
            .rand_bytes(0)
            .tempfile_in(path.parent().unwrap())
            .map_err(|e| e.to_string())?;
        serde_json::to_writer(
            &mut marker,
            &TargetReceipt {
                receipt: receipt.clone(),
            },
        )
        .map_err(|e| e.to_string())?;
        marker.as_file().sync_all().map_err(|e| e.to_string())?;
        sync_directory(path.parent().unwrap())?;
        markers.push(marker.into_temp_path());
    }
    Ok(markers)
}

pub(crate) struct Change {
    pub path: PathBuf,
    pub original: Vec<u8>,
    pub replacement: Vec<u8>,
}

pub(crate) struct Failure {
    pub detail: String,
    pub recovery_required: bool,
    pub rolled_back: bool,
    pub committed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Stage,
    Commit,
    Rollback,
    Finalize,
}

struct Staged {
    change: Change,
    stage: Option<TempPath>,
    backup: Option<TempPath>,
    committed: bool,
}

#[derive(Serialize)]
struct ReceiptEntry<'a> {
    path: &'a Path,
    staged: &'a Path,
    backup: &'a Path,
    original_sha256: String,
    replacement_sha256: String,
}

fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

// Stable sibling lock identities survive atomic target replacement. Do not
// unlink them: unlinking a locked inode allows a second process to lock a new
// inode with the same name. OS locks themselves vanish when a process dies.
// Explicit unlock matters on Unix: a concurrent fork may briefly inherit the
// open-file description before exec closes it. Closing only the parent's fd
// can otherwise leave a completed transaction apparently locked by that child.
struct FileLock(std::fs::File);

impl FileLock {
    fn acquire(file: std::fs::File) -> Result<Self, std::io::Error> {
        fs2::FileExt::try_lock_exclusive(&file)?;
        Ok(Self(file))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

fn target_locks<'a>(paths: impl Iterator<Item = &'a Path>) -> Result<Vec<FileLock>, String> {
    let mut paths: Vec<_> = paths
        .map(std::fs::canonicalize)
        .collect::<Result<_, _>>()
        .map_err(|e| e.to_string())?;
    paths.sort();
    paths.dedup();
    let mut locks = Vec::new();
    for path in paths {
        let name = path.file_name().ok_or("invalid lock target")?;
        let lock_path =
            path.with_file_name(format!(".dcu-lock-{}", digest(name.as_encoded_bytes())));
        if let Ok(meta) = std::fs::symlink_metadata(&lock_path)
            && (!meta.is_file() || meta.file_type().is_symlink())
        {
            return Err("unsafe sibling lock file".into());
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|e| e.to_string())?;
        locks.push(
            FileLock::acquire(file)
                .map_err(|_| format!("update target is busy: {}", path.display()))?,
        );
    }
    Ok(locks)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    files: Vec<RecoveryEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoveryEntry {
    path: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
    original_sha256: String,
    replacement_sha256: String,
}

fn scoped(root: &Path, path: &Path) -> Result<(), String> {
    let parent = path.parent().ok_or("recovery path has no parent")?;
    if !path.is_absolute()
        || !std::fs::canonicalize(parent)
            .map_err(|e| e.to_string())?
            .starts_with(root)
    {
        return Err("transaction path escapes the working directory".into());
    }
    let metadata = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err("transaction path is not a regular non-symlink file".into());
    }
    #[cfg(windows)]
    validate_windows_target(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err("hard-linked recovery path is unsupported".into());
        }
    }
    Ok(())
}

/// Explicit, idempotent recovery. Preflight the whole receipt before a write.
/// Already-restored targets are accepted; external edits and corrupt backups
/// block the operation without removing recovery evidence.
#[allow(clippy::too_many_lines)]
pub(crate) fn recover(root: &Path, finish: bool) -> Result<bool, String> {
    use std::io::Read;
    let receipts = pending(root).map_err(|e| e.to_string())?;
    if receipts.is_empty() {
        return Ok(false);
    }
    if receipts.len() != 1 {
        return Err("multiple recovery receipts; reconcile them individually".into());
    }
    let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let receipt_path = &receipts[0];
    scoped(&root, receipt_path)?;
    let mut handle = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(receipt_path)
        .map_err(|e| e.to_string())?;
    let _receipt_lock = FileLock::acquire(handle.try_clone().map_err(|e| e.to_string())?)
        .map_err(|_| "transaction is still active or recovery is already running")?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut handle)
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err("receipt exceeds size limit".into());
    }
    let receipt: Receipt =
        serde_json::from_slice(&bytes).map_err(|_| "invalid recovery receipt")?;
    if receipt.schema_version != 1 || receipt.files.is_empty() || receipt.files.len() > 4096 {
        return Err("unsupported recovery receipt schema or file count".into());
    }
    let mut identities = HashSet::new();
    let mut artifacts = HashSet::new();
    let mut originals = Vec::new();
    let mut changes = Vec::new();
    for entry in &receipt.files {
        scoped(&root, &entry.path)?;
        if !identities.insert(std::fs::canonicalize(&entry.path).map_err(|e| e.to_string())?) {
            return Err("duplicate recovery target".into());
        }
        for (path, prefix) in [
            (&entry.backup, ".dcu-backup-"),
            (&entry.staged, ".dcu-stage-"),
        ] {
            if path.parent() != entry.path.parent()
                || !path.file_name().is_some_and(|n| {
                    n.to_string_lossy().starts_with(prefix) && n.to_string_lossy().ends_with(".tmp")
                })
                || !artifacts.insert(path.clone())
            {
                return Err("invalid recovery artifact path".into());
            }
            if path.exists() {
                scoped(&root, path)?;
            }
        }
        let current = std::fs::read(&entry.path).map_err(|e| e.to_string())?;
        let hash = digest(&current);
        if hash != entry.original_sha256 && hash != entry.replacement_sha256 {
            return Err(format!(
                "external edit detected in {}; recovery preserved",
                entry.path.display()
            ));
        }
        let desired = if finish {
            &entry.replacement_sha256
        } else {
            &entry.original_sha256
        };
        let source = if finish { &entry.staged } else { &entry.backup };
        if hash != *desired {
            let data = std::fs::read(source)
                .map_err(|_| format!("missing recovery artifact {}", source.display()))?;
            if digest(&data) != *desired {
                return Err(format!("corrupt recovery artifact {}", source.display()));
            }
            changes.push((entry.path.clone(), current.clone(), data));
        }
        originals.push((entry.path.clone(), current));
    }
    // Stage new copies, never consume original backups: a crash during recovery
    // can repeat the same command and validate each target again.
    let _locks = target_locks(receipt.files.iter().map(|e| e.path.as_path()))?;
    let receipt_identity = std::fs::canonicalize(receipt_path).map_err(|e| e.to_string())?;
    let mut markers = Vec::new();
    for entry in &receipt.files {
        let path = marker_path(&entry.path)?;
        if let Some(marker) = read_marker(&path)? {
            if marker.receipt != receipt_identity {
                return Err("target belongs to a different recovery receipt".into());
            }
            markers.push(path);
        }
    }
    let mut staged = Vec::new();
    for (path, old, data) in changes {
        let permissions = std::fs::metadata(&path)
            .map_err(|e| e.to_string())?
            .permissions();
        staged.push((
            path.clone(),
            old,
            Some(file(
                path.parent().unwrap(),
                ".dcu-stage-",
                &data,
                permissions,
                &path,
            )?),
        ));
    }
    for (path, current) in &originals {
        unchanged(path, current)?;
    }
    for (path, current, stage) in &mut staged {
        unchanged(path, current)?;
        replace(stage, path)?;
    }
    for entry in &receipt.files {
        for path in [&entry.backup, &entry.staged] {
            if path.exists() {
                std::fs::remove_file(path).map_err(|e| e.to_string())?;
            }
        }
    }
    // Keep the OS lock until after evidence cleanup. Closing releases it after
    // forced termination too; a live transaction cannot be mistaken for a crash.
    std::fs::remove_file(receipt_path).map_err(|e| e.to_string())?;
    sync_directory(&root)?;
    for path in markers {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        sync_directory(path.parent().unwrap())?;
    }
    Ok(true)
}

pub(crate) fn pending(root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(RECEIPT_PREFIX)
            && name.ends_with(".json")
            && entry.file_type()?.is_file()
        {
            paths.push(entry.path());
        }
    }
    paths.sort();
    Ok(paths)
}

fn file(
    parent: &Path,
    prefix: &str,
    bytes: &[u8],
    permissions: std::fs::Permissions,
    origin: &Path,
) -> Result<TempPath, String> {
    let mut f = Builder::new()
        .prefix(prefix)
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|e| e.to_string())?;
    #[cfg(windows)]
    copy_windows_dacl(origin, f.path())?;
    #[cfg(unix)]
    preserve_unix_metadata(origin, f.path())?;
    f.write_all(bytes).map_err(|e| e.to_string())?;
    f.as_file()
        .set_permissions(permissions)
        .map_err(|e| e.to_string())?;
    // Writing/chmod may clear Unix capabilities or alter ACL masks. Restore
    // the exposed attributes after the final data/mode operation as well.
    #[cfg(unix)]
    preserve_unix_metadata(origin, f.path())?;
    f.as_file().sync_all().map_err(|e| e.to_string())?;
    Ok(f.into_temp_path())
}

#[cfg(windows)]
struct WindowsDacl {
    descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
    acl: *mut windows_sys::Win32::Security::ACL,
    protected: bool,
}

#[cfg(windows)]
#[allow(unsafe_code)]
impl Drop for WindowsDacl {
    fn drop(&mut self) {
        // SAFETY: GetNamedSecurityInfo allocated this descriptor with LocalAlloc.
        unsafe {
            windows_sys::Win32::Foundation::LocalFree(self.descriptor);
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn read_windows_dacl(path: &Path) -> Result<WindowsDacl, String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::{
        Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, SE_DACL_PROTECTED,
    };
    let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut result = WindowsDacl {
        descriptor: std::ptr::null_mut(),
        acl: std::ptr::null_mut(),
        protected: false,
    };
    // SAFETY: path is a live terminated UTF-16 buffer, outputs point to live
    // initialized fields, and the descriptor owns the returned DACL storage.
    let code = unsafe {
        GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut result.acl,
            std::ptr::null_mut(),
            &raw mut result.descriptor,
        )
    };
    if code != 0 {
        return Err(format!("cannot preserve Windows DACL ({code})"));
    }
    let mut control = 0;
    let mut revision = 0;
    // SAFETY: descriptor is the live allocation returned by the successful call.
    if unsafe {
        GetSecurityDescriptorControl(result.descriptor, &raw mut control, &raw mut revision)
    } == 0
    {
        return Err("cannot read Windows DACL protection".into());
    }
    result.protected = control & SE_DACL_PROTECTED != 0;
    Ok(result)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn set_windows_dacl(path: &Path, source: &WindowsDacl) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Security::{
        Authorization::{SE_FILE_OBJECT, SetNamedSecurityInfoW},
        DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    let path: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let flags = DACL_SECURITY_INFORMATION
        | if source.protected {
            PROTECTED_DACL_SECURITY_INFORMATION
        } else {
            UNPROTECTED_DACL_SECURITY_INFORMATION
        };
    // SAFETY: both path and the owned descriptor/DACL remain live through this
    // synchronous call; optional owner/group/SACL pointers are explicitly null.
    let code = unsafe {
        SetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            flags,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            source.acl,
            std::ptr::null(),
        )
    };
    if code != 0 {
        return Err(format!("cannot preserve Windows DACL ({code})"));
    }
    Ok(())
}

#[cfg(windows)]
fn copy_windows_dacl(origin: &Path, destination: &Path) -> Result<(), String> {
    set_windows_dacl(destination, &read_windows_dacl(origin)?)
}

#[cfg(unix)]
fn preserve_unix_metadata(origin: &Path, destination: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, chown};
    let source = std::fs::metadata(origin).map_err(|e| e.to_string())?;
    let staged = std::fs::metadata(destination).map_err(|e| e.to_string())?;
    if source.uid() != staged.uid() || source.gid() != staged.gid() {
        chown(destination, Some(source.uid()), Some(source.gid()))
            .map_err(|_| "cannot preserve Unix owner/group; update refused")?;
    }
    let attributes: Vec<_> = xattr::list(origin)
        .map_err(|_| "cannot read extended metadata; update refused")?
        .collect();
    for key in xattr::list(destination).map_err(|_| "cannot inspect staged metadata")? {
        if !attributes.contains(&key) {
            xattr::remove(destination, &key).map_err(|_| "cannot preserve extended metadata")?;
        }
    }
    for key in attributes {
        if let Some(value) =
            xattr::get(origin, &key).map_err(|_| "cannot read extended metadata")?
        {
            xattr::set(destination, &key, &value)
                .map_err(|_| "cannot preserve extended metadata; update refused")?;
        }
    }
    Ok(())
}

fn unchanged(path: &Path, expected: &[u8]) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(format!(
            "{} is not a regular, non-symlink file",
            path.display()
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    if bytes != expected {
        return Err(format!(
            "{} changed since scanning; refused to overwrite",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn commit(root: &Path, changes: Vec<Change>) -> Result<(), Failure> {
    commit_with(root, changes, |_, _, _| Ok(()))
}

#[cfg_attr(not(unix), allow(clippy::unnecessary_wraps))]
fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    std::fs::File::open(path)
        .and_then(|f| f.sync_all())
        .map_err(|e| e.to_string())?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(not(windows))]
fn replace(file: &mut Option<TempPath>, path: &Path) -> Result<(), String> {
    match file.take().unwrap().persist(path) {
        Ok(()) => sync_directory(path.parent().ok_or("replacement has no parent")?),
        Err(e) => {
            let detail = e.error.to_string();
            *file = Some(e.path);
            Err(detail)
        }
    }
}

// Retain Windows ACL/creation metadata instead of MoveFileExW (tempfile's
// persist implementation). Do not bypass permission-merge errors.
#[cfg(windows)]
#[allow(unsafe_code)]
fn replace(file: &mut Option<TempPath>, path: &Path) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{ReplaceFileW, SetFileAttributesW};
    let target: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let source: Vec<_> = file
        .as_ref()
        .unwrap()
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let attributes = std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .file_attributes();
    // SAFETY: both paths are live NUL-terminated UTF-16 buffers, optional
    // pointers are null, and no buffers escape this synchronous call.
    unsafe {
        if SetFileAttributesW(source.as_ptr(), attributes) == 0
            || ReplaceFileW(
                target.as_ptr(),
                source.as_ptr(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                std::ptr::null(),
            ) == 0
        {
            return Err(std::io::Error::last_os_error().to_string());
        }
    }
    file.take(); // The temp path no longer exists after successful replacement.
    Ok(())
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn validate_windows_target(path: &Path) -> Result<(), String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if file
        .metadata()
        .map_err(|e| e.to_string())?
        .permissions()
        .readonly()
    {
        return Err(format!(
            "read-only update target is unsupported: {}",
            path.display()
        ));
    }
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle belongs to the live file; info is a valid writable
    // structure for the duration of this synchronous Win32 call.
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &raw mut info) } == 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if info.nNumberOfLinks > 1 {
        return Err(format!(
            "hard-linked update target is unsupported: {}",
            path.display()
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn commit_with(
    root: &Path,
    mut changes: Vec<Change>,
    mut hook: impl FnMut(Step, usize, &Path) -> Result<(), String>,
) -> Result<(), Failure> {
    let simple = |detail| Failure {
        detail,
        recovery_required: false,
        rolled_back: false,
        committed: false,
    };
    if !pending(root).map_err(|e| simple(e.to_string()))?.is_empty() {
        return Err(simple(
            "pending update receipt exists; inspect its backups before another -u".into(),
        ));
    }
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    if changes.is_empty() {
        return Ok(());
    }
    let mut identities = HashSet::new();
    let canonical_root = std::fs::canonicalize(root).map_err(|e| simple(e.to_string()))?;
    for c in &changes {
        unchanged(&c.path, &c.original).map_err(simple)?;
        #[cfg(windows)]
        validate_windows_target(&c.path).map_err(simple)?;
        let identity = std::fs::canonicalize(&c.path).map_err(|e| simple(e.to_string()))?;
        if !identity.starts_with(&canonical_root) {
            return Err(simple(
                "update target escapes the working directory; run from its project root".into(),
            ));
        }
        if !identities.insert(identity) {
            return Err(simple(format!(
                "duplicate update target: {}",
                c.path.display()
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if std::fs::metadata(&c.path)
                .map_err(|e| simple(e.to_string()))?
                .nlink()
                > 1
            {
                return Err(simple(format!(
                    "hard-linked update target is unsupported: {}",
                    c.path.display()
                )));
            }
        }
    }
    let _locks = target_locks(changes.iter().map(|c| c.path.as_path())).map_err(simple)?;
    if let Some(receipt) = pending_for(changes.iter().map(|c| c.path.as_path()))
        .map_err(simple)?
        .first()
    {
        return Err(simple(format!(
            "pending update receipt {}; recover from its directory",
            receipt.display()
        )));
    }
    let mut staged = Vec::new();
    for (i, change) in changes.into_iter().enumerate() {
        hook(Step::Stage, i, &change.path).map_err(simple)?;
        let parent = change
            .path
            .parent()
            .ok_or_else(|| simple("target has no parent".into()))?;
        let permissions = std::fs::metadata(&change.path)
            .map_err(|e| simple(e.to_string()))?
            .permissions();
        let stage = file(
            parent,
            ".dcu-stage-",
            &change.replacement,
            permissions.clone(),
            &change.path,
        )
        .map_err(simple)?;
        let backup = file(
            parent,
            ".dcu-backup-",
            &change.original,
            permissions,
            &change.path,
        )
        .map_err(simple)?;
        sync_directory(parent).map_err(simple)?;
        staged.push(Staged {
            change,
            stage: Some(stage),
            backup: Some(backup),
            committed: false,
        });
    }
    let mut receipt = Builder::new()
        .prefix(RECEIPT_PREFIX)
        .rand_bytes(0)
        .suffix("active.json")
        .tempfile_in(root)
        .map_err(|e| simple(e.to_string()))?;
    let _receipt_lock = FileLock::acquire(
        receipt
            .as_file()
            .try_clone()
            .map_err(|e| simple(e.to_string()))?,
    )
    .map_err(|e| simple(e.to_string()))?;
    let entries: Vec<_> = staged
        .iter()
        .map(|s| ReceiptEntry {
            path: &s.change.path,
            staged: s.stage.as_ref().unwrap().as_ref(),
            backup: s.backup.as_ref().unwrap().as_ref(),
            original_sha256: digest(&s.change.original),
            replacement_sha256: digest(&s.change.replacement),
        })
        .collect();
    serde_json::to_writer_pretty(
        &mut receipt,
        &serde_json::json!({"schemaVersion":1,"files":entries}),
    )
    .map_err(|e| simple(e.to_string()))?;
    receipt
        .as_file()
        .sync_all()
        .map_err(|e| simple(e.to_string()))?;
    sync_directory(root).map_err(simple)?;
    let markers = target_markers(&staged, receipt.path()).map_err(simple)?;
    for s in &staged {
        unchanged(&s.change.path, &s.change.original).map_err(simple)?;
    }
    let mut failed = None;
    let mut uncertain = Vec::new();
    for (i, s) in staged.iter_mut().enumerate() {
        let mut attempted = false;
        let result = hook(Step::Commit, i, &s.change.path)
            .and_then(|()| unchanged(&s.change.path, &s.change.original))
            .and_then(|()| {
                attempted = true;
                replace(&mut s.stage, &s.change.path)
            });
        match result {
            Ok(()) => s.committed = true,
            Err(e) => {
                if attempted {
                    match std::fs::read(&s.change.path) {
                        Ok(bytes) if bytes == s.change.replacement => s.committed = true,
                        Ok(bytes) if bytes == s.change.original => {}
                        _ => uncertain.push(format!(
                            "{} has uncertain replacement state",
                            s.change.path.display()
                        )),
                    }
                }
                failed = Some(format!("cannot replace {}: {e}", s.change.path.display()));
                break;
            }
        }
    }
    let Some(detail) = failed else {
        if let Err(error) = hook(Step::Finalize, 0, root)
            .and_then(|()| std::fs::remove_file(receipt.path()).map_err(|e| e.to_string()))
        {
            let receipt_path = receipt.path().to_owned();
            let _ = receipt.keep();
            for marker in markers {
                let _ = marker.keep();
            }
            for s in staged {
                if let Some(f) = s.backup {
                    let _ = f.keep();
                }
                if let Some(f) = s.stage {
                    let _ = f.keep();
                }
            }
            return Err(Failure {
                detail: format!(
                    "files committed but finalization failed: {error}; finish recovery at {}",
                    receipt_path.display()
                ),
                committed: true,
                recovery_required: true,
                rolled_back: false,
            });
        }
        let completed = |detail| Failure {
            detail,
            committed: true,
            recovery_required: false,
            rolled_back: false,
        };
        drop(receipt);
        sync_directory(root).map_err(completed)?;
        for marker in markers {
            marker.close().map_err(|e| {
                completed(format!(
                    "files committed but target marker cleanup failed: {e}"
                ))
            })?;
        }
        for target in &mut staged {
            if let Some(backup) = target.backup.take() {
                backup.close().map_err(|e| {
                    completed(format!("files committed but backup cleanup failed: {e}"))
                })?;
            }
            if let Some(stage) = target.stage.take() {
                stage.close().map_err(|e| {
                    completed(format!("files committed but stage cleanup failed: {e}"))
                })?;
            }
            sync_directory(target.change.path.parent().unwrap()).map_err(completed)?;
        }
        return Ok(());
    };
    let rolled_back = staged.iter().any(|s| s.committed);
    let mut recovery = uncertain;
    for (i, s) in staged
        .iter_mut()
        .enumerate()
        .rev()
        .filter(|(_, s)| s.committed)
    {
        // Never replace a concurrent edit made after this transaction's write.
        let result = hook(Step::Rollback, i, &s.change.path)
            .and_then(|()| unchanged(&s.change.path, &s.change.replacement))
            .and_then(|()| replace(&mut s.backup, &s.change.path));
        if let Err(e) = result {
            recovery.push(format!("{}: {e}", s.change.path.display()));
        }
    }
    if recovery.is_empty() {
        receipt.close().map_err(|e| simple(e.to_string()))?;
        sync_directory(root).map_err(simple)?;
        drop(markers);
        return Err(Failure {
            detail: format!("{detail}; all committed files rolled back"),
            recovery_required: false,
            rolled_back,
            committed: false,
        });
    }
    let receipt_path = receipt.path().to_owned();
    let _ = receipt.keep();
    for marker in markers {
        let _ = marker.keep();
    }
    for s in staged {
        if let Some(f) = s.stage {
            let _ = f.keep();
        }
        if let Some(f) = s.backup {
            let _ = f.keep();
        }
    }
    Err(Failure {
        detail: format!(
            "{detail}; recovery required: {}; receipt and original backups: {}",
            recovery.join("; "),
            receipt_path.display()
        ),
        recovery_required: true,
        rolled_back,
        committed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn non_lock_files(root: &Path) -> usize {
        std::fs::read_dir(root)
            .unwrap()
            .filter(|entry| {
                !entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".dcu-lock-")
            })
            .count()
    }

    fn fixture() -> (tempfile::TempDir, Vec<Change>) {
        let dir = tempfile::TempDir::new().unwrap();
        let changes = ["a.toml", "b.toml"]
            .into_iter()
            .map(|name| {
                let path = dir.path().join(name);
                std::fs::write(&path, b"old\r\n").unwrap();
                Change {
                    path,
                    original: b"old\r\n".to_vec(),
                    replacement: b"new\r\n".to_vec(),
                }
            })
            .collect();
        (dir, changes)
    }

    #[test]
    fn untrusted_marker_lock_and_receipt_shapes_are_rejected_before_writes() {
        let (dir, changes) = fixture();
        let path = &changes[0].path;
        let marker = marker_path(path).unwrap();
        std::fs::create_dir(&marker).unwrap();
        assert!(read_marker(&marker).err().unwrap().contains("unsafe"));
        std::fs::remove_dir(&marker).unwrap();
        std::fs::write(&marker, vec![b' '; 16 * 1024 + 1]).unwrap();
        assert!(read_marker(&marker).err().unwrap().contains("unsafe"));
        assert!(
            decode_marker(&vec![b' '; 16 * 1024 + 1])
                .err()
                .unwrap()
                .contains("size limit")
        );
        assert!(
            decode_marker(br#"{"receipt":"relative.json"}"#)
                .err()
                .unwrap()
                .contains("path")
        );
        let lock_path = path.with_file_name(format!(
            ".dcu-lock-{}",
            digest(path.file_name().unwrap().as_encoded_bytes())
        ));
        std::fs::create_dir(&lock_path).unwrap();
        assert!(
            target_locks(std::iter::once(path.as_path()))
                .err()
                .unwrap()
                .contains("unsafe")
        );
        assert!(
            scoped(dir.path(), Path::new("relative"))
                .err()
                .unwrap()
                .contains("escapes")
        );
        assert!(scoped(dir.path(), dir.path()).is_err());
        let directory = dir.path().join("directory");
        std::fs::create_dir(&directory).unwrap();
        assert!(
            scoped(&std::fs::canonicalize(dir.path()).unwrap(), &directory)
                .err()
                .unwrap()
                .contains("regular")
        );
        let first = dir.path().join(".dcu-transaction-first.json");
        let second = dir.path().join(".dcu-transaction-second.json");
        std::fs::write(&first, b"{}").unwrap();
        std::fs::write(&second, b"{}").unwrap();
        assert!(recover(dir.path(), false).unwrap_err().contains("multiple"));
        std::fs::remove_file(&second).unwrap();
        std::fs::write(&first, vec![b' '; 4 * 1024 * 1024 + 1]).unwrap();
        assert!(
            recover(dir.path(), false)
                .unwrap_err()
                .contains("size limit")
        );
        std::fs::write(&first, br#"{"schemaVersion":2,"files":[]}"#).unwrap();
        assert!(recover(dir.path(), false).unwrap_err().contains("schema"));
        assert_eq!(std::fs::read(path).unwrap(), b"old\r\n");
    }

    #[test]
    fn conflicting_pending_marker_cannot_be_attached_to_a_new_transaction() {
        let (dir, changes) = fixture();
        let receipt = dir.path().join(".dcu-transaction-existing.json");
        std::fs::write(&receipt, b"{}").unwrap();
        let marker = marker_path(&changes[0].path).unwrap();
        std::fs::write(
            &marker,
            serde_json::to_vec(&TargetReceipt {
                receipt: receipt.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        let staged = vec![Staged {
            change: changes.into_iter().next().unwrap(),
            stage: None,
            backup: None,
            committed: false,
        }];
        assert!(
            target_markers(&staged, &receipt)
                .err()
                .unwrap()
                .contains("pending update receipt")
        );
    }

    #[test]
    fn recovery_refuses_a_marker_owned_by_a_different_receipt() {
        let dir = crashed_fixture();
        let path = dir.path().join("a.toml");
        let marker = marker_path(&path).unwrap();
        std::fs::write(
            &marker,
            serde_json::to_vec(&TargetReceipt {
                receipt: dir.path().join(".dcu-transaction-other.json"),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(
            recover(dir.path(), false)
                .unwrap_err()
                .contains("different recovery receipt")
        );
        assert_eq!(std::fs::read(path).unwrap(), b"new\r\n");
    }

    #[test]
    fn empty_batches_and_out_of_scope_targets_have_no_side_effects() {
        let (dir, changes) = fixture();
        commit(dir.path(), Vec::new()).unwrap_or_else(|e| panic!("{}", e.detail));
        let other = tempfile::TempDir::new().unwrap();
        assert!(
            commit(other.path(), changes)
                .err()
                .unwrap()
                .detail
                .contains("escapes")
        );
        assert_eq!(non_lock_files(dir.path()), 2);
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_hard_links_and_replacement_failures_keep_the_stage() {
        let (dir, changes) = fixture();
        let path = &changes[0].path;
        let linked = dir.path().join("linked");
        std::fs::hard_link(path, &linked).unwrap();
        assert!(
            scoped(&std::fs::canonicalize(dir.path()).unwrap(), &linked)
                .unwrap_err()
                .contains("hard-linked")
        );
        let mut stage = Some(
            Builder::new()
                .tempfile_in(dir.path())
                .unwrap()
                .into_temp_path(),
        );
        let stage_path = stage.as_ref().unwrap().to_owned();
        let directory = dir.path().join("target-directory");
        std::fs::create_dir(&directory).unwrap();
        assert!(replace(&mut stage, &directory).is_err());
        assert!(stage.is_some());
        assert!(stage_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn unix_metadata_removes_stale_attributes_and_preserves_a_different_group() {
        use std::os::unix::fs::{MetadataExt, chown};
        let (dir, changes) = fixture();
        let origin = &changes[0].path;
        let destination = &changes[1].path;
        let attribute = if cfg!(target_os = "macos") {
            "com.dcu.stale"
        } else {
            "user.dcu.stale"
        };
        xattr::set(destination, attribute, b"stale").unwrap();
        // Root-owned coverage containers can exercise a differing group. Other
        // hosts still validate attribute removal without requiring elevation.
        let metadata = std::fs::metadata(origin).unwrap();
        if metadata.uid() == 0 {
            chown(origin, None, Some(1)).unwrap();
        }
        preserve_unix_metadata(origin, destination).unwrap();
        assert!(xattr::get(destination, attribute).unwrap().is_none());
        assert_eq!(
            std::fs::metadata(origin).unwrap().gid(),
            std::fs::metadata(destination).unwrap().gid()
        );
        assert!(dir.path().is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn completed_lock_is_released_even_if_an_inherited_description_remains_open() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("lock");
        let file = std::fs::File::create(&path).unwrap();
        let inherited = file.try_clone().unwrap();
        let guard = FileLock::acquire(file).unwrap();
        let second = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(fs2::FileExt::try_lock_exclusive(&second).is_err());
        drop(guard);
        fs2::FileExt::try_lock_exclusive(&second).unwrap();
        fs2::FileExt::unlock(&second).unwrap();
        drop(inherited);
    }

    #[test]
    fn successful_batch_preserves_crlf_and_cleans_artifacts() {
        let (dir, changes) = fixture();
        commit(dir.path(), changes).unwrap_or_else(|e| panic!("{}", e.detail));
        assert_eq!(
            std::fs::read(dir.path().join("a.toml")).unwrap(),
            b"new\r\n"
        );
        assert_eq!(non_lock_files(dir.path()), 2);
    }

    #[test]
    fn staging_and_second_commit_failures_leave_originals() {
        for fail_step in [Step::Stage, Step::Commit] {
            let (dir, changes) = fixture();
            let e = commit_with(dir.path(), changes, |step, i, _| {
                if step == fail_step && i == 1 {
                    Err("injected failure".into())
                } else {
                    Ok(())
                }
            })
            .err()
            .unwrap();
            assert!(!e.recovery_required);
            for name in ["a.toml", "b.toml"] {
                assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), b"old\r\n");
            }
            assert_eq!(non_lock_files(dir.path()), 2);
        }
    }

    #[test]
    fn concurrent_edit_is_preserved_and_prior_files_are_rolled_back() {
        let (dir, changes) = fixture();
        let e = commit_with(dir.path(), changes, |step, i, path| {
            if step == Step::Commit && i == 1 {
                std::fs::write(path, b"external").unwrap();
            }
            Ok(())
        })
        .err()
        .unwrap();
        assert!(!e.recovery_required);
        assert_eq!(
            std::fs::read(dir.path().join("a.toml")).unwrap(),
            b"old\r\n"
        );
        assert_eq!(
            std::fs::read(dir.path().join("b.toml")).unwrap(),
            b"external"
        );
    }

    #[test]
    fn rollback_failure_keeps_receipt_and_backup_without_overwriting_external_edit() {
        let (dir, changes) = fixture();
        let e = commit_with(dir.path(), changes, |step, i, path| {
            if step == Step::Commit && i == 1 {
                return Err("commit failed".into());
            }
            if step == Step::Rollback {
                std::fs::write(path, b"external edit").unwrap();
            }
            Ok(())
        })
        .err()
        .unwrap();
        assert!(e.recovery_required);
        assert_eq!(
            std::fs::read(dir.path().join("a.toml")).unwrap(),
            b"external edit"
        );
        let receipts = pending(dir.path()).unwrap();
        assert_eq!(receipts.len(), 1);
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipts[0]).unwrap()).unwrap();
        let backup = receipt["files"][0]["backup"].as_str().unwrap();
        assert_eq!(std::fs::read(backup).unwrap(), b"old\r\n");
        assert!(e.detail.contains(receipts[0].to_str().unwrap()));
    }

    #[test]
    fn duplicate_targets_and_pending_receipts_are_rejected_before_writing() {
        let (dir, mut changes) = fixture();
        changes.push(Change {
            path: changes[0].path.clone(),
            original: b"old\r\n".to_vec(),
            replacement: b"other".to_vec(),
        });
        assert!(
            commit(dir.path(), changes)
                .err()
                .unwrap()
                .detail
                .contains("duplicate")
        );
        std::fs::write(dir.path().join(".dcu-transaction-test.json"), b"{}").unwrap();
        assert!(
            commit(dir.path(), Vec::new())
                .err()
                .unwrap()
                .detail
                .contains("receipt")
        );
    }

    #[test]
    fn another_transaction_in_same_root_cannot_commit_concurrently() {
        let (dir, changes) = fixture();
        commit_with(dir.path(), changes, |step, _, _| {
            if step == Step::Commit {
                assert!(
                    commit(dir.path(), Vec::new())
                        .err()
                        .unwrap()
                        .detail
                        .contains("receipt")
                );
            }
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{}", e.detail));
        assert!(pending(dir.path()).unwrap().is_empty());
    }

    // A child test process exits without running destructors, reproducing
    // forced termination after one replacement. This hook exists only in tests.
    #[test]
    fn forced_termination_child() {
        let Some(root) = std::env::var_os("DCU_TRANSACTION_CRASH_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let changes = ["a.toml", "b.toml"]
            .into_iter()
            .map(|name| Change {
                path: root.join(name),
                original: b"old\r\n".to_vec(),
                replacement: b"new\r\n".to_vec(),
            })
            .collect();
        let _ = commit_with(&root, changes, |step, i, _| {
            if step == Step::Commit && i == 1 {
                std::process::exit(73);
            }
            Ok(())
        });
        panic!("child did not reach forced termination");
    }

    #[test]
    fn forced_termination_leaves_original_backups_and_a_pending_receipt() {
        let (dir, _) = fixture();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transaction::tests::forced_termination_child",
                "--nocapture",
            ])
            .env("DCU_TRANSACTION_CRASH_TEST_ROOT", dir.path())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(73),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            std::fs::read(dir.path().join("a.toml")).unwrap(),
            b"new\r\n"
        );
        assert_eq!(
            std::fs::read(dir.path().join("b.toml")).unwrap(),
            b"old\r\n"
        );
        let receipts = pending(dir.path()).unwrap();
        assert_eq!(receipts.len(), 1);
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&receipts[0]).unwrap()).unwrap();
        for entry in receipt["files"].as_array().unwrap() {
            assert_eq!(
                std::fs::read(entry["backup"].as_str().unwrap()).unwrap(),
                b"old\r\n"
            );
            assert_eq!(entry["original_sha256"], digest(b"old\r\n"));
            assert_eq!(entry["replacement_sha256"], digest(b"new\r\n"));
        }
        assert!(commit(dir.path(), Vec::new()).is_err());
    }

    fn crashed_fixture() -> tempfile::TempDir {
        let (dir, _) = fixture();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "transaction::tests::forced_termination_child",
                "--nocapture",
            ])
            .env("DCU_TRANSACTION_CRASH_TEST_ROOT", dir.path())
            .output()
            .unwrap()
            .status;
        assert_eq!(status.code(), Some(73));
        dir
    }

    #[test]
    fn explicit_recovery_rolls_back_or_finishes_and_is_idempotent() {
        for finish in [false, true] {
            let dir = crashed_fixture();
            assert!(recover(dir.path(), finish).unwrap());
            for name in ["a.toml", "b.toml"] {
                assert_eq!(
                    std::fs::read(dir.path().join(name)).unwrap(),
                    if finish { b"new\r\n" } else { b"old\r\n" }
                );
            }
            assert!(!recover(dir.path(), finish).unwrap());
            assert_eq!(non_lock_files(dir.path()), 2);
        }
    }

    #[test]
    fn recovery_rejects_live_transactions_external_edits_and_corrupt_backups() {
        let (dir, changes) = fixture();
        commit_with(dir.path(), changes, |step, _, _| {
            if step == Step::Commit {
                assert!(recover(dir.path(), false).unwrap_err().contains("active"));
            }
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{}", e.detail));
        let dir = crashed_fixture();
        std::fs::write(dir.path().join("b.toml"), "external edit").unwrap();
        assert!(
            recover(dir.path(), false)
                .unwrap_err()
                .contains("external edit")
        );
        assert_eq!(
            std::fs::read(dir.path().join("a.toml")).unwrap(),
            b"new\r\n"
        );
        assert_eq!(pending(dir.path()).unwrap().len(), 1);
        std::fs::write(dir.path().join("b.toml"), b"old\r\n").unwrap();
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(pending(dir.path()).unwrap().remove(0)).unwrap())
                .unwrap();
        let backup = receipt["files"][0]["backup"].as_str().unwrap();
        std::fs::write(backup, "corrupt").unwrap();
        assert!(recover(dir.path(), false).unwrap_err().contains("corrupt"));
        assert_eq!(
            std::fs::read(dir.path().join("a.toml")).unwrap(),
            b"new\r\n"
        );
    }

    #[test]
    fn recovery_rejects_forged_paths_and_duplicate_targets_before_writing() {
        for duplicate in [false, true] {
            let dir = crashed_fixture();
            let receipt_path = pending(dir.path()).unwrap().remove(0);
            let mut receipt: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
            if duplicate {
                receipt["files"][1]["path"] = receipt["files"][0]["path"].clone();
            } else {
                receipt["files"][0]["backup"] = dir
                    .path()
                    .parent()
                    .unwrap()
                    .join("outside.tmp")
                    .display()
                    .to_string()
                    .into();
            }
            std::fs::write(&receipt_path, serde_json::to_vec(&receipt).unwrap()).unwrap();
            assert!(recover(dir.path(), false).is_err());
            assert_eq!(
                std::fs::read(dir.path().join("a.toml")).unwrap(),
                b"new\r\n"
            );
            assert!(receipt_path.exists());
        }
    }

    #[test]
    fn different_working_directories_share_target_locks() {
        let (dir, changes) = fixture();
        let other = dir.path().parent().unwrap();
        commit_with(dir.path(), changes, |step, _, path| {
            if step == Step::Commit {
                let bytes = std::fs::read(path).unwrap();
                let error = commit(
                    other,
                    vec![Change {
                        path: path.to_owned(),
                        original: bytes,
                        replacement: b"other".to_vec(),
                    }],
                )
                .err()
                .unwrap();
                assert!(error.detail.contains("busy"));
            }
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{}", e.detail));
    }

    #[test]
    fn interrupted_transactions_protect_targets_from_other_working_directories() {
        let dir = crashed_fixture();
        let path = dir.path().join("a.toml");
        assert_eq!(
            pending_for(std::iter::once(path.as_path())).unwrap().len(),
            1
        );
        let original = std::fs::read(&path).unwrap();
        let error = commit(
            dir.path().parent().unwrap(),
            vec![Change {
                path: path.clone(),
                original: original.clone(),
                replacement: b"other".to_vec(),
            }],
        )
        .err()
        .unwrap();
        assert!(error.detail.contains("pending update receipt"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        recover(dir.path(), false).unwrap();
        assert!(
            pending_for(std::iter::once(path.as_path()))
                .unwrap()
                .is_empty()
        );
        commit(
            dir.path().parent().unwrap(),
            vec![Change {
                path,
                original: b"old\r\n".to_vec(),
                replacement: b"other".to_vec(),
            }],
        )
        .unwrap_or_else(|e| panic!("{}", e.detail));
    }

    #[test]
    fn finalization_failures_report_committed_bytes_and_keep_recoverable_evidence() {
        for finish in [false, true] {
            let (dir, changes) = fixture();
            let error = commit_with(dir.path(), changes, |step, _, _| {
                if step == Step::Finalize {
                    Err("cleanup failed".into())
                } else {
                    Ok(())
                }
            })
            .err()
            .unwrap();
            assert!(error.committed);
            assert!(error.recovery_required);
            for name in ["a.toml", "b.toml"] {
                assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), b"new\r\n");
            }
            recover(dir.path(), finish).unwrap();
            for name in ["a.toml", "b.toml"] {
                assert_eq!(
                    std::fs::read(dir.path().join(name)).unwrap(),
                    if finish { b"new\r\n" } else { b"old\r\n" }
                );
            }
            assert_eq!(non_lock_files(dir.path()), 2);
        }
    }

    #[test]
    fn stale_target_markers_are_reclaimed_only_under_target_locks() {
        let (dir, changes) = fixture();
        let marker = marker_path(&changes[0].path).unwrap();
        let receipt = TargetReceipt {
            receipt: dir.path().join(".dcu-transaction-gone.json"),
        };
        std::fs::write(&marker, serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(
            pending_for(changes.iter().map(|c| c.path.as_path()))
                .unwrap()
                .is_empty()
        );
        assert!(marker.exists(), "read-only queries must not clean markers");
        commit(dir.path(), changes).unwrap_or_else(|e| panic!("{}", e.detail));
        assert!(!marker.exists());
        assert_eq!(non_lock_files(dir.path()), 2);
    }

    #[cfg(windows)]
    #[test]
    fn actual_receipt_deletion_failure_preserves_committed_files_and_backups() {
        use std::os::windows::fs::OpenOptionsExt;
        let (dir, changes) = fixture();
        let mut held = None;
        let error = commit_with(dir.path(), changes, |step, _, _| {
            if step == Step::Finalize {
                let path = pending(dir.path()).unwrap().remove(0);
                // Permit existing readers/writers but deny deletion until the
                // external handle closes, producing a real sharing violation.
                held = Some(
                    std::fs::OpenOptions::new()
                        .read(true)
                        .share_mode(3)
                        .open(path)
                        .unwrap(),
                );
            }
            Ok(())
        })
        .err()
        .unwrap();
        assert!(error.committed);
        assert!(error.recovery_required);
        drop(held);
        recover(dir.path(), false).unwrap();
        for name in ["a.toml", "b.toml"] {
            assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), b"old\r\n");
        }
        assert_eq!(non_lock_files(dir.path()), 2);
    }

    #[cfg(windows)]
    #[test]
    fn windows_readonly_and_hardlinked_targets_are_rejected() {
        let (dir, changes) = fixture();
        let path = changes[0].path.clone();
        let original = std::fs::metadata(&path).unwrap().permissions();
        let mut readonly = original.clone();
        readonly.set_readonly(true);
        std::fs::set_permissions(&path, readonly).unwrap();
        let error = commit(dir.path(), changes).err().unwrap();
        std::fs::set_permissions(&path, original).unwrap();
        assert!(error.detail.contains("read-only"));
        std::fs::hard_link(&path, dir.path().join("linked")).unwrap();
        assert!(
            commit(
                dir.path(),
                vec![Change {
                    path,
                    original: b"old\r\n".to_vec(),
                    replacement: Vec::new()
                }]
            )
            .err()
            .unwrap()
            .detail
            .contains("hard-linked")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_sharing_violation_rolls_back_first_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let (dir, changes) = fixture();
        let locked = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&changes[1].path)
            .unwrap();
        let error = commit(dir.path(), changes).err().unwrap();
        drop(locked);
        assert!(error.rolled_back);
        assert!(!error.recovery_required);
        assert_eq!(
            std::fs::read(dir.path().join("a.toml")).unwrap(),
            b"old\r\n"
        );
        assert_eq!(
            std::fs::read(dir.path().join("b.toml")).unwrap(),
            b"old\r\n"
        );
        assert!(pending(dir.path()).unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_replace_preserves_creation_metadata() {
        use std::os::windows::fs::MetadataExt;
        let (dir, changes) = fixture();
        let path = changes[0].path.clone();
        let before = std::fs::metadata(&path).unwrap().creation_time();
        commit(dir.path(), changes).unwrap_or_else(|e| panic!("{}", e.detail));
        assert_eq!(std::fs::metadata(&path).unwrap().creation_time(), before);
    }

    #[cfg(windows)]
    #[test]
    fn windows_stages_backups_and_replacement_retain_protected_dacl() {
        let (dir, changes) = fixture();
        let mut acl = read_windows_dacl(&changes[0].path).unwrap();
        acl.protected = true;
        for change in &changes {
            set_windows_dacl(&change.path, &acl).unwrap();
        }
        commit_with(dir.path(), changes, |step, _, _| {
            if step == Step::Commit {
                for entry in std::fs::read_dir(dir.path()).unwrap().flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with(".dcu-stage-") || name.starts_with(".dcu-backup-") {
                        assert!(read_windows_dacl(&entry.path()).unwrap().protected);
                    }
                }
            }
            Ok(())
        })
        .unwrap_or_else(|e| panic!("{}", e.detail));
        assert!(
            read_windows_dacl(&dir.path().join("a.toml"))
                .unwrap()
                .protected
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_hardlinks_and_unix_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let (dir, changes) = fixture();
        std::fs::set_permissions(&changes[0].path, std::fs::Permissions::from_mode(0o640)).unwrap();
        commit(dir.path(), changes).unwrap_or_else(|e| panic!("{}", e.detail));
        assert_eq!(
            std::fs::metadata(dir.path().join("a.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        let original = dir.path().join("a.toml");
        let linked = dir.path().join("linked");
        symlink(&original, &linked).unwrap();
        assert!(
            commit(
                dir.path(),
                vec![Change {
                    path: linked,
                    original: b"new\r\n".to_vec(),
                    replacement: Vec::new()
                }]
            )
            .is_err()
        );
        std::fs::hard_link(&original, dir.path().join("hard")).unwrap();
        assert!(
            commit(
                dir.path(),
                vec![Change {
                    path: original,
                    original: b"new\r\n".to_vec(),
                    replacement: Vec::new()
                }]
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_replacement_retains_owner_group_and_extended_attributes() {
        use std::os::unix::fs::MetadataExt;
        let (dir, changes) = fixture();
        let path = changes[0].path.clone();
        let attribute = if cfg!(target_os = "macos") {
            "com.dcu.test"
        } else {
            "user.dcu.test"
        };
        xattr::set(&path, attribute, b"preserve me").unwrap();
        let original = std::fs::metadata(&path).unwrap();
        commit(dir.path(), changes).unwrap_or_else(|e| panic!("{}", e.detail));
        let current = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (original.uid(), original.gid()),
            (current.uid(), current.gid())
        );
        assert_eq!(
            xattr::get(&path, attribute).unwrap().unwrap(),
            b"preserve me"
        );
    }
}
