//! Directory-FD based access to Lasper's privileged durable state.

use crate::adapters::error::{NspawnError, Result};
use fs2::FileExt;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

const DIRECTORY_MODE: u32 = 0o700;
const LOCK_MODE: u32 = 0o600;
const MAX_NAME_BYTES: usize = 240;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StateDirectory {
    States,
    Deployments,
}

impl StateDirectory {
    fn name(self) -> &'static str {
        match self {
            Self::States => "states",
            Self::Deployments => "deployments",
        }
    }
}

#[derive(Clone)]
pub(crate) struct TrustedStateRoot {
    inner: Arc<TrustedStateRootInner>,
}

struct TrustedStateRootInner {
    path: PathBuf,
    expected_uid: u32,
    verify_ancestors: bool,
    root: OnceLock<TrustedDirectory>,
    states: OnceLock<TrustedDirectory>,
    deployments: OnceLock<TrustedDirectory>,
}

impl TrustedStateRoot {
    pub(crate) fn production() -> Self {
        Self::new(crate::paths::trusted_state_root(), 0, true)
    }

    fn new(path: PathBuf, expected_uid: u32, verify_ancestors: bool) -> Self {
        Self {
            inner: Arc::new(TrustedStateRootInner {
                path,
                expected_uid,
                verify_ancestors,
                root: OnceLock::new(),
                states: OnceLock::new(),
                deployments: OnceLock::new(),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        Self::new(path, uzers::get_current_uid(), false)
    }

    pub(crate) fn directory(&self, child: StateDirectory) -> Result<TrustedDirectory> {
        let slot = match child {
            StateDirectory::States => &self.inner.states,
            StateDirectory::Deployments => &self.inner.deployments,
        };
        if let Some(directory) = slot.get() {
            return Ok(directory.clone());
        }

        let root = self.open_root()?;
        let directory = root.open_or_create_child(child.name(), DIRECTORY_MODE)?;
        let _ = slot.set(directory.clone());
        Ok(slot.get().cloned().unwrap_or(directory))
    }

    fn open_root(&self) -> Result<TrustedDirectory> {
        if let Some(root) = self.inner.root.get() {
            return Ok(root.clone());
        }
        let root = open_or_create_absolute_directory(
            &self.inner.path,
            self.inner.expected_uid,
            self.inner.verify_ancestors,
        )?;
        let _ = self.inner.root.set(root.clone());
        Ok(self.inner.root.get().cloned().unwrap_or(root))
    }
}

impl std::fmt::Debug for TrustedStateRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrustedStateRoot")
            .field("path", &self.inner.path)
            .field("expected_uid", &self.inner.expected_uid)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub(crate) struct TrustedDirectory {
    file: Arc<File>,
    path: Arc<PathBuf>,
    expected_uid: u32,
}

pub(crate) struct TrustedFile {
    pub(crate) bytes: Vec<u8>,
    pub(crate) uid: u32,
    pub(crate) mode: u32,
}

impl TrustedDirectory {
    fn new(file: File, path: PathBuf, expected_uid: u32) -> Result<Self> {
        validate_directory(&file, &path, expected_uid)?;
        Ok(Self {
            file: Arc::new(file),
            path: Arc::new(path),
            expected_uid,
        })
    }

    fn open_or_create_child(&self, name: &str, mode: u32) -> Result<Self> {
        let name = validate_name(name)?;
        let file = open_or_create_directory_at(self.file.as_raw_fd(), &name, mode, &self.path)?;
        Self::new(
            file,
            self.path.join(name.to_str().unwrap()),
            self.expected_uid,
        )
    }

    pub(crate) fn expected_uid(&self) -> u32 {
        self.expected_uid
    }

    pub(crate) fn entry_names(&self) -> Result<Vec<String>> {
        let current = c".";
        let directory_fd = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                current.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if directory_fd < 0 {
            return Err(NspawnError::Io(
                (*self.path).clone(),
                std::io::Error::last_os_error(),
            ));
        }
        let directory = unsafe { libc::fdopendir(directory_fd) };
        if directory.is_null() {
            let error = std::io::Error::last_os_error();
            unsafe { libc::close(directory_fd) };
            return Err(NspawnError::Io((*self.path).clone(), error));
        }

        let mut names = Vec::new();
        loop {
            unsafe { *libc::__errno_location() = 0 };
            let entry = unsafe { libc::readdir(directory) };
            if entry.is_null() {
                let error = std::io::Error::last_os_error();
                unsafe { libc::closedir(directory) };
                return if error.raw_os_error().unwrap_or(0) == 0 {
                    Ok(names)
                } else {
                    Err(NspawnError::Io((*self.path).clone(), error))
                };
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            if let Ok(name) = std::str::from_utf8(name) {
                names.push(name.to_owned());
            }
        }
    }

    pub(crate) fn read_bounded(&self, name: &str, max_bytes: usize) -> Result<Option<TrustedFile>> {
        let name = validate_name(name)?;
        let mut file = match open_file_at(
            self.file.as_raw_fd(),
            &name,
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            0,
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(map_open_error(&self.path, &name, error)),
        };
        let metadata = file
            .metadata()
            .map_err(|error| NspawnError::Io(self.path.join(name.to_str().unwrap()), error))?;
        if !metadata.file_type().is_file() {
            return Err(NspawnError::Validation(format!(
                "trusted state entry is not a regular file: {}",
                self.path.join(name.to_str().unwrap()).display()
            )));
        }
        if metadata.len() > max_bytes as u64 {
            return Err(NspawnError::Validation(format!(
                "trusted state entry exceeds {max_bytes} bytes: {}",
                self.path.join(name.to_str().unwrap()).display()
            )));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| NspawnError::Io(self.path.join(name.to_str().unwrap()), error))?;
        if bytes.len() > max_bytes {
            return Err(NspawnError::Validation(format!(
                "trusted state entry exceeds {max_bytes} bytes: {}",
                self.path.join(name.to_str().unwrap()).display()
            )));
        }
        Ok(Some(TrustedFile {
            bytes,
            uid: metadata.uid(),
            mode: metadata.permissions().mode() & 0o7777,
        }))
    }

    pub(crate) fn with_exclusive_lock<T>(
        &self,
        target: &str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.with_exclusive_lock_inner(target, false, operation)
    }

    pub(crate) fn with_exclusive_lock_and_cleanup<T>(
        &self,
        target: &str,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        self.with_exclusive_lock_inner(target, true, operation)
    }

    fn with_exclusive_lock_inner<T>(
        &self,
        target: &str,
        cleanup_if_absent: bool,
        operation: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let target = validate_name(target)?;
        let lock_name = CString::new(format!(".{}.lock", target.to_string_lossy()))
            .map_err(|_| NspawnError::Validation("trusted state lock name is invalid".into()))?;
        let _lock = self.acquire_stable_lock(&lock_name, true)?.ok_or_else(|| {
            NspawnError::Runtime("trusted state lock unexpectedly disappeared".into())
        })?;
        let result = operation();
        if cleanup_if_absent && result.is_ok() && !self.entry_exists(&target)? {
            unlink_at(self.file.as_raw_fd(), &lock_name).map_err(|error| {
                NspawnError::Io(self.path.join(lock_name.to_str().unwrap()), error)
            })?;
            self.file
                .sync_all()
                .map_err(|error| NspawnError::Io((*self.path).clone(), error))?;
        }
        result
    }

    pub(crate) fn write_atomic(&self, name: &str, bytes: &[u8], mode: u32) -> Result<()> {
        let name = validate_name(name)?;
        self.reject_non_regular_target(&name)?;

        let temporary = CString::new(format!(
            ".{}.{}.tmp",
            name.to_string_lossy(),
            uuid::Uuid::new_v4().simple()
        ))
        .expect("generated trusted state temporary name contains no NUL");
        let mut file = open_file_at(
            self.file.as_raw_fd(),
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            mode,
        )
        .map_err(|error| map_open_error(&self.path, &temporary, error))?;

        let result = (|| {
            file.set_permissions(std::fs::Permissions::from_mode(mode))
                .map_err(|error| {
                    NspawnError::Io(self.path.join(temporary.to_str().unwrap()), error)
                })?;
            file.write_all(bytes).map_err(|error| {
                NspawnError::Io(self.path.join(temporary.to_str().unwrap()), error)
            })?;
            file.sync_all().map_err(|error| {
                NspawnError::Io(self.path.join(temporary.to_str().unwrap()), error)
            })?;
            rename_at(self.file.as_raw_fd(), &temporary, &name)
                .map_err(|error| NspawnError::Io(self.path.join(name.to_str().unwrap()), error))?;
            self.file
                .sync_all()
                .map_err(|error| NspawnError::Io((*self.path).clone(), error))
        })();

        if result.is_err() {
            let _ = unlink_at(self.file.as_raw_fd(), &temporary);
        }
        result
    }

    pub(crate) fn remove_with_lock(&self, name: &str) -> Result<()> {
        self.with_exclusive_lock_and_cleanup(name, || self.remove_unlocked(name))
    }

    pub(crate) fn remove_unlocked(&self, name: &str) -> Result<()> {
        let name = validate_name(name)?;
        self.reject_non_regular_target(&name)?;
        match unlink_at(self.file.as_raw_fd(), &name) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(NspawnError::Io(
                    self.path.join(name.to_str().unwrap()),
                    error,
                ))
            }
        }
        self.file
            .sync_all()
            .map_err(|error| NspawnError::Io((*self.path).clone(), error))
    }

    fn entry_exists(&self, name: &CStr) -> Result<bool> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
        {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(NspawnError::Io(
                self.path.join(name.to_str().unwrap()),
                error,
            ))
        }
    }

    fn reject_non_regular_target(&self, name: &CStr) -> Result<()> {
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::fstatat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if result == 0 {
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                return Err(NspawnError::Validation(format!(
                    "trusted state target is not a regular file: {}",
                    self.path.join(name.to_str().unwrap()).display()
                )));
            }
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(NspawnError::Io(
                self.path.join(name.to_str().unwrap()),
                error,
            ))
        }
    }

