/*!
Implementation of a virtual filesystem with a configurable backend and file
watching.

memofs is currently an unstable minimum viable library. Its primary consumer is
[Rojo](https://github.com/rojo-rbx/rojo), a build system for Roblox.

## Current Features
* API similar to `std::fs`
* Configurable backends
    * `StdBackend`, which uses `std::fs` and the `notify` crate
    * `NoopBackend`, which always throws errors
    * `InMemoryFs`, a simple in-memory filesystem useful for testing

## Future Features
* Hash-based hierarchical memoization keys (hence the name)
* Configurable caching (write-through, write-around, write-back)
*/

mod in_memory_fs;
mod noop_backend;
mod snapshot;
mod std_backend;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::{io, str};

pub use in_memory_fs::InMemoryFs;
pub use noop_backend::NoopBackend;
pub use snapshot::VfsSnapshot;
pub use std_backend::StdBackend;

mod sealed {
    use super::*;

    /// Sealing trait for VfsBackend.
    pub trait Sealed {}

    impl Sealed for NoopBackend {}
    impl Sealed for StdBackend {}
    impl Sealed for InMemoryFs {}
}

/// Trait that transforms `io::Result<T>` into `io::Result<Option<T>>`.
///
/// `Ok(None)` takes the place of IO errors whose `io::ErrorKind` is `NotFound`.
pub trait IoResultExt<T> {
    fn with_not_found(self) -> io::Result<Option<T>>;
}

impl<T> IoResultExt<T> for io::Result<T> {
    fn with_not_found(self) -> io::Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(err) => {
                if err.kind() == io::ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(err)
                }
            }
        }
    }
}

/// Backend that can be used to create a `Vfs`.
///
/// This trait is sealed and cannot not be implemented outside this crate.
pub trait VfsBackend: sealed::Sealed + Send + 'static {
    fn read(&mut self, path: &Path) -> io::Result<Vec<u8>>;
    fn write(&mut self, path: &Path, data: &[u8]) -> io::Result<()>;
    fn exists(&mut self, path: &Path) -> io::Result<bool>;
    fn read_dir(&mut self, path: &Path) -> io::Result<ReadDir>;
    fn create_dir(&mut self, path: &Path) -> io::Result<()>;
    fn create_dir_all(&mut self, path: &Path) -> io::Result<()>;
    fn metadata(&mut self, path: &Path) -> io::Result<Metadata>;
    fn remove_file(&mut self, path: &Path) -> io::Result<()>;
    fn remove_dir_all(&mut self, path: &Path) -> io::Result<()>;
    fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf>;

    fn event_receiver(&self) -> crossbeam_channel::Receiver<VfsEvent>;
    fn watch(&mut self, path: &Path) -> io::Result<()>;
    fn unwatch(&mut self, path: &Path) -> io::Result<()>;
}

/// Vfs equivalent to [`std::fs::DirEntry`][std::fs::DirEntry].
///
/// [std::fs::DirEntry]: https://doc.rust-lang.org/stable/std/fs/struct.DirEntry.html
pub struct DirEntry {
    pub(crate) path: PathBuf,
}

impl DirEntry {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Vfs equivalent to [`std::fs::ReadDir`][std::fs::ReadDir].
///
/// [std::fs::ReadDir]: https://doc.rust-lang.org/stable/std/fs/struct.ReadDir.html
pub struct ReadDir {
    pub(crate) inner: Box<dyn Iterator<Item = io::Result<DirEntry>>>,
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next()
    }
}

/// Vfs equivalent to [`std::fs::Metadata`][std::fs::Metadata].
///
/// [std::fs::Metadata]: https://doc.rust-lang.org/stable/std/fs/struct.Metadata.html
#[derive(Debug)]
pub struct Metadata {
    pub(crate) is_file: bool,
}

impl Metadata {
    pub fn is_file(&self) -> bool {
        self.is_file
    }

    pub fn is_dir(&self) -> bool {
        !self.is_file
    }
}

/// Represents an event that a filesystem can raise that might need to be
/// handled.
#[derive(Debug)]
#[non_exhaustive]
pub enum VfsEvent {
    Create(PathBuf),
    Write(PathBuf),
    Remove(PathBuf),
    Rescan,
}

