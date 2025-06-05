mod mac_watcher;

use anyhow::{Context as _, Result, anyhow};
use gpui::App;
use gpui::BackgroundExecutor;
use gpui::Global;
use gpui::ReadGlobal as _;
use std::borrow::Cow;
use util::command::new_std_command;

use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::{FileTypeExt, MetadataExt};

use async_tar::Archive;
use futures::{AsyncRead, Stream, StreamExt, future::BoxFuture};
use git::repository::{GitRepository, RealGitRepository};
use rope::Rope;
use serde::{Deserialize, Serialize};
use smol::io::AsyncWriteExt;
use std::{
    io::{self, Write},
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;
use text::LineEnding;

pub trait Watcher: Send + Sync {
    fn add(&self, path: &Path) -> Result<()>;
    fn remove(&self, path: &Path) -> Result<()>;
}

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum PathEventKind {
    Removed,
    Created,
    Changed,
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct PathEvent {
    pub path: PathBuf,
    pub kind: Option<PathEventKind>,
}

impl From<PathEvent> for PathBuf {
    fn from(event: PathEvent) -> Self {
        event.path
    }
}

#[async_trait::async_trait]
pub trait Fs: Send + Sync {
    async fn create_dir(&self, path: &Path) -> Result<()>;
    async fn create_symlink(&self, path: &Path, target: PathBuf) -> Result<()>;
    async fn create_file(&self, path: &Path, options: CreateOptions) -> Result<()>;
    async fn create_file_with(
        &self,
        path: &Path,
        content: Pin<&mut (dyn AsyncRead + Send)>,
    ) -> Result<()>;
    async fn extract_tar_file(
        &self,
        path: &Path,
        content: Archive<Pin<&mut (dyn AsyncRead + Send)>>,
    ) -> Result<()>;
    async fn copy_file(&self, source: &Path, target: &Path, options: CopyOptions) -> Result<()>;
    async fn rename(&self, source: &Path, target: &Path, options: RenameOptions) -> Result<()>;
    async fn remove_dir(&self, path: &Path, options: RemoveOptions) -> Result<()>;
    async fn trash_dir(&self, path: &Path, options: RemoveOptions) -> Result<()> {
        self.remove_dir(path, options).await
    }
    async fn remove_file(&self, path: &Path, options: RemoveOptions) -> Result<()>;
    async fn trash_file(&self, path: &Path, options: RemoveOptions) -> Result<()> {
        self.remove_file(path, options).await
    }
    async fn open_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>>;
    async fn open_sync(&self, path: &Path) -> Result<Box<dyn io::Read + Send + Sync>>;
    async fn load(&self, path: &Path) -> Result<String> {
        Ok(String::from_utf8(self.load_bytes(path).await?)?)
    }
    async fn load_bytes(&self, path: &Path) -> Result<Vec<u8>>;
    async fn atomic_write(&self, path: PathBuf, text: String) -> Result<()>;
    async fn save(&self, path: &Path, text: &Rope, line_ending: LineEnding) -> Result<()>;
    async fn write(&self, path: &Path, content: &[u8]) -> Result<()>;
    async fn canonicalize(&self, path: &Path) -> Result<PathBuf>;
    async fn is_file(&self, path: &Path) -> bool;
    async fn is_dir(&self, path: &Path) -> bool;
    async fn metadata(&self, path: &Path) -> Result<Option<Metadata>>;
    async fn read_link(&self, path: &Path) -> Result<PathBuf>;
    async fn read_dir(
        &self,
        path: &Path,
    ) -> Result<Pin<Box<dyn Send + Stream<Item = Result<PathBuf>>>>>;

    async fn watch(
        &self,
        path: &Path,
        latency: Duration,
    ) -> (
        Pin<Box<dyn Send + Stream<Item = Vec<PathEvent>>>>,
        Arc<dyn Watcher>,
    );

    fn home_dir(&self) -> Option<PathBuf>;
    fn open_repo(&self, abs_dot_git: &Path) -> Option<Arc<dyn GitRepository>>;
    fn git_init(&self, abs_work_directory: &Path, fallback_branch_name: String) -> Result<()>;
    fn is_fake(&self) -> bool;
    async fn is_case_sensitive(&self) -> Result<bool>;
}

struct GlobalFs(Arc<dyn Fs>);

impl Global for GlobalFs {}

impl dyn Fs {
    /// Returns the global [`Fs`].
    pub fn global(cx: &App) -> Arc<Self> {
        GlobalFs::global(cx).0.clone()
    }

    /// Sets the global [`Fs`].
    pub fn set_global(fs: Arc<Self>, cx: &mut App) {
        cx.set_global(GlobalFs(fs));
    }
}

#[derive(Copy, Clone, Default)]
pub struct CreateOptions {
    pub overwrite: bool,
    pub ignore_if_exists: bool,
}

#[derive(Copy, Clone, Default)]
pub struct CopyOptions {
    pub overwrite: bool,
    pub ignore_if_exists: bool,
}

#[derive(Copy, Clone, Default)]
pub struct RenameOptions {
    pub overwrite: bool,
    pub ignore_if_exists: bool,
}

#[derive(Copy, Clone, Default)]
pub struct RemoveOptions {
    pub recursive: bool,
    pub ignore_if_not_exists: bool,
}

#[derive(Copy, Clone, Debug)]
pub struct Metadata {
    pub inode: u64,
    pub mtime: MTime,
    pub is_symlink: bool,
    pub is_dir: bool,
    pub len: u64,
    pub is_fifo: bool,
}

/// Filesystem modification time. The purpose of this newtype is to discourage use of operations
/// that do not make sense for mtimes. In particular, it is not always valid to compare mtimes using
/// `<` or `>`, as there are many things that can cause the mtime of a file to be earlier than it
/// was. See ["mtime comparison considered harmful" - apenwarr](https://apenwarr.ca/log/20181113).
///
/// Do not derive Ord, PartialOrd, or arithmetic operation traits.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(transparent)]
pub struct MTime(SystemTime);

impl MTime {
    /// Conversion intended for persistence and testing.
    pub fn from_seconds_and_nanos(secs: u64, nanos: u32) -> Self {
        MTime(UNIX_EPOCH + Duration::new(secs, nanos))
    }

    /// Conversion intended for persistence.
    pub fn to_seconds_and_nanos_for_persistence(self) -> Option<(u64, u32)> {
        self.0
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
    }

    /// Returns the value wrapped by this `MTime`, for presentation to the user. The name including
    /// "_for_user" is to discourage misuse - this method should not be used when making decisions
    /// about file dirtiness.
    pub fn timestamp_for_user(self) -> SystemTime {
        self.0
    }

    /// Temporary method to split out the behavior changes from introduction of this newtype.
    pub fn bad_is_greater_than(self, other: MTime) -> bool {
        self.0 > other.0
    }
}

impl From<proto::Timestamp> for MTime {
    fn from(timestamp: proto::Timestamp) -> Self {
        MTime(timestamp.into())
    }
}

impl From<MTime> for proto::Timestamp {
    fn from(mtime: MTime) -> Self {
        mtime.0.into()
    }
}

pub struct RealFs {
    git_binary_path: Option<PathBuf>,
    executor: BackgroundExecutor,
}

pub trait FileHandle: Send + Sync + std::fmt::Debug {
    fn current_path(&self, fs: &Arc<dyn Fs>) -> Result<PathBuf>;
}

impl FileHandle for std::fs::File {
    fn current_path(&self, _: &Arc<dyn Fs>) -> Result<PathBuf> {
        use std::{
            ffi::{CStr, OsStr},
            os::unix::ffi::OsStrExt,
        };

        let fd = self.as_fd();
        let mut path_buf: [libc::c_char; libc::PATH_MAX as usize] = [0; libc::PATH_MAX as usize];

        let result = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, path_buf.as_mut_ptr()) };
        if result == -1 {
            anyhow::bail!("fcntl returned -1".to_string());
        }

        let c_str = unsafe { CStr::from_ptr(path_buf.as_ptr()) };
        let path = PathBuf::from(OsStr::from_bytes(c_str.to_bytes()));
        Ok(path)
    }
}