    fn acquire_stable_lock(&self, name: &CStr, create: bool) -> Result<Option<File>> {
        loop {
            let mut flags = libc::O_RDWR | libc::O_CLOEXEC | libc::O_NOFOLLOW;
            if create {
                flags |= libc::O_CREAT;
            }
            let file = match open_file_at(self.file.as_raw_fd(), name, flags, LOCK_MODE) {
                Ok(file) => file,
                Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None)
                }
                Err(error) => return Err(map_open_error(&self.path, name, error)),
            };
            let metadata = file
                .metadata()
                .map_err(|error| NspawnError::Io(self.path.join(name.to_str().unwrap()), error))?;
            if !metadata.file_type().is_file()
                || metadata.uid() != self.expected_uid
                || metadata.permissions().mode() & 0o022 != 0
            {
                return Err(NspawnError::Validation(format!(
                    "trusted state lock has unsafe ownership or mode: {}",
                    self.path.join(name.to_str().unwrap()).display()
                )));
            }
            file.set_permissions(std::fs::Permissions::from_mode(LOCK_MODE))
                .map_err(|error| NspawnError::Io(self.path.join(name.to_str().unwrap()), error))?;
            file.lock_exclusive()
                .map_err(|error| NspawnError::Io(self.path.join(name.to_str().unwrap()), error))?;
            if same_opened_entry(&file, self.file.as_raw_fd(), name)? {
                return Ok(Some(file));
            }
            let _ = FileExt::unlock(&file);
        }
    }
}