/// Contains implementation details of the Vfs, wrapped by `Vfs` and `VfsLock`,
/// the public interfaces to this type.
struct VfsInner {
    backend: Box<dyn VfsBackend>,
    watch_enabled: bool,
}

impl VfsInner {
    fn read<P: AsRef<Path>>(&mut self, path: P) -> io::Result<Arc<Vec<u8>>> {
        let path = path.as_ref();
        let contents = self.backend.read(path)?;

        if self.watch_enabled {
            self.backend.watch(path)?;
        }

        Ok(Arc::new(contents))
    }

    fn read_to_string<P: AsRef<Path>>(&mut self, path: P) -> io::Result<Arc<String>> {
        let path = path.as_ref();
        let contents = self.backend.read(path)?;

        if self.watch_enabled {
            self.backend.watch(path)?;
        }

        let contents_str = str::from_utf8(&contents).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("File was not valid UTF-8: {}", path.display()),
            )
        })?;

        Ok(Arc::new(contents_str.into()))
    }

    fn exists<P: AsRef<Path>>(&mut self, path: P) -> io::Result<bool> {
        let path = path.as_ref();
        self.backend.exists(path)
    }

    fn write<P: AsRef<Path>, C: AsRef<[u8]>>(&mut self, path: P, contents: C) -> io::Result<()> {
        let path = path.as_ref();
        let contents = contents.as_ref();
        self.backend.write(path, contents)
    }

    fn read_dir<P: AsRef<Path>>(&mut self, path: P) -> io::Result<ReadDir> {
        let path = path.as_ref();
        let dir = self.backend.read_dir(path)?;

        if self.watch_enabled {
            self.backend.watch(path)?;
        }

        Ok(dir)
    }

    fn create_dir<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        self.backend.create_dir(path)
    }

    fn create_dir_all<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        self.backend.create_dir_all(path)
    }

    fn remove_file<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        let _ = self.backend.unwatch(path);
        self.backend.remove_file(path)
    }

    fn remove_dir_all<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        let _ = self.backend.unwatch(path);
        self.backend.remove_dir_all(path)
    }

    fn metadata<P: AsRef<Path>>(&mut self, path: P) -> io::Result<Metadata> {
        let path = path.as_ref();
        self.backend.metadata(path)
    }

    fn canonicalize<P: AsRef<Path>>(&mut self, path: P) -> io::Result<PathBuf> {
        let path = path.as_ref();
        self.backend.canonicalize(path)
    }

    fn normalize<P: AsRef<Path>>(&mut self, path: P) -> PathBuf {
        let path = path.as_ref();

        if let Ok(normalized) = self.backend.canonicalize(path) {
            return normalized;
        }

        // Walk up toward the root, remembering the components we skip, until we
        // reach an ancestor that exists and can be canonicalized.
        let mut suffix = Vec::new();
        let mut current = path;
        loop {
            match (current.parent(), current.file_name()) {
                (Some(parent), Some(file_name)) => {
                    suffix.push(file_name);
                    if let Ok(mut normalized) = self.backend.canonicalize(parent) {
                        normalized.extend(suffix.iter().rev());
                        return normalized;
                    }
                    current = parent;
                }
                // We reached a prefix/root (e.g. / or C:\) without finding an
                // existing ancestor, or hit a component (like `..`) that can't
                // be reattached. Nothing left to normalize against, so use the
                // path as given.
                _ => return path.to_path_buf(),
            }
        }
    }

    fn event_receiver(&self) -> crossbeam_channel::Receiver<VfsEvent> {
        self.backend.event_receiver()
    }

    fn commit_event(&mut self, event: &VfsEvent) -> io::Result<()> {
        if let VfsEvent::Remove(path) = event {
            let _ = self.backend.unwatch(path);
        }

        Ok(())
    }
}

/// A virtual filesystem with a configurable backend.
///
/// All operations on the Vfs take a lock on an internal backend. For performing
/// large batches of operations, it might be more performant to call `lock()`
/// and use [`VfsLock`](struct.VfsLock.html) instead.
pub struct Vfs {
    inner: Mutex<VfsInner>,
}