pub struct RealWatcher {}

impl RealFs {
    pub fn new(git_binary_path: Option<PathBuf>, executor: BackgroundExecutor) -> Self {
        Self {
            git_binary_path,
            executor,
        }
    }
}

#[async_trait::async_trait]
impl Fs for RealFs {
    async fn create_dir(&self, path: &Path) -> Result<()> {
        Ok(smol::fs::create_dir_all(path).await?)
    }

    async fn create_symlink(&self, path: &Path, target: PathBuf) -> Result<()> {
        smol::fs::unix::symlink(target, path).await?;
        Ok(())
    }

    async fn create_file(&self, path: &Path, options: CreateOptions) -> Result<()> {
        let mut open_options = smol::fs::OpenOptions::new();
        open_options.write(true).create(true);
        if options.overwrite {
            open_options.truncate(true);
        } else if !options.ignore_if_exists {
            open_options.create_new(true);
        }
        open_options.open(path).await?;
        Ok(())
    }

    async fn create_file_with(
        &self,
        path: &Path,
        content: Pin<&mut (dyn AsyncRead + Send)>,
    ) -> Result<()> {
        let mut file = smol::fs::File::create(&path).await?;
        futures::io::copy(content, &mut file).await?;
        Ok(())
    }

    async fn extract_tar_file(
        &self,
        path: &Path,
        content: Archive<Pin<&mut (dyn AsyncRead + Send)>>,
    ) -> Result<()> {
        content.unpack(path).await?;
        Ok(())
    }