fn open_or_create_absolute_directory(
    path: &Path,
    expected_uid: u32,
    verify_ancestors: bool,
) -> Result<TrustedDirectory> {
    if !path.is_absolute() {
        return Err(NspawnError::Validation(format!(
            "trusted state root must be absolute: {}",
            path.display()
        )));
    }
    let components = path
        .components()
        .filter_map(|component| match component {
            Component::RootDir => None,
            Component::Normal(component) => Some(Ok(component.to_os_string())),
            _ => Some(Err(NspawnError::Validation(format!(
                "trusted state root contains an unsafe component: {}",
                path.display()
            )))),
        })
        .collect::<Result<Vec<_>>>()?;

    let mut current_path = PathBuf::from("/");
    let mut current = open_directory(Path::new("/"))
        .map_err(|error| NspawnError::Io(current_path.clone(), error))?;
    if verify_ancestors {
        validate_directory(&current, &current_path, expected_uid)?;
    }
    for component in components {
        let name = CString::new(component.as_bytes())
            .map_err(|_| NspawnError::Validation("trusted state path contains NUL".into()))?;
        current_path.push(&component);
        current =
            open_or_create_directory_at(current.as_raw_fd(), &name, DIRECTORY_MODE, &current_path)?;
        if verify_ancestors || current_path == path {
            validate_directory(&current, &current_path, expected_uid)?;
        }
    }
    TrustedDirectory::new(current, path.to_path_buf(), expected_uid)
}

fn open_or_create_directory_at(
    parent: RawFd,
    name: &CStr,
    mode: u32,
    display_path: &Path,
) -> Result<File> {
    match open_file_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        0,
    ) {
        Ok(file) => Ok(file),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let created = unsafe { libc::mkdirat(parent, name.as_ptr(), mode) };
            if created != 0 {
                let mkdir_error = std::io::Error::last_os_error();
                if mkdir_error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(NspawnError::Io(display_path.to_path_buf(), mkdir_error));
                }
            }
            open_file_at(
                parent,
                name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                0,
            )
            .map_err(|error| NspawnError::Io(display_path.to_path_buf(), error))
        }
        Err(error) => Err(map_open_error(display_path, name, error)),
    }
}