impl Vfs {
    /// Creates a new `Vfs` with the default backend, `StdBackend`.
    ///
    /// Returns an error if the filesystem watcher could not be initialized,
    /// which can happen in restricted or sandboxed environments.
    pub fn new_default() -> io::Result<Self> {
        Ok(Self::new(StdBackend::new()?))
    }

    /// Creates a new `Vfs` with the given backend.
    pub fn new<B: VfsBackend>(backend: B) -> Self {
        let lock = VfsInner {
            backend: Box::new(backend),
            watch_enabled: true,
        };

        Self {
            inner: Mutex::new(lock),
        }
    }

    /// Manually lock the Vfs, useful for large batches of operations.
    pub fn lock(&self) -> VfsLock<'_> {
        VfsLock {
            inner: self.inner.lock().unwrap(),
        }
    }

    /// Turns automatic file watching on or off. Enabled by default.
    ///
    /// Turning off file watching may be useful for single-use cases, especially
    /// on platforms like macOS where registering file watches has significant
    /// performance cost.
    pub fn set_watch_enabled(&self, enabled: bool) {
        let mut inner = self.inner.lock().unwrap();
        inner.watch_enabled = enabled;
    }

    /// Read a file from the VFS, or the underlying backend if it isn't
    /// resident.
    ///
    /// Roughly equivalent to [`std::fs::read`][std::fs::read].
    ///
    /// [std::fs::read]: https://doc.rust-lang.org/stable/std/fs/fn.read.html
    #[inline]
    pub fn read<P: AsRef<Path>>(&self, path: P) -> io::Result<Arc<Vec<u8>>> {
        let path = path.as_ref();
        self.inner.lock().unwrap().read(path)
    }

    /// Read a file from the VFS (or from the underlying backend if it isn't
    /// resident) into a string.
    ///
    /// Roughly equivalent to [`std::fs::read_to_string`][std::fs::read_to_string].
    ///
    /// [std::fs::read_to_string]: https://doc.rust-lang.org/stable/std/fs/fn.read_to_string.html
    #[inline]
    pub fn read_to_string<P: AsRef<Path>>(&self, path: P) -> io::Result<Arc<String>> {
        let path = path.as_ref();
        self.inner.lock().unwrap().read_to_string(path)
    }

    /// Read a file from the VFS (or the underlying backend if it isn't
    /// resident) into a string, and normalize its line endings to LF.
    ///
    /// Roughly equivalent to [`std::fs::read_to_string`][std::fs::read_to_string], but also performs
    /// line ending normalization.
    ///
    /// [std::fs::read_to_string]: https://doc.rust-lang.org/stable/std/fs/fn.read_to_string.html
    #[inline]
    pub fn read_to_string_lf_normalized<P: AsRef<Path>>(&self, path: P) -> io::Result<Arc<String>> {
        let path = path.as_ref();
        let contents = self.inner.lock().unwrap().read_to_string(path)?;

        Ok(contents.replace("\r\n", "\n").into())
    }

    /// Write a file to the VFS and the underlying backend.
    ///
    /// Roughly equivalent to [`std::fs::write`][std::fs::write].
    ///
    /// [std::fs::write]: https://doc.rust-lang.org/stable/std/fs/fn.write.html
    #[inline]
    pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(&self, path: P, contents: C) -> io::Result<()> {
        let path = path.as_ref();
        let contents = contents.as_ref();
        self.inner.lock().unwrap().write(path, contents)
    }

    /// Read all of the children of a directory.
    ///
    /// Roughly equivalent to [`std::fs::read_dir`][std::fs::read_dir].
    ///
    /// [std::fs::read_dir]: https://doc.rust-lang.org/stable/std/fs/fn.read_dir.html
    #[inline]
    pub fn read_dir<P: AsRef<Path>>(&self, path: P) -> io::Result<ReadDir> {
        let path = path.as_ref();
        self.inner.lock().unwrap().read_dir(path)
    }

    /// Return whether the given path exists.
    ///
    /// Roughly equivalent to [`std::fs::exists`][std::fs::exists].
    ///
    /// [std::fs::exists]: https://doc.rust-lang.org/stable/std/fs/fn.exists.html
    #[inline]
    pub fn exists<P: AsRef<Path>>(&self, path: P) -> io::Result<bool> {
        let path = path.as_ref();
        self.inner.lock().unwrap().exists(path)
    }

    /// Creates a directory at the provided location.
    ///
    /// Roughly equivalent to [`std::fs::create_dir`][std::fs::create_dir].
    /// Similiar to that function, this function will fail if the parent of the
    /// path does not exist.
    ///
    /// [std::fs::create_dir]: https://doc.rust-lang.org/stable/std/fs/fn.create_dir.html
    #[inline]
    pub fn create_dir<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        self.inner.lock().unwrap().create_dir(path)
    }

    /// Creates a directory at the provided location, recursively creating
    /// all parent components if they are missing.
    ///
    /// Roughly equivalent to [`std::fs::create_dir_all`][std::fs::create_dir_all].
    ///
    /// [std::fs::create_dir_all]: https://doc.rust-lang.org/stable/std/fs/fn.create_dir_all.html
    #[inline]
    pub fn create_dir_all<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        self.inner.lock().unwrap().create_dir_all(path)
    }

    /// Remove a file.
    ///
    /// Roughly equivalent to [`std::fs::remove_file`][std::fs::remove_file].
    ///
    /// [std::fs::remove_file]: https://doc.rust-lang.org/stable/std/fs/fn.remove_file.html
    #[inline]
    pub fn remove_file<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        self.inner.lock().unwrap().remove_file(path)
    }

    /// Remove a directory and all of its descendants.
    ///
    /// Roughly equivalent to [`std::fs::remove_dir_all`][std::fs::remove_dir_all].
    ///
    /// [std::fs::remove_dir_all]: https://doc.rust-lang.org/stable/std/fs/fn.remove_dir_all.html
    #[inline]
    pub fn remove_dir_all<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        self.inner.lock().unwrap().remove_dir_all(path)
    }

    /// Query metadata about the given path.
    ///
    /// Roughly equivalent to [`std::fs::metadata`][std::fs::metadata].
    ///
    /// [std::fs::metadata]: https://doc.rust-lang.org/stable/std/fs/fn.metadata.html
    #[inline]
    pub fn metadata<P: AsRef<Path>>(&self, path: P) -> io::Result<Metadata> {
        let path = path.as_ref();
        self.inner.lock().unwrap().metadata(path)
    }

    /// Normalize a path via the underlying backend.
    ///
    /// Roughly equivalent to [`std::fs::canonicalize`][std::fs::canonicalize]. Relative paths are
    /// resolved against the backend's current working directory (if applicable) and errors are
    /// surfaced directly from the backend.
    ///
    /// [std::fs::canonicalize]: https://doc.rust-lang.org/stable/std/fs/fn.canonicalize.html
    #[inline]
    pub fn canonicalize<P: AsRef<Path>>(&self, path: P) -> io::Result<PathBuf> {
        let path = path.as_ref();
        self.inner.lock().unwrap().canonicalize(path)
    }

    /// Canonicalize a path leniently: like [`Vfs::canonicalize`], but usable on
    /// paths that may no longer exist.
    ///
    /// This method canonicalizes the deepest ancestor that still exists and
    /// reattaches the missing components. Canonicalizing a directory and then
    /// joining a name onto it gives the same result as canonicalizing that name
    /// directly, so for a removed file this reproduces the canonical form the
    /// path had while it existed.
    ///
    /// If no ancestor of the path exists, the path is returned unchanged.
    #[inline]
    pub fn normalize<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        let path = path.as_ref();
        self.inner.lock().unwrap().normalize(path)
    }

    /// Retrieve a handle to the event receiver for this `Vfs`.
    #[inline]
    pub fn event_receiver(&self) -> crossbeam_channel::Receiver<VfsEvent> {
        self.inner.lock().unwrap().event_receiver()
    }

    /// Commit an event to this `Vfs`.
    #[inline]
    pub fn commit_event(&self, event: &VfsEvent) -> io::Result<()> {
        self.inner.lock().unwrap().commit_event(event)
    }
}