    async fn copy_file(&self, source: &Path, target: &Path, options: CopyOptions) -> Result<()> {
        if !options.overwrite && smol::fs::metadata(target).await.is_ok() {
            if options.ignore_if_exists {
                return Ok(());
            } else {
                anyhow::bail!("{target:?} already exists");
            }
        }

        smol::fs::copy(source, target).await?;
        Ok(())
    }

    async fn rename(&self, source: &Path, target: &Path, options: RenameOptions) -> Result<()> {
        if !options.overwrite && smol::fs::metadata(target).await.is_ok() {
            if options.ignore_if_exists {
                return Ok(());
            } else {
                anyhow::bail!("{target:?} already exists");
            }
        }

        smol::fs::rename(source, target).await?;
        Ok(())
    }

    async fn remove_dir(&self, path: &Path, options: RemoveOptions) -> Result<()> {
        let result = if options.recursive {
            smol::fs::remove_dir_all(path).await
        } else {
            smol::fs::remove_dir(path).await
        };
        match result {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound && options.ignore_if_not_exists => {
                Ok(())
            }
            Err(err) => Err(err)?,
        }
    }

    async fn remove_file(&self, path: &Path, options: RemoveOptions) -> Result<()> {
        match smol::fs::remove_file(path).await {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == io::ErrorKind::NotFound && options.ignore_if_not_exists => {
                Ok(())
            }
            Err(err) => Err(err)?,
        }
    }

    async fn trash_file(&self, path: &Path, _options: RemoveOptions) -> Result<()> {
        use cocoa::{
            base::{id, nil},
            foundation::{NSAutoreleasePool, NSString},
        };
        use objc::{class, msg_send, sel, sel_impl};

        unsafe {
            unsafe fn ns_string(string: &str) -> id {
                unsafe { NSString::alloc(nil).init_str(string).autorelease() }
            }

            let url: id = msg_send![class!(NSURL), fileURLWithPath: ns_string(path.to_string_lossy().as_ref())];
            let array: id = msg_send![class!(NSArray), arrayWithObject: url];
            let workspace: id = msg_send![class!(NSWorkspace), sharedWorkspace];

            let _: id = msg_send![workspace, recycleURLs: array completionHandler: nil];
        }
        Ok(())
    }