fn validate_directory(file: &File, path: &Path, expected_uid: u32) -> Result<()> {
    let metadata = file
        .metadata()
        .map_err(|error| NspawnError::Io(path.to_path_buf(), error))?;
    if !metadata.file_type().is_dir()
        || metadata.uid() != expected_uid
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(NspawnError::Validation(format!(
            "trusted state directory must be owned by uid {expected_uid} and not group/world writable: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<CString> {
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || Path::new(name).components().count() != 1
        || !matches!(
            Path::new(name).components().next(),
            Some(Component::Normal(_))
        )
    {
        return Err(NspawnError::Validation(format!(
            "trusted state filename is invalid: {name:?}"
        )));
    }
    CString::new(name)
        .map_err(|_| NspawnError::Validation("trusted state filename contains NUL".into()))
}

fn open_directory(path: &Path) -> std::io::Result<File> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_file_at(parent: RawFd, name: &CStr, flags: i32, mode: u32) -> std::io::Result<File> {
    let fd = unsafe { libc::openat(parent, name.as_ptr(), flags, mode) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn rename_at(parent: RawFd, old: &CStr, new: &CStr) -> std::io::Result<()> {
    if unsafe { libc::renameat(parent, old.as_ptr(), parent, new.as_ptr()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn unlink_at(parent: RawFd, name: &CStr) -> std::io::Result<()> {
    if unsafe { libc::unlinkat(parent, name.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn same_opened_entry(file: &File, parent: RawFd, name: &CStr) -> Result<bool> {
    let opened = file.metadata().map_err(NspawnError::GenericIo)?;
    let mut current: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe {
        libc::fstatat(
            parent,
            name.as_ptr(),
            &mut current,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(false)
        } else {
            Err(NspawnError::GenericIo(error))
        };
    }
    Ok(opened.dev() == current.st_dev && opened.ino() == current.st_ino)
}

fn map_open_error(parent: &Path, name: &CStr, error: std::io::Error) -> NspawnError {
    let path = name
        .to_str()
        .map(|name| parent.join(name))
        .unwrap_or_else(|_| parent.to_path_buf());
    if error.raw_os_error() == Some(libc::ELOOP) {
        NspawnError::Validation(format!(
            "trusted state path must not be a symlink: {}",
            path.display()
        ))
    } else {
        NspawnError::Io(path, error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atomic_files_are_bounded_and_lock_cleanup_is_scoped() {
        let temporary = tempfile::tempdir().unwrap();
        let root = TrustedStateRoot::for_test(temporary.path().join("lasper"));
        let states = root.directory(StateDirectory::States).unwrap();

        states
            .with_exclusive_lock("machine.json", || {
                states.write_atomic("machine.json", br#"{"version":1}"#, 0o600)
            })
            .unwrap();
        let file = states.read_bounded("machine.json", 64).unwrap().unwrap();
        assert_eq!(file.bytes, br#"{"version":1}"#);
        assert_eq!(file.uid, uzers::get_current_uid());
        assert_eq!(file.mode, 0o600);

        states.remove_with_lock("machine.json").unwrap();
        assert!(states.read_bounded("machine.json", 64).unwrap().is_none());
        assert!(!temporary
            .path()
            .join("lasper/states/.machine.json.lock")
            .exists());
    }

    #[test]
    fn rejects_names_and_symlink_targets_outside_the_directory_fd() {
        let temporary = tempfile::tempdir().unwrap();
        let root = TrustedStateRoot::for_test(temporary.path().join("lasper"));
        let states = root.directory(StateDirectory::States).unwrap();
        assert!(states.read_bounded("../escape", 64).is_err());

        std::os::unix::fs::symlink("/etc/passwd", temporary.path().join("lasper/states/link"))
            .unwrap();
        assert!(states.read_bounded("link", 64).is_err());
        assert!(states.write_atomic("link", b"no", 0o600).is_err());
    }

    #[test]
    fn repeated_directory_listing_uses_an_independent_offset() {
        let temporary = tempfile::tempdir().unwrap();
        let root = TrustedStateRoot::for_test(temporary.path().join("lasper"));
        let deployments = root.directory(StateDirectory::Deployments).unwrap();
        deployments
            .write_atomic("deployment-one.json", b"{}", 0o600)
            .unwrap();

        let first = deployments.entry_names().unwrap();
        let second = deployments.entry_names().unwrap();

        assert_eq!(first, second);
        assert!(first.contains(&"deployment-one.json".to_string()));
    }

    #[test]
    fn rejects_an_unsafe_trusted_root_even_for_an_injected_test_path() {
        let temporary = tempfile::tempdir().unwrap();
        let root_path = temporary.path().join("unsafe");
        std::fs::create_dir(&root_path).unwrap();
        std::fs::set_permissions(&root_path, std::fs::Permissions::from_mode(0o777)).unwrap();
        let root = TrustedStateRoot::for_test(root_path);
        assert!(root.directory(StateDirectory::States).is_err());
    }
}