/// A locked handle to a [`Vfs`](struct.Vfs.html), created by `Vfs::lock`.
///
/// Implements roughly the same API as [`Vfs`](struct.Vfs.html).
pub struct VfsLock<'a> {
    inner: MutexGuard<'a, VfsInner>,
}

impl VfsLock<'_> {
    /// Turns automatic file watching on or off. Enabled by default.
    ///
    /// Turning off file watching may be useful for single-use cases, especially
    /// on platforms like macOS where registering file watches has significant
    /// performance cost.
    pub fn set_watch_enabled(&mut self, enabled: bool) {
        self.inner.watch_enabled = enabled;
    }

    /// Read a file from the VFS, or the underlying backend if it isn't
    /// resident.
    ///
    /// Roughly equivalent to [`std::fs::read`][std::fs::read].
    ///
    /// [std::fs::read]: https://doc.rust-lang.org/stable/std/fs/fn.read.html
    #[inline]
    pub fn read<P: AsRef<Path>>(&mut self, path: P) -> io::Result<Arc<Vec<u8>>> {
        let path = path.as_ref();
        self.inner.read(path)
    }

    /// Write a file to the VFS and the underlying backend.
    ///
    /// Roughly equivalent to [`std::fs::write`][std::fs::write].
    ///
    /// [std::fs::write]: https://doc.rust-lang.org/stable/std/fs/fn.write.html
    #[inline]
    pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(
        &mut self,
        path: P,
        contents: C,
    ) -> io::Result<()> {
        let path = path.as_ref();
        let contents = contents.as_ref();
        self.inner.write(path, contents)
    }

    /// Read all of the children of a directory.
    ///
    /// Roughly equivalent to [`std::fs::read_dir`][std::fs::read_dir].
    ///
    /// [std::fs::read_dir]: https://doc.rust-lang.org/stable/std/fs/fn.read_dir.html
    #[inline]
    pub fn read_dir<P: AsRef<Path>>(&mut self, path: P) -> io::Result<ReadDir> {
        let path = path.as_ref();
        self.inner.read_dir(path)
    }

    /// Creates a directory at the provided location.
    ///
    /// Roughly equivalent to [`std::fs::create_dir`][std::fs::create_dir].
    /// Similiar to that function, this function will fail if the parent of the
    /// path does not exist.
    ///
    /// [std::fs::create_dir]: https://doc.rust-lang.org/stable/std/fs/fn.create_dir.html
    #[inline]
    pub fn create_dir<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        self.inner.create_dir(path)
    }

    /// Creates a directory at the provided location, recursively creating
    /// all parent components if they are missing.
    ///
    /// Roughly equivalent to [`std::fs::create_dir_all`][std::fs::create_dir_all].
    ///
    /// [std::fs::create_dir_all]: https://doc.rust-lang.org/stable/std/fs/fn.create_dir_all.html
    #[inline]
    pub fn create_dir_all<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        self.inner.create_dir_all(path)
    }

    /// Remove a file.
    ///
    /// Roughly equivalent to [`std::fs::remove_file`][std::fs::remove_file].
    ///
    /// [std::fs::remove_file]: https://doc.rust-lang.org/stable/std/fs/fn.remove_file.html
    #[inline]
    pub fn remove_file<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        self.inner.remove_file(path)
    }

    /// Remove a directory and all of its descendants.
    ///
    /// Roughly equivalent to [`std::fs::remove_dir_all`][std::fs::remove_dir_all].
    ///
    /// [std::fs::remove_dir_all]: https://doc.rust-lang.org/stable/std/fs/fn.remove_dir_all.html
    #[inline]
    pub fn remove_dir_all<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let path = path.as_ref();
        self.inner.remove_dir_all(path)
    }

    /// Query metadata about the given path.
    ///
    /// Roughly equivalent to [`std::fs::metadata`][std::fs::metadata].
    ///
    /// [std::fs::metadata]: https://doc.rust-lang.org/stable/std/fs/fn.metadata.html
    #[inline]
    pub fn metadata<P: AsRef<Path>>(&mut self, path: P) -> io::Result<Metadata> {
        let path = path.as_ref();
        self.inner.metadata(path)
    }

    /// Normalize a path via the underlying backend.
    ///
    /// Roughly equivalent to [`std::fs::canonicalize`][std::fs::canonicalize].
    ///
    /// [std::fs::canonicalize]: https://doc.rust-lang.org/stable/std/fs/fn.canonicalize.html
    #[inline]
    pub fn canonicalize<P: AsRef<Path>>(&mut self, path: P) -> io::Result<PathBuf> {
        let path = path.as_ref();
        self.inner.canonicalize(path)
    }

    /// Canonicalize a path leniently: like [`VfsLock::canonicalize`], but
    /// usable on paths that may no longer exist. See [`Vfs::normalize`].
    #[inline]
    pub fn normalize<P: AsRef<Path>>(&mut self, path: P) -> PathBuf {
        let path = path.as_ref();
        self.inner.normalize(path)
    }

    /// Retrieve a handle to the event receiver for this `Vfs`.
    #[inline]
    pub fn event_receiver(&self) -> crossbeam_channel::Receiver<VfsEvent> {
        self.inner.event_receiver()
    }

    /// Commit an event to this `Vfs`.
    #[inline]
    pub fn commit_event(&mut self, event: &VfsEvent) -> io::Result<()> {
        self.inner.commit_event(event)
    }
}