    async fn trash_dir(&self, path: &Path, options: RemoveOptions) -> Result<()> {
        self.trash_file(path, options).await
    }

    async fn open_sync(&self, path: &Path) -> Result<Box<dyn io::Read + Send + Sync>> {
        Ok(Box::new(std::fs::File::open(path)?))
    }

    async fn open_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>> {
        Ok(Arc::new(std::fs::File::open(path)?))
    }

    async fn load(&self, path: &Path) -> Result<String> {
        let path = path.to_path_buf();
        let text = smol::unblock(|| std::fs::read_to_string(path)).await?;
        Ok(text)
    }
    async fn load_bytes(&self, path: &Path) -> Result<Vec<u8>> {
        let path = path.to_path_buf();
        let bytes = smol::unblock(|| std::fs::read(path)).await?;
        Ok(bytes)
    }

    async fn atomic_write(&self, path: PathBuf, data: String) -> Result<()> {
        smol::unblock(move || {
            let mut tmp_file = tempfile::NamedTempFile::new()?;
            tmp_file.write_all(data.as_bytes())?;
            tmp_file.persist(path)?;
            anyhow::Ok(())
        })
        .await?;

        Ok(())
    }

    async fn save(&self, path: &Path, text: &Rope, line_ending: LineEnding) -> Result<()> {
        let buffer_size = text.summary().len.min(10 * 1024);
        if let Some(path) = path.parent() {
            self.create_dir(path).await?;
        }
        let file = smol::fs::File::create(path).await?;
        let mut writer = smol::io::BufWriter::with_capacity(buffer_size, file);
        for chunk in chunks(text, line_ending) {
            writer.write_all(chunk.as_bytes()).await?;
        }
        writer.flush().await?;
        Ok(())
    }

    async fn write(&self, path: &Path, content: &[u8]) -> Result<()> {
        if let Some(path) = path.parent() {
            self.create_dir(path).await?;
        }
        smol::fs::write(path, content).await?;
        Ok(())
    }