#[cfg(test)]
mod test {
    use crate::{InMemoryFs, StdBackend, Vfs, VfsSnapshot};
    use std::io;
    use std::path::PathBuf;

    /// https://github.com/rojo-rbx/rojo/issues/899
    #[test]
    fn read_to_string_lf_normalized_keeps_trailing_newline() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot("test", VfsSnapshot::file("bar\r\nfoo\r\n\r\n"))
            .unwrap();

        let vfs = Vfs::new(imfs);

        assert_eq!(
            vfs.read_to_string_lf_normalized("test").unwrap().as_str(),
            "bar\nfoo\n\n"
        );
    }

    /// https://github.com/rojo-rbx/rojo/issues/1200
    #[test]
    fn canonicalize_in_memory_success() {
        let mut imfs = InMemoryFs::new();
        let contents = "Lorem ipsum dolor sit amet.".to_string();

        imfs.load_snapshot("/test/file.txt", VfsSnapshot::file(contents.to_string()))
            .unwrap();

        let vfs = Vfs::new(imfs);

        assert_eq!(
            vfs.canonicalize("/test/nested/../file.txt").unwrap(),
            PathBuf::from("/test/file.txt")
        );
        assert_eq!(
            vfs.read_to_string(vfs.canonicalize("/test/nested/../file.txt").unwrap())
                .unwrap()
                .to_string(),
            contents.to_string()
        );
    }

    #[test]
    fn canonicalize_in_memory_missing_errors() {
        let imfs = InMemoryFs::new();
        let vfs = Vfs::new(imfs);

        let err = vfs.canonicalize("test").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn canonicalize_std_backend_success() {
        let contents = "Lorem ipsum dolor sit amet.".to_string();
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file.txt");
        fs_err::write(&file_path, contents.to_string()).unwrap();

        let vfs = Vfs::new(StdBackend::new().unwrap());
        let canonicalized = vfs.canonicalize(&file_path).unwrap();
        assert_eq!(canonicalized, dunce::canonicalize(&file_path).unwrap());
        assert_eq!(
            vfs.read_to_string(&canonicalized).unwrap().to_string(),
            contents.to_string()
        );
    }

    #[test]
    #[cfg(windows)]
    fn canonicalize_std_backend_not_verbatim() {
        use std::path::{Component, Prefix};

        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file.txt");
        fs_err::write(&file_path, "hello").unwrap();

        let vfs = Vfs::new(StdBackend::new().unwrap());
        let canonicalized = vfs.canonicalize(&file_path).unwrap();

        let is_verbatim = matches!(
            canonicalized.components().next(),
            Some(Component::Prefix(prefix)) if matches!(
                prefix.kind(),
                Prefix::Verbatim(_) | Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(_, _)
            )
        );
        assert!(
            !is_verbatim,
            "expected a non-verbatim path, got {:?}",
            canonicalized
        );

        // Joining a relative parent path must preserve the `..` segment. On a
        // verbatim path Rust would drop it lexically, which is the root cause
        // of the bug.
        let joined = canonicalized.join("..").join("sibling");
        assert!(
            joined.components().any(|c| c == Component::ParentDir),
            "`..` should be preserved when joining onto {:?}",
            canonicalized
        );
    }

    #[test]
    fn canonicalize_std_backend_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test");

        let vfs = Vfs::new(StdBackend::new().unwrap());
        let err = vfs.canonicalize(&file_path).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    /// A path that exists is canonicalized directly, so `normalize` and
    /// `canonicalize` must agree.
    #[test]
    fn normalize_in_memory_existing() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/test",
            VfsSnapshot::dir([("file.txt", VfsSnapshot::file("hello"))]),
        )
        .unwrap();
        let vfs = Vfs::new(imfs);

        assert_eq!(
            vfs.normalize("/test/nested/../file.txt"),
            PathBuf::from("/test/file.txt")
        );
    }

    /// When a file no longer exists, we canonicalize the parent and reattach
    /// the file name.
    #[test]
    fn normalize_in_memory_missing_leaf_uses_parent() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/test",
            VfsSnapshot::dir([("file.txt", VfsSnapshot::file("hello"))]),
        )
        .unwrap();
        let vfs = Vfs::new(imfs);

        assert_eq!(
            vfs.normalize("/test/nested/../gone.txt"),
            PathBuf::from("/test/gone.txt")
        );
    }

    /// When several ancestors are gone (e.g. a directory deleted along with its
    /// contents) we walk up to the nearest existing ancestor, canonicalize it,
    /// and rebuild the rest of the path from there.
    #[test]
    fn normalize_in_memory_missing_ancestors_walk_up() {
        let mut imfs = InMemoryFs::new();
        imfs.load_snapshot(
            "/project",
            VfsSnapshot::dir([("src", VfsSnapshot::empty_dir())]),
        )
        .unwrap();
        let vfs = Vfs::new(imfs);

        // Only `/project/src` exists; the `nested/..` segment before it must
        // still be resolved even though everything after `src` is gone.
        assert_eq!(
            vfs.normalize("/project/nested/../src/sub/deep/gone.txt"),
            PathBuf::from("/project/src/sub/deep/gone.txt")
        );
    }

    /// If nothing along the path exists there is nothing to normalize against,
    /// so the path is returned unchanged rather than erroring.
    #[test]
    fn normalize_in_memory_nothing_exists() {
        let vfs = Vfs::new(InMemoryFs::new());

        assert_eq!(
            vfs.normalize("/does/not/exist.txt"),
            PathBuf::from("/does/not/exist.txt")
        );
    }

    /// A missing path ending in `..` can't have its trailing components
    /// reattached (`file_name` is `None`), so it falls back to the path as
    /// given.
    #[test]
    fn normalize_in_memory_parent_dir_suffix_bails() {
        let vfs = Vfs::new(InMemoryFs::new());

        assert_eq!(
            vfs.normalize("/does/not/exist/.."),
            PathBuf::from("/does/not/exist/..")
        );
    }

    /// `normalize` on an existing path must match `canonicalize` exactly.
    #[test]
    fn normalize_std_backend_existing() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("file.txt");
        fs_err::write(&file_path, "hello").unwrap();

        let vfs = Vfs::new(StdBackend::new().unwrap());
        assert_eq!(
            vfs.normalize(&file_path),
            dunce::canonicalize(&file_path).unwrap()
        );
    }

    /// A removed path normalizes to the exact canonical form it had while it
    /// existed, even when several of its ancestors were removed with it.
    ///
    /// The in-memory tests above cover the normalization *logic* but run
    /// identically on every OS. This one exercises the OS specifically.
    #[test]
    fn normalize_std_backend_matches_key_after_removal() {
        let temp = tempfile::tempdir().unwrap();
        let sub = temp.path().join("sub");
        fs_err::create_dir(&sub).unwrap();
        let file = sub.join("file.txt");
        fs_err::write(&file, "hello").unwrap();

        let vfs = Vfs::new(StdBackend::new().unwrap());

        // The canonical form a consumer (like Rojo's instance tree) would
        // have stored for this file while it existed.
        let canonical_key = vfs.canonicalize(&file).unwrap();

        fs_err::remove_file(&file).unwrap();
        fs_err::remove_dir(&sub).unwrap();

        // Even though `file` and `sub` are gone, normalization walks up to
        // the surviving temp dir, canonicalizes it, and reattaches
        // `sub/file.txt`, reproducing the canonical form on every platform.
        assert_eq!(vfs.normalize(&file), canonical_key);
    }
}