    async fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        Ok(smol::fs::canonicalize(path)
            .await
            .with_context(|| format!("canonicalizing {path:?}"))?)
    }

    async fn is_file(&self, path: &Path) -> bool {
        smol::fs::metadata(path)
            .await
            .map_or(false, |metadata| metadata.is_file())
    }

    async fn is_dir(&self, path: &Path) -> bool {
        smol::fs::metadata(path)
            .await
            .map_or(false, |metadata| metadata.is_dir())
    }

    async fn metadata(&self, path: &Path) -> Result<Option<Metadata>> {
        let symlink_metadata = match smol::fs::symlink_metadata(path).await {
            Ok(metadata) => metadata,
            Err(err) => {
                return match (err.kind(), err.raw_os_error()) {
                    (io::ErrorKind::NotFound, _) => Ok(None),
                    (io::ErrorKind::Other, Some(libc::ENOTDIR)) => Ok(None),
                    _ => Err(anyhow::Error::new(err)),
                };
            }
        };

        let path_buf = path.to_path_buf();
        let path_exists = smol::unblock(move || {
            path_buf
                .try_exists()
                .with_context(|| format!("checking existence for path {path_buf:?}"))
        })
        .await?;
        let is_symlink = symlink_metadata.file_type().is_symlink();
        let metadata = match (is_symlink, path_exists) {
            (true, true) => smol::fs::metadata(path)
                .await
                .with_context(|| "accessing symlink for path {path}")?,
            _ => symlink_metadata,
        };

        let inode = metadata.ino();

        let is_fifo = metadata.file_type().is_fifo();

        Ok(Some(Metadata {
            inode,
            mtime: MTime(metadata.modified().unwrap()),
            len: metadata.len(),
            is_symlink,
            is_dir: metadata.file_type().is_dir(),
            is_fifo,
        }))
    }

    async fn read_link(&self, path: &Path) -> Result<PathBuf> {
        let path = smol::fs::read_link(path).await?;
        Ok(path)
    }

    async fn read_dir(
        &self,
        path: &Path,
    ) -> Result<Pin<Box<dyn Send + Stream<Item = Result<PathBuf>>>>> {
        let result = smol::fs::read_dir(path).await?.map(|entry| match entry {
            Ok(entry) => Ok(entry.path()),
            Err(error) => Err(anyhow!("failed to read dir entry {error:?}")),
        });
        Ok(Box::pin(result))
    }

    async fn watch(
        &self,
        path: &Path,
        latency: Duration,
    ) -> (
        Pin<Box<dyn Send + Stream<Item = Vec<PathEvent>>>>,
        Arc<dyn Watcher>,
    ) {
        use fsevent::StreamFlags;

        let (events_tx, events_rx) = smol::channel::unbounded();
        let handles = Arc::new(parking_lot::Mutex::new(collections::BTreeMap::default()));
        let watcher = Arc::new(mac_watcher::MacWatcher::new(
            events_tx,
            Arc::downgrade(&handles),
            latency,
        ));
        watcher.add(path).expect("handles can't be dropped");

        (
            Box::pin(
                events_rx
                    .map(|events| {
                        events
                            .into_iter()
                            .map(|event| {
                                let kind = if event.flags.contains(StreamFlags::ITEM_REMOVED) {
                                    Some(PathEventKind::Removed)
                                } else if event.flags.contains(StreamFlags::ITEM_CREATED) {
                                    Some(PathEventKind::Created)
                                } else if event.flags.contains(StreamFlags::ITEM_MODIFIED) {
                                    Some(PathEventKind::Changed)
                                } else {
                                    None
                                };
                                PathEvent {
                                    path: event.path,
                                    kind,
                                }
                            })
                            .collect()
                    })
                    .chain(futures::stream::once(async move {
                        drop(handles);
                        vec![]
                    })),
            ),
            watcher,
        )
    }

    fn open_repo(&self, dotgit_path: &Path) -> Option<Arc<dyn GitRepository>> {
        Some(Arc::new(RealGitRepository::new(
            dotgit_path,
            self.git_binary_path.clone(),
            self.executor.clone(),
        )?))
    }

    fn git_init(&self, abs_work_directory_path: &Path, fallback_branch_name: String) -> Result<()> {
        let config = new_std_command("git")
            .current_dir(abs_work_directory_path)
            .args(&["config", "--global", "--get", "init.defaultBranch"])
            .output()?;

        let branch_name;

        if config.status.success() && !config.stdout.is_empty() {
            branch_name = String::from_utf8_lossy(&config.stdout);
        } else {
            branch_name = Cow::Borrowed(fallback_branch_name.as_str());
        }

        new_std_command("git")
            .current_dir(abs_work_directory_path)
            .args(&["init", "-b"])
            .arg(branch_name.trim())
            .output()?;

        Ok(())
    }

    fn is_fake(&self) -> bool {
        false
    }

    /// Checks whether the file system is case sensitive by attempting to create two files
    /// that have the same name except for the casing.
    ///
    /// It creates both files in a temporary directory it removes at the end.
    async fn is_case_sensitive(&self) -> Result<bool> {
        let temp_dir = TempDir::new()?;
        let test_file_1 = temp_dir.path().join("case_sensitivity_test.tmp");
        let test_file_2 = temp_dir.path().join("CASE_SENSITIVITY_TEST.TMP");

        let create_opts = CreateOptions {
            overwrite: false,
            ignore_if_exists: false,
        };

        // Create file1
        self.create_file(&test_file_1, create_opts).await?;

        // Now check whether it's possible to create file2
        let case_sensitive = match self.create_file(&test_file_2, create_opts).await {
            Ok(_) => Ok(true),
            Err(e) => {
                if let Some(io_error) = e.downcast_ref::<io::Error>() {
                    if io_error.kind() == io::ErrorKind::AlreadyExists {
                        Ok(false)
                    } else {
                        Err(e)
                    }
                } else {
                    Err(e)
                }
            }
        };

        temp_dir.close()?;
        case_sensitive
    }

    fn home_dir(&self) -> Option<PathBuf> {
        Some(paths::home_dir().clone())
    }
}

impl Watcher for RealWatcher {
    fn add(&self, _: &Path) -> Result<()> {
        Ok(())
    }

    fn remove(&self, _: &Path) -> Result<()> {
        Ok(())
    }
}

fn chunks(rope: &Rope, line_ending: LineEnding) -> impl Iterator<Item = &str> {
    rope.chunks().flat_map(move |chunk| {
        let mut newline = false;
        let end_with_newline = chunk.ends_with('\n').then_some(line_ending.as_str());
        chunk
            .lines()
            .flat_map(move |line| {
                let ending = if newline {
                    Some(line_ending.as_str())
                } else {
                    None
                };
                newline = true;
                ending.into_iter().chain([line])
            })
            .chain(end_with_newline)
    })
}

pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components = path.components().peekable();
    let mut ret = if let Some(c @ Component::Prefix(..)) = components.peek().cloned() {
        components.next();
        PathBuf::from(c.as_os_str())
    } else {
        PathBuf::new()
    };

    for component in components {
        match component {
            Component::Prefix(..) => unreachable!(),
            Component::RootDir => {
                ret.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                ret.pop();
            }
            Component::Normal(c) => {
                ret.push(c);
            }
        }
    }
    ret
}

pub async fn copy_recursive<'a>(
    fs: &'a dyn Fs,
    source: &'a Path,
    target: &'a Path,
    options: CopyOptions,
) -> Result<()> {
    for (item, is_dir) in read_dir_items(fs, source).await? {
        let Ok(item_relative_path) = item.strip_prefix(source) else {
            continue;
        };
        let target_item = if item_relative_path == Path::new("") {
            target.to_path_buf()
        } else {
            target.join(item_relative_path)
        };
        if is_dir {
            if !options.overwrite && fs.metadata(&target_item).await.is_ok_and(|m| m.is_some()) {
                if options.ignore_if_exists {
                    continue;
                } else {
                    anyhow::bail!("{target_item:?} already exists");
                }
            }
            let _ = fs
                .remove_dir(
                    &target_item,
                    RemoveOptions {
                        recursive: true,
                        ignore_if_not_exists: true,
                    },
                )
                .await;
            fs.create_dir(&target_item).await?;
        } else {
            fs.copy_file(&item, &target_item, options).await?;
        }
    }
    Ok(())
}

/// Recursively reads all of the paths in the given directory.
///
/// Returns a vector of tuples of (path, is_dir).
pub async fn read_dir_items<'a>(fs: &'a dyn Fs, source: &'a Path) -> Result<Vec<(PathBuf, bool)>> {
    let mut items = Vec::new();
    read_recursive(fs, source, &mut items).await?;
    Ok(items)
}

fn read_recursive<'a>(
    fs: &'a dyn Fs,
    source: &'a Path,
    output: &'a mut Vec<(PathBuf, bool)>,
) -> BoxFuture<'a, Result<()>> {
    use futures::future::FutureExt;

    async move {
        let metadata = fs
            .metadata(source)
            .await?
            .with_context(|| format!("path does not exist: {source:?}"))?;

        if metadata.is_dir {
            output.push((source.to_path_buf(), true));
            let mut children = fs.read_dir(source).await?;
            while let Some(child_path) = children.next().await {
                if let Ok(child_path) = child_path {
                    read_recursive(fs, &child_path, output).await?;
                }
            }
        } else {
            output.push((source.to_path_buf(), false));
        }
        Ok(())
    }
    .boxed()
}
