use std::{
    collections::{btree_map, BTreeMap, HashSet},
    ffi::{CStr, CString},
    fs::File,
    io,
    mem::{self, MaybeUninit},
    os::{
        fd::{AsRawFd, FromRawFd, RawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, LazyLock, RwLock,
    },
    time::Duration,
};

use caps::{has_cap, CapSet, Capability};
use intaglio::{cstr::SymbolTable, Symbol};
use nix::request_code_read;

use crate::virtio::{
    bindings,
    fs::{
        filesystem::{
            self, Context, DirEntry, Entry, ExportTable, Extensions, FileSystem, FsOptions,
            GetxattrReply, ListxattrReply, OpenOptions, SetattrValid, ZeroCopyReader,
            ZeroCopyWriter,
        },
        fuse,
        multikey::MultikeyBTreeMap,
    },
};

//--------------------------------------------------------------------------------------------------
// Modules
//--------------------------------------------------------------------------------------------------

#[path = "../tests/overlayfs/mod.rs"]
#[cfg(test)]
mod tests;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// The prefix for whiteout files
const WHITEOUT_PREFIX: &str = ".wh.";

/// The marker for opaque directories
const OPAQUE_MARKER: &str = ".wh..wh..opq";

/// Maximum allowed number of layers for the overlay filesystem.
const MAX_LAYERS: usize = 128;

#[cfg(not(feature = "efi"))]
static INIT_BINARY: &[u8] = include_bytes!("../../../../../../init/init");

/// The name of the init binary
const INIT_CSTR: &[u8] = b"init.krun\0";

/// The name of the empty directory
const EMPTY_CSTR: LazyLock<&CStr> =
    LazyLock::new(|| unsafe { CStr::from_bytes_with_nul_unchecked(b"\0") });

/// The name of the `/proc/self/fd` directory
const PROC_SELF_FD_CSTR: LazyLock<&CStr> =
    LazyLock::new(|| unsafe { CStr::from_bytes_with_nul_unchecked(b"/proc/self/fd\0") });

/// FICLONE ioctl for copy-on-write file cloning
/// Defined in Linux's fs.h as _IOW(0x94, 9, int)
const FICLONE: u64 = (0x94 << 8) | 9 | (std::mem::size_of::<i32>() as u64) << 16 | 1 << 30;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Type alias for inode identifiers
type Inode = u64;

/// Type alias for file handle identifiers
type Handle = u64;

/// Alternative key for looking up inodes by device and inode number
#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
struct InodeAltKey {
    /// The inode number from the host filesystem
    ino: libc::ino64_t,

    /// The device ID from the host filesystem
    dev: libc::dev_t,

    /// The mount ID from the host filesystem
    mnt_id: u64,
}

/// Data associated with an inode
#[derive(Debug)]
pub(crate) struct InodeData {
    /// The inode number in the overlay filesystem
    pub(crate) inode: Inode,

    /// The file handle for the inode
    pub(crate) file: File,

    /// The device ID from the host filesystem
    pub(crate) dev: libc::dev_t,

    /// The mount ID from the host filesystem
    pub(crate) mnt_id: u64,

    /// Reference count for this inode from the perspective of [`FileSystem::lookup`]
    pub(crate) refcount: AtomicU64,

    /// Path to inode
    pub(crate) path: Vec<Symbol>,

    /// The layer index this inode belongs to
    pub(crate) layer_idx: usize,
}

/// Data associated with an open file handle
#[derive(Debug)]
pub(crate) struct HandleData {
    /// The inode this handle refers to
    inode: Inode,

    /// The underlying file object
    file: RwLock<File>,

    /// Whether the file handle is exported
    exported: AtomicBool,
}

pub(crate) struct ScopedGid;

pub(crate) struct ScopedUid;

/// The caching policy that the file system should report to the FUSE client. By default the FUSE
/// protocol uses close-to-open consistency. This means that any cached contents of the file are
/// invalidated the next time that file is opened.
#[derive(Default, Debug, Clone)]
pub enum CachePolicy {
    /// The client should never cache file data and all I/O should be directly forwarded to the
    /// server. This policy must be selected when file contents may change without the knowledge of
    /// the FUSE client (i.e., the file system does not have exclusive access to the directory).
    Never,

    /// The client is free to choose when and how to cache file data. This is the default policy and
    /// uses close-to-open consistency as described in the enum documentation.
    #[default]
    Auto,

    /// The client should always cache file data. This means that the FUSE client will not
    /// invalidate any cached data that was returned by the file system the last time the file was
    /// opened. This policy should only be selected when the file system has exclusive access to the
    /// directory.
    Always,
}

/// Configuration options that control the behavior of the file system.
#[derive(Debug, Clone)]
pub struct Config {
    /// How long the FUSE client should consider directory entries to be valid. If the contents of a
    /// directory can only be modified by the FUSE client (i.e., the file system has exclusive
    /// access), then this should be a large value.
    ///
    /// The default value for this option is 5 seconds.
    pub entry_timeout: Duration,

    /// How long the FUSE client should consider file and directory attributes to be valid. If the
    /// attributes of a file or directory can only be modified by the FUSE client (i.e., the file
    /// system has exclusive access), then this should be set to a large value.
    ///
    /// The default value for this option is 5 seconds.
    pub attr_timeout: Duration,

    /// The caching policy the file system should use. See the documentation of `CachePolicy` for
    /// more details.
    pub cache_policy: CachePolicy,

    /// Whether the file system should enabled writeback caching. This can improve performance as it
    /// allows the FUSE client to cache and coalesce multiple writes before sending them to the file
    /// system. However, enabling this option can increase the risk of data corruption if the file
    /// contents can change without the knowledge of the FUSE client (i.e., the server does **NOT**
    /// have exclusive access). Additionally, the file system should have read access to all files
    /// in the directory it is serving as the FUSE client may send read requests even for files
    /// opened with `O_WRONLY`.
    ///
    /// Therefore callers should only enable this option when they can guarantee that: 1) the file
    /// system has exclusive access to the directory and 2) the file system has read permissions for
    /// all files in that directory.
    ///
    /// The default value for this option is `false`.
    pub writeback: bool,

    /// The path of the root directory.
    ///
    /// The default is `/`.
    pub root_dir: String,

    /// Whether the file system should support Extended Attributes (xattr). Enabling this feature may
    /// have a significant impact on performance, especially on write parallelism. This is the result
    /// of FUSE attempting to remove the special file privileges after each write request.
    ///
    /// The default value for this options is `false`.
    pub xattr: bool,

    /// Optional file descriptor for /proc/self/fd. Callers can obtain a file descriptor and pass it
    /// here, so there's no need to open it in the filesystem implementation. This is specially useful
    /// for sandboxing.
    ///
    /// The default is `None`.
    pub proc_sfd_rawfd: Option<RawFd>,

    /// ID of this filesystem to uniquely identify exports.
    pub export_fsid: u64,

    /// Table of exported FDs to share with other subsystems.
    pub export_table: Option<ExportTable>,

    /// Layers to be used for the overlay filesystem
    pub layers: Vec<PathBuf>,
}

/// An overlay filesystem implementation that combines multiple layers into a single logical filesystem.
///
/// This implementation follows standard overlay filesystem concepts, similar to Linux's OverlayFS,
/// while using OCI image specification's layer filesystem changeset format for whiteouts:
///
/// - Uses OCI-style whiteout files (`.wh.` prefixed files) to mark deleted files in upper layers
/// - Uses OCI-style opaque directory markers (`.wh..wh..opq`) to mask lower layer directories
///
/// ## Layer Structure
///
/// The overlay filesystem consists of:
/// - A single top layer (upperdir) that is writable
/// - Zero or more lower layers that are read-only
///
/// ## Layer Ordering
///
/// When creating an overlay filesystem, layers are provided in order from lowest to highest:
/// The last layer in the provided sequence becomes the top layer (upperdir), while
/// the others become read-only lower layers. This matches the OCI specification where:
/// - The top layer (upperdir) handles all modifications
/// - Lower layers provide the base content
/// - Changes in the top layer shadow content in lower layers
///
/// ## Layer Behavior
///
/// - All write operations occur in the top layer
/// - When reading, the top layer takes precedence over lower layers
/// - Whiteout files in the top layer hide files from lower layers
/// - Opaque directory markers completely mask lower layer directory contents
/// - It is undefined behavior for whiteouts and their corresponding entries to exist at the same level in the same directory.
///   For example, looking up such entry can result in different behavior depending on which is found first.
///   The filesystem will try to prevent adding whiteout entries directly.
///
/// TODO: Need to implement entry caching to improve the performance of [`Self::lookup_segment_by_segment`].
pub struct OverlayFs {
    /// Map of inodes by ID and alternative keys. The alternative keys allow looking up inodes by their
    /// underlying host filesystem inode number, device ID and mount ID.
    inodes: RwLock<MultikeyBTreeMap<Inode, InodeAltKey, Arc<InodeData>>>,

    /// Counter for generating the next inode ID. Each new inode gets a unique ID from this counter.
    next_inode: AtomicU64,

    /// The initial inode ID (typically 1 for the root directory)
    init_inode: u64,

    /// Map of open file handles by ID. Each open file gets a unique handle ID that maps to the
    /// underlying file descriptor and associated data.
    handles: RwLock<BTreeMap<Handle, Arc<HandleData>>>,

    /// Counter for generating the next handle ID. Each new file handle gets a unique ID from this counter.
    next_handle: AtomicU64,

    /// The initial handle ID
    init_handle: u64,

    /// File descriptor pointing to the `/proc/self/fd` directory. This is used to convert an fd from
    /// `inodes` into one that can go into `handles`. This is accomplished by reading the
    /// `/proc/self/fd/{}` symlink.
    proc_self_fd: File,

    /// Whether writeback caching is enabled for this directory. This will only be true when
    /// `cfg.writeback` is true and `init` was called with `FsOptions::WRITEBACK_CACHE`.
    writeback: AtomicBool,

    /// Whether to announce submounts. When true, the filesystem will report when directories are
    /// mount points for other filesystems.
    announce_submounts: AtomicBool,

    /// The UID of the process if it doesn't have CAP_SETUID capability, None otherwise.
    /// Used to restrict UID changes to privileged processes.
    my_uid: Option<libc::uid_t>,

    /// The GID of the process if it doesn't have CAP_SETGID capability, None otherwise.
    /// Used to restrict GID changes to privileged processes.
    my_gid: Option<libc::gid_t>,

    /// Whether the process has CAP_FOWNER capability.
    cap_fowner: bool,

    /// Configuration options for the filesystem
    config: Config,

    /// Symbol table for interned filenames to efficiently store and compare path components
    filenames: Arc<RwLock<SymbolTable>>,

    /// Root inodes for each layer, ordered from bottom to top. The last element is the upperdir
    /// (writable layer) while all others are read-only lower layers.
    layer_roots: Arc<RwLock<Vec<Inode>>>,
}

/// Represents either a file or a path
enum FileOrPath {
    /// A file
    File(File),

    /// A path
    Path(CString),
}

/// Represents either a file descriptor or a path
enum FileId {
    /// A file descriptor
    Fd(RawFd),

    /// A path
    Path(CString),
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl ScopedGid {
    fn new(gid: libc::gid_t) -> io::Result<Self> {
        let res = unsafe { libc::syscall(libc::SYS_setresgid, -1, gid, -1) };
        if res != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {})
    }
}

impl ScopedUid {
    fn new(uid: libc::uid_t) -> io::Result<Self> {
        let res = unsafe { libc::syscall(libc::SYS_setresuid, -1, uid, -1) };
        if res != 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {})
    }
}

impl InodeAltKey {
    fn new(ino: libc::ino64_t, dev: libc::dev_t, mnt_id: u64) -> Self {
        Self { ino, dev, mnt_id }
    }
}

impl OverlayFs {
    /// Creates a new OverlayFs with the given layers
    pub fn new(config: Config) -> io::Result<Self> {
        if config.layers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one layer must be provided",
            ));
        }

        if config.layers.len() > MAX_LAYERS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "maximum overlayfs layer count exceeded",
            ));
        }

        let mut next_inode = 1;
        let mut inodes = MultikeyBTreeMap::new();

        // Initialize the root inodes for all layers
        let layer_roots = Self::init_root_inodes(&config.layers, &mut inodes, &mut next_inode)?;

        // Set the `init.krun` inode
        let init_inode = next_inode;
        next_inode += 1;

        // Get the file descriptor for /proc/self/fd
        let proc_self_fd = if let Some(fd) = config.proc_sfd_rawfd {
            fd
        } else {
            // Safe because this doesn't modify any memory and we check the return value.
            let fd = unsafe {
                libc::openat(
                    libc::AT_FDCWD,
                    PROC_SELF_FD_CSTR.as_ptr(),
                    libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };

            if fd < 0 {
                return Err(io::Error::last_os_error());
            }

            fd
        };

        // Get the UID of the process
        let my_uid = if has_cap(None, CapSet::Effective, Capability::CAP_SETUID).unwrap_or_default()
        {
            None
        } else {
            // SAFETY: This syscall is always safe to call and  always succeeds.
            Some(unsafe { libc::getuid() })
        };

        // Get the GID of the process
        let my_gid = if has_cap(None, CapSet::Effective, Capability::CAP_SETGID).unwrap_or_default()
        {
            None
        } else {
            // SAFETY: This syscall is always safe to call and  always succeeds.
            Some(unsafe { libc::getgid() })
        };

        let cap_fowner =
            has_cap(None, CapSet::Effective, Capability::CAP_FOWNER).unwrap_or_default();

        // SAFETY: We just opened this fd or it was provided by our caller.
        let proc_self_fd = unsafe { File::from_raw_fd(proc_self_fd) };

        Ok(OverlayFs {
            inodes: RwLock::new(inodes),
            next_inode: AtomicU64::new(next_inode),
            init_inode,
            handles: RwLock::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
            init_handle: 0,
            proc_self_fd,
            writeback: AtomicBool::new(false),
            announce_submounts: AtomicBool::new(false),
            my_uid,
            my_gid,
            cap_fowner,
            config,
            filenames: Arc::new(RwLock::new(SymbolTable::new())),
            layer_roots: Arc::new(RwLock::new(layer_roots)),
        })
    }

    /// Initialize root inodes for all layers
    ///
    /// This function processes layers from top to bottom, creating root inodes for each layer.
    ///
    /// Parameters:
    /// - layers: Slice of paths to the layer roots, ordered from bottom to top
    /// - inodes: Mutable reference to the inodes map to populate
    /// - next_inode: Mutable reference to the next inode counter
    ///
    /// Returns:
    /// - io::Result<Vec<Inode>> containing the root inodes for each layer
    fn init_root_inodes(
        layers: &[PathBuf],
        inodes: &mut MultikeyBTreeMap<Inode, InodeAltKey, Arc<InodeData>>,
        next_inode: &mut u64,
    ) -> io::Result<Vec<Inode>> {
        // Pre-allocate layer_roots with the right size
        let mut layer_roots = vec![0; layers.len()];

        // Process layers from top to bottom
        for (i, layer_path) in layers.iter().enumerate().rev() {
            let layer_idx = i; // Layer index from bottom to top

            // Get the stat information for this layer's root
            let c_path = CString::new(layer_path.to_string_lossy().as_bytes())?;

            // Open the directory
            let file = Self::open_path_file(&c_path)?;

            // Get statx information
            let (st, mnt_id) = Self::statx(file.as_raw_fd(), None)?;

            // Create the alt key for this inode
            let alt_key = InodeAltKey::new(st.st_ino, st.st_dev, mnt_id);

            // Create the inode data
            let inode_id = *next_inode;
            *next_inode += 1;

            let inode_data = Arc::new(InodeData {
                inode: inode_id,
                file,
                dev: st.st_dev,
                mnt_id,
                refcount: AtomicU64::new(1),
                path: vec![],
                layer_idx,
            });

            // Insert the inode into the map
            inodes.insert(inode_id, alt_key, inode_data);

            // Store the root inode for this layer
            layer_roots[layer_idx] = inode_id;
        }

        Ok(layer_roots)
    }

    /// Opens a file without following symlinks.
    fn open_file(path: &CStr, flags: i32) -> io::Result<File> {
        let fd = unsafe { libc::open(path.as_ptr(), flags | libc::O_NOFOLLOW, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Safe because we just opened this fd.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Opens a file relative to a parent without following symlinks.
    fn open_file_at(parent: RawFd, name: &CStr, flags: i32) -> io::Result<File> {
        let fd = unsafe { libc::openat(parent, name.as_ptr(), flags | libc::O_NOFOLLOW, 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Safe because we just opened this fd.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Opens a path as an O_PATH file.
    fn open_path_file(path: &CStr) -> io::Result<File> {
        Self::open_file(path, libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
    }

    /// Opens a path relative to a parent as an O_PATH file.
    fn open_path_file_at(parent: RawFd, name: &CStr) -> io::Result<File> {
        Self::open_file_at(
            parent,
            name,
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    }

    /// Performs a statx syscall without any modifications to the returned stat structure.
    fn statx(fd: RawFd, name: Option<&CStr>) -> io::Result<(libc::stat64, u64)> {
        let mut stx = MaybeUninit::<libc::statx>::zeroed();
        let res = unsafe {
            libc::statx(
                fd,
                name.unwrap_or(&*EMPTY_CSTR).as_ptr(),
                libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                libc::STATX_BASIC_STATS | libc::STATX_MNT_ID,
                stx.as_mut_ptr(),
            )
        };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        // Safe because the kernel guarantees that the struct is now fully initialized.
        let stx = unsafe { stx.assume_init() };

        // Unfortunately, we cannot use an initializer to create the stat64 object,
        // because it may contain padding and reserved fields (depending on the
        // architecture), and it does not implement the Default trait.
        // So we take a zeroed struct and set what we can. (Zero in all fields is
        // wrong, but safe.)
        let mut st = unsafe { MaybeUninit::<libc::stat64>::zeroed().assume_init() };

        st.st_dev = libc::makedev(stx.stx_dev_major, stx.stx_dev_minor);
        st.st_ino = stx.stx_ino;
        st.st_mode = stx.stx_mode as _;
        st.st_nlink = stx.stx_nlink as _;
        st.st_uid = stx.stx_uid;
        st.st_gid = stx.stx_gid;
        st.st_rdev = libc::makedev(stx.stx_rdev_major, stx.stx_rdev_minor);
        st.st_size = stx.stx_size as _;
        st.st_blksize = stx.stx_blksize as _;
        st.st_blocks = stx.stx_blocks as _;
        st.st_atime = stx.stx_atime.tv_sec;
        st.st_atime_nsec = stx.stx_atime.tv_nsec as _;
        st.st_mtime = stx.stx_mtime.tv_sec;
        st.st_mtime_nsec = stx.stx_mtime.tv_nsec as _;
        st.st_ctime = stx.stx_ctime.tv_sec;
        st.st_ctime_nsec = stx.stx_ctime.tv_nsec as _;

        Ok((st, stx.stx_mnt_id))
    }

    /// Turns an inode data into a file descriptor string.
    fn data_to_fd_str(data: &InodeData) -> io::Result<CString> {
        let fd = format!("{}", data.file.as_raw_fd());
        CString::new(fd).map_err(|_| einval())
    }

    /// Turns an inode data into a path.
    fn data_to_path(data: &InodeData) -> io::Result<CString> {
        let path = format!("/proc/self/fd/{}", data.file.as_raw_fd());
        CString::new(path).map_err(|_| einval())
    }

    /// Turns an inode into an opened file.
    fn open_inode(&self, inode: Inode, mut flags: i32) -> io::Result<File> {
        let data = self.get_inode_data(inode)?;
        let fd_str = Self::data_to_fd_str(&data)?;

        // When writeback caching is enabled, the kernel may send read requests even if the
        // userspace program opened the file write-only. So we need to ensure that we have opened
        // the file for reading as well as writing.
        let writeback = self.writeback.load(Ordering::Relaxed);
        if writeback && flags & libc::O_ACCMODE == libc::O_WRONLY {
            flags &= !libc::O_ACCMODE;
            flags |= libc::O_RDWR;
        }

        // When writeback caching is enabled the kernel is responsible for handling `O_APPEND`.
        // However, this breaks atomicity as the file may have changed on disk, invalidating the
        // cached copy of the data in the kernel and the offset that the kernel thinks is the end of
        // the file. Just allow this for now as it is the user's responsibility to enable writeback
        // caching only for directories that are not shared. It also means that we need to clear the
        // `O_APPEND` flag.
        if writeback && flags & libc::O_APPEND != 0 {
            flags &= !libc::O_APPEND;
        }

        // If the file is a symlink, just clone existing file.
        if data.file.metadata()?.is_symlink() {
            return Ok(data.file.try_clone()?);
        }

        // Safe because this doesn't modify any memory and we check the return value. We don't
        // really check `flags` because if the kernel can't handle poorly specified flags then we
        // have much bigger problems.
        //
        // It is safe to follow here since symlinks are returned early as O_PATH files.
        let fd = unsafe {
            libc::openat(
                self.proc_self_fd.as_raw_fd(),
                fd_str.as_ptr(),
                flags | libc::O_CLOEXEC & (!libc::O_NOFOLLOW),
            )
        };

        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Safe because we just opened this fd.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Turns an inode into an opened file or a path.
    fn open_inode_or_path(&self, inode: Inode, flags: i32) -> io::Result<FileOrPath> {
        match self.open_inode(inode, flags) {
            Ok(file) => Ok(FileOrPath::File(file)),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
                let data = self.get_inode_data(inode)?;
                let path = Self::data_to_path(&data)?;
                Ok(FileOrPath::Path(path))
            }
            Err(e) => Err(e),
        }
    }

    pub fn get_config(&self) -> &Config {
        &self.config
    }

    pub fn get_filenames(&self) -> &Arc<RwLock<SymbolTable>> {
        &self.filenames
    }

    fn get_layer_root(&self, layer_idx: usize) -> io::Result<Arc<InodeData>> {
        let layer_roots = self.layer_roots.read().unwrap();

        // Check if the layer index is valid
        if layer_idx >= layer_roots.len() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "layer index out of bounds",
            ));
        }

        // Get the inode for this layer
        let inode = layer_roots[layer_idx];
        if inode == 0 {
            return Err(io::Error::new(io::ErrorKind::NotFound, "layer not found"));
        }

        // Get the inode data
        self.get_inode_data(inode)
    }

    /// Creates a new inode and adds it to the inode map
    fn create_inode(
        &self,
        file: File,
        ino: libc::ino64_t,
        dev: libc::dev_t,
        mnt_id: u64,
        path: Vec<Symbol>,
        layer_idx: usize,
    ) -> (Inode, Arc<InodeData>) {
        let inode = self.next_inode.fetch_add(1, Ordering::SeqCst);

        let data = Arc::new(InodeData {
            inode,
            file,
            dev,
            mnt_id,
            refcount: AtomicU64::new(1),
            path,
            layer_idx,
        });

        let alt_key = InodeAltKey::new(ino, dev, mnt_id);
        self.inodes
            .write()
            .unwrap()
            .insert(inode, alt_key, data.clone());

        (inode, data)
    }

    /// Creates an Entry from stat information and inode data
    fn create_entry(&self, inode: Inode, st: bindings::stat64) -> Entry {
        Entry {
            inode,
            generation: 0,
            attr: st,
            attr_flags: 0,
            attr_timeout: self.config.attr_timeout,
            entry_timeout: self.config.entry_timeout,
        }
    }

    fn create_whiteout_path(&self, name: &CStr) -> io::Result<CString> {
        let name_str = name.to_str().map_err(|_| einval())?;
        let whiteout_path = format!("{WHITEOUT_PREFIX}{name_str}");
        CString::new(whiteout_path).map_err(|_| einval())
    }

    /// Checks for whiteout file in top layer
    fn check_whiteout(&self, parent: RawFd, name: &CStr) -> io::Result<bool> {
        let whiteout_cpath = self.create_whiteout_path(name)?;

        match Self::statx(parent, Some(&whiteout_cpath)) {
            Ok(_) => {
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Ok(false)
            }
            Err(e) => {
                Err(e)
            }
        }
    }

    /// Checks for an opaque directory marker in the given parent directory path.
    fn check_opaque_marker(&self, parent: RawFd) -> io::Result<bool> {
        let opaque_cpath = CString::new(OPAQUE_MARKER).map_err(|_| einval())?;

        match Self::statx(parent, Some(&opaque_cpath)) {
            Ok(_) => {
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                Ok(false)
            }
            Err(e) => {
                Err(e)
            }
        }
    }

    /// Interns a name and returns the corresponding Symbol
    fn intern_name(&self, name: &CStr) -> io::Result<Symbol> {
        // Clone the name to avoid lifetime issues
        let name_to_intern = CString::new(name.to_bytes()).map_err(|_| einval())?;

        // Get a write lock to intern it
        let mut filenames = self.filenames.write().unwrap();
        filenames.intern(name_to_intern).map_err(|e| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("Failed to intern filename: {}", e),
            )
        })
    }

    /// Gets the InodeData for an inode
    pub(super) fn get_inode_data(&self, inode: Inode) -> io::Result<Arc<InodeData>> {
        self.inodes
            .read()
            .unwrap()
            .get(&inode)
            .cloned()
            .ok_or_else(ebadf)
    }

    /// Gets the HandleData for a handle
    pub(super) fn get_inode_handle_data(
        &self,
        inode: Inode,
        handle: Handle,
    ) -> io::Result<Arc<HandleData>> {
        self.handles
            .read()
            .unwrap()
            .get(&handle)
            .filter(|hd| hd.inode == inode)
            .cloned()
            .ok_or_else(ebadf)
    }

    fn get_top_layer_idx(&self) -> usize {
        self.layer_roots.read().unwrap().len() - 1
    }

    fn bump_refcount(&self, inode: Inode) {
        let inodes = self.inodes.write().unwrap();
        let inode_data = inodes.get(&inode).unwrap();
        inode_data.refcount.fetch_add(1, Ordering::SeqCst);
    }

    /// Validates a name to prevent path traversal attacks and special overlay markers
    ///
    /// This function checks if a name contains:
    /// - Path traversal sequences like ".."
    /// - Other potentially dangerous patterns like slashes
    /// - Whiteout markers (.wh. prefix)
    /// - Opaque directory markers (.wh..wh..opq)
    ///
    /// Returns:
    /// - Ok(()) if the name is safe
    /// - Err(io::Error) if the name contains invalid patterns
    fn validate_name(name: &CStr) -> io::Result<()> {
        let name_bytes = name.to_bytes();

        // Check for empty name
        if name_bytes.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "empty name is not allowed",
            ));
        }

        // Check for path traversal sequences
        if name_bytes == b".." || name_bytes.contains(&b'/') || name_bytes.contains(&b'\\') {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "path traversal attempt detected",
            ));
        }

        // Check for null bytes
        if name_bytes.contains(&0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "name contains null bytes",
            ));
        }

        // Convert to str for string pattern matching
        let name_str = match std::str::from_utf8(name_bytes) {
            Ok(s) => s,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "name contains invalid UTF-8",
                ))
            }
        };

        // Check for whiteout prefix
        if name_str.starts_with(".wh.") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "name cannot start with whiteout prefix",
            ));
        }

        // Check for opaque marker
        if name_str == ".wh..wh..opq" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "name cannot be an opaque directory marker",
            ));
        }

        Ok(())
    }

    /// Looks up a path segment by segment in a given layer
    ///
    /// This function traverses a path one segment at a time within a specific layer,
    /// handling whiteouts and opaque markers along the way.
    ///
    /// ### Arguments
    /// * `layer_root` - Root inode data for the layer being searched
    /// * `path_segments` - Path components to traverse, as interned symbols
    /// * `path_inodes` - Vector to store inode data for each path segment traversed
    ///
    /// # Return Value
    /// Returns `Option<io::Result<bindings::stat64>>` where:
    /// - `Some(Ok(stat))` - Successfully found the file/directory and retrieved its stats
    /// - `Some(Err(e))` - Encountered an error during lookup that should be propagated:
    ///   - If error is `NotFound`, caller should try next layer
    ///   - For any other IO error, caller should stop searching entirely
    /// - `None` - Stop searching lower layers because either:
    ///   - Found a whiteout file for this path (file was deleted in this layer)
    ///   - Found an opaque directory marker (directory contents are masked in this layer)
    ///
    /// # Example Return Flow
    /// 1. If path exists: `Some(Ok(stat))`
    /// 2. If path has whiteout: `None`
    /// 3. If path not found: `Some(Err(NotFound))`
    /// 4. If directory has opaque marker: `None`
    /// 5. If IO error occurs: `Some(Err(io_error))`
    ///
    /// # Side Effects
    /// - Creates inodes for each path segment if they don't already exist
    /// - Updates path_inodes with inode data for each segment traversed
    /// - Increments reference counts for existing inodes that are reused
    ///
    /// # Path Resolution
    /// For a path like "foo/bar/baz", the function:
    /// 1. Starts at layer_root
    /// 2. Looks up "foo", checking for whiteouts/opaque markers
    /// 3. If "foo" exists, creates/reuses its inode and adds to path_inodes
    /// 4. Repeats for "bar" and "baz"
    /// 5. Returns stats for "baz" if found
    fn lookup_segment_by_segment(
        &self,
        layer_root: &Arc<InodeData>,
        path_segments: &[Symbol],
        path_inodes: &mut Vec<Arc<InodeData>>,
    ) -> Option<io::Result<(File, libc::stat64, u64)>> {
        let mut opaque_marker_found = false;

        // Start from layer root
        let root_file = match layer_root.file.try_clone() {
            Ok(file) => file,
            Err(e) => {
                return Some(Err(e));
            }
        };

        // Set current.
        let mut current = match Self::statx(root_file.as_raw_fd(), None) {
            Ok((stat, mnt_id)) => (root_file, stat, mnt_id),
            Err(e) => return Some(Err(e)),
        };

        // Traverse each path segment
        for (depth, segment) in path_segments.iter().enumerate() {
            // Get the current segment name and parent vol path
            let filenames = self.filenames.read().unwrap();
            let segment_name = filenames.get(*segment).unwrap();

            // Check for whiteout at current level
            match self.check_whiteout(current.0.as_raw_fd(), segment_name) {
                Ok(true) => {
                    return None; // Found whiteout, stop searching
                }
                Ok(false) => (), // No whiteout, continue
                Err(e) => {
                    return Some(Err(e));
                }
            }

            // Check for opaque marker at current level
            match self.check_opaque_marker(current.0.as_raw_fd()) {
                Ok(true) => {
                    opaque_marker_found = true;
                }
                Ok(false) => (),
                Err(e) => {
                    return Some(Err(e));
                }
            }

            let segment_name = segment_name.to_owned();

            drop(filenames); // Now safe to drop filenames lock

            match Self::statx(current.0.as_raw_fd(), Some(&segment_name)) {
                Ok((st, mnt_id)) => {
                    // Open the current segment
                    let new_file =
                        match Self::open_path_file_at(current.0.as_raw_fd(), &segment_name) {
                            Ok(file) => {
                                file
                            }
                            Err(e) => {
                                return Some(Err(e));
                            }
                        };

                    // Update parent for next iteration
                    current = match new_file.try_clone() {
                        Ok(file) => (file, st, mnt_id),
                        Err(e) => {
                            return Some(Err(e));
                        }
                    };

                    // Create or get inode for this path segment
                    let alt_key = InodeAltKey::new(st.st_ino, st.st_dev, mnt_id);
                    let inode_data = {
                        let inodes = self.inodes.read().unwrap();
                        if let Some(data) = inodes.get_alt(&alt_key) {
                            data.clone()
                        } else {
                            drop(inodes); // Drop read lock before write lock

                            let mut path = path_inodes[depth].path.clone();
                            path.push(*segment);

                            // Safe because we just opened this fd.
                            let (_, data) = self.create_inode(
                                new_file,
                                st.st_ino,
                                st.st_dev,
                                mnt_id,
                                path,
                                layer_root.layer_idx,
                            );

                            data
                        }
                    };

                    // Update path_inodes with the current segment's inode data
                    if (depth + 1) >= path_inodes.len() {
                        // Haven't seen this depth before, append
                        path_inodes.push(inode_data);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound && opaque_marker_found => {
                    // For example, for a lookup of /foo/bar/baz, where /foo/bar has an opaque marker,
                    // then if we cannot find /foo/bar/baz in the current layer, we cannot find it
                    // in any other layer as /foo/bar is masked.
                    return None;
                }
                Err(e) => {
                    return Some(Err(e));
                }
            }
        }

        Some(Ok(current))
    }

    /// Looks up a file or directory entry across multiple filesystem layers.
    ///
    /// This function starts from the specified upper layer (given by start_layer_idx) and searches downwards
    /// through the layers to locate the file represented by the provided path segments (an interned path).
    /// At each layer, it calls lookup_segment_by_segment to traverse the path step by step while handling
    /// whiteout files and opaque directory markers. If an entry is found in a layer, the function returns
    /// an Entry structure containing the file metadata along with a vector of InodeData for each path segment traversed.
    ///
    /// ## Arguments
    ///
    /// * `start_layer_idx` - The index of the starting layer (from the topmost, which may be the writable layer).
    /// * `path_segments` - A slice of interned symbols representing the path components to traverse.
    ///
    /// ## Returns
    ///
    /// On success, returns a tuple containing:
    /// - An Entry representing the located file or directory along with its attributes.
    /// - A vector of Arc<InodeData> corresponding to the inodes for each traversed path segment.
    ///
    /// ## Errors
    ///
    /// Returns an io::Error if:
    /// - The file is not found in any layer (ENOENT), or
    /// - An error occurs during the lookup process in one of the layers.
    fn lookup_layer_by_layer<'a>(
        &'a self,
        start_layer_idx: usize,
        path_segments: &[Symbol],
    ) -> io::Result<(Entry, Arc<InodeData>, Vec<Arc<InodeData>>)> {
        let mut path_inodes = vec![];

        // Start from the start_layer_idx and try each layer down to layer 0
        for layer_idx in (0..=start_layer_idx).rev() {
            let layer_root = self.get_layer_root(layer_idx)?;

            // If path_inodes has only the root inode or is empty, we need to restart the lookup with the new layer root.
            if path_inodes.len() < 2 {
                path_inodes = vec![layer_root.clone()];
            }

            match self.lookup_segment_by_segment(&layer_root, &path_segments, &mut path_inodes) {
                Some(Ok((file, st, mnt_id))) => {
                    let alt_key = InodeAltKey::new(st.st_ino, st.st_dev, mnt_id);

                    // Check if we already have this inode
                    let inodes = self.inodes.read().unwrap();
                    if let Some(data) = inodes.get_alt(&alt_key) {
                        return Ok((self.create_entry(data.inode, st), data.clone(), path_inodes));
                    }

                    drop(inodes);

                    // Open the path
                    let path = path_segments.to_vec();

                    // Create new inode
                    let (inode, data) =
                        self.create_inode(file, st.st_ino, st.st_dev, mnt_id, path, layer_idx);
                    path_inodes.push(data.clone());

                    return Ok((self.create_entry(inode, st), data, path_inodes));
                }
                Some(Err(e)) if e.kind() == io::ErrorKind::NotFound => {
                    // Continue to check lower layers
                    continue;
                }
                Some(Err(e)) => {
                    return Err(e);
                }
                None => {
                    // Hit a whiteout or opaque marker, stop searching lower layers
                    return Err(io::Error::from_raw_os_error(libc::ENOENT));
                }
            }
        }

        // Not found in any layer
        Err(io::Error::from_raw_os_error(libc::ENOENT))
    }

    /// Performs a lookup operation
    pub(crate) fn do_lookup(
        &self,
        parent: Inode,
        name: &CStr,
    ) -> io::Result<(Entry, Vec<Arc<InodeData>>)> {
        // Get the parent inode data
        let parent_data = self.get_inode_data(parent)?;

        // Create path segments for lookup by appending the new name
        let mut path_segments = parent_data.path.clone();
        let symbol = self.intern_name(name)?;
        path_segments.push(symbol);

        let (mut entry, child_data, path_inodes) =
            self.lookup_layer_by_layer(parent_data.layer_idx, &path_segments)?;

        // Set the submount flag if the endirectory is a mount point
        let mut attr_flags = 0;
        if (entry.attr.st_mode & libc::S_IFMT) == libc::S_IFDIR
            && self.announce_submounts.load(Ordering::Relaxed)
            && (child_data.dev != parent_data.dev || child_data.mnt_id != parent_data.mnt_id)
        {
            attr_flags |= fuse::ATTR_SUBMOUNT;
        }

        entry.attr_flags = attr_flags;

        Ok((entry, path_inodes))
    }

    /// Copies up a file or directory from a lower layer to the top layer
    pub(crate) fn copy_up(&self, path_inodes: &[Arc<InodeData>]) -> io::Result<()> {
        // Get the top layer root
        let top_layer_idx = self.get_top_layer_idx();
        let top_layer_root = self.get_layer_root(top_layer_idx)?;

        // Start from root and copy up each segment that's not in the top layer
        let mut parent = top_layer_root.file.try_clone()?;

        // Skip the root inode
        for inode_data in path_inodes.iter().skip(1) {
            // Skip if this segment is already in the top layer
            if inode_data.layer_idx == top_layer_idx {
                parent = inode_data.file.try_clone()?;
                continue;
            }

            // Get the current segment name
            let segment_name = {
                let name = inode_data.path.last().unwrap();
                let filenames = self.filenames.read().unwrap();
                filenames.get(*name).unwrap().to_owned()
            };

            let (src_stat, _) = Self::statx(inode_data.file.as_raw_fd(), None)?;
            let file_type = src_stat.st_mode & libc::S_IFMT;

            // Copy up the file
            match file_type {
                libc::S_IFREG => {
                    // Open source file with O_RDONLY
                    let src_file = self.open_inode(inode_data.inode, libc::O_RDONLY)?;

                    // Open destination file with O_WRONLY | O_CREAT
                    let dst_file = Self::open_file_at(
                        parent.as_raw_fd(),
                        &segment_name,
                        libc::O_WRONLY | libc::O_CREAT,
                    )?;

                    // Try to use FICLONE ioctl for CoW copying first (works on modern Linux filesystems like Btrfs, XFS, etc.)
                    let result = unsafe {
                        libc::ioctl(dst_file.as_raw_fd(), FICLONE as _, src_file.as_raw_fd())
                    };

                    if result < 0 {
                        debug!("FICLONE failed, falling back to regular copy");
                        let err = io::Error::last_os_error();
                        // If FICLONE fails (e.g., across filesystems), fall back to regular copy
                        if err.raw_os_error() == Some(libc::EXDEV)
                            || err.raw_os_error() == Some(libc::EINVAL)
                            || err.raw_os_error() == Some(libc::ETXTBSY)
                            || err.raw_os_error() == Some(libc::EOPNOTSUPP)
                        {
                            // Fall back to regular copy
                            self.copy_file_contents(
                                src_file.as_raw_fd(),
                                dst_file.as_raw_fd(),
                                (src_stat.st_mode & 0o777) as u32,
                            )?;
                        } else {
                            return Err(err);
                        }
                    }
                }
                libc::S_IFDIR => {
                    // Directory: just create it with the same permissions
                    unsafe {
                        if libc::mkdirat(
                            parent.as_raw_fd(),
                            segment_name.as_ptr(),
                            src_stat.st_mode & 0o777,
                        ) < 0
                        {
                            return Err(io::Error::last_os_error());
                        }
                    }
                }
                libc::S_IFLNK => {
                    // Symbolic link: read target and recreate link
                    let mut buf = vec![0u8; libc::PATH_MAX as usize];
                    let len = unsafe {
                        libc::readlinkat(
                            inode_data.file.as_raw_fd(),
                            EMPTY_CSTR.as_ptr(),
                            buf.as_mut_ptr() as *mut _,
                            buf.len(),
                        )
                    };

                    if len < 0 {
                        return Err(io::Error::last_os_error());
                    }

                    buf.truncate(len as usize);

                    unsafe {
                        if libc::symlinkat(
                            buf.as_ptr() as *const _,
                            parent.as_raw_fd(),
                            segment_name.as_ptr(),
                        ) < 0
                        {
                            return Err(io::Error::last_os_error());
                        }

                        if libc::fchmodat(
                            parent.as_raw_fd(),
                            segment_name.as_ptr(),
                            src_stat.st_mode & 0o777,
                            0,
                        ) < 0
                        {
                            return Err(io::Error::last_os_error());
                        }
                    }
                }
                _ => {
                    // Other types (devices, sockets, etc.) are not supported yet.
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "unsupported file type for copy up",
                    ));
                }
            }

            // Update parent for next iteration
            let child = Self::open_path_file_at(parent.as_raw_fd(), &segment_name)?;
            let (new_stat, new_mnt_id) = Self::statx(child.as_raw_fd(), None)?;
            parent = child.try_clone()?;

            // Update the inode entry to point to the new copy in the top layer
            let alt_key = InodeAltKey::new(new_stat.st_ino, new_stat.st_dev, new_mnt_id);
            let mut inodes = self.inodes.write().unwrap();

            // Create new inode data with updated dev/ino/layer_idx but same refcount
            let new_data = Arc::new(InodeData {
                inode: inode_data.inode,
                file: child,
                dev: new_stat.st_dev,
                mnt_id: new_mnt_id,
                refcount: AtomicU64::new(inode_data.refcount.load(Ordering::SeqCst)),
                path: inode_data.path.clone(),
                layer_idx: top_layer_idx,
            });

            // Replace the old entry with the new one
            inodes.insert(inode_data.inode, alt_key, new_data);
        }

        Ok(())
    }

    /// Helper method to copy file contents when clonefile is not available or fails
    fn copy_file_contents(&self, src_fd: RawFd, dst_fd: RawFd, mode: u32) -> io::Result<()> {
        unsafe {
            // Copy file contents
            let mut buf = [0u8; 8192];
            loop {
                let n_read = libc::read(src_fd, buf.as_mut_ptr() as *mut _, buf.len());
                if n_read <= 0 {
                    break;
                }
                let mut pos = 0;
                while pos < n_read {
                    let n_written = libc::write(
                        dst_fd,
                        buf.as_ptr().add(pos as usize) as *const _,
                        (n_read - pos) as usize,
                    );
                    if n_written <= 0 {
                        return Err(io::Error::last_os_error());
                    }
                    pos += n_written;
                }
            }

            // Explicitly set permissions to match source file
            // This will override any effects from the umask
            if libc::fchmod(dst_fd, mode as libc::mode_t) < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        Ok(())
    }

    /// Ensures the file is in the top layer by copying it up if necessary.
    ///
    /// This function:
    /// 1. Checks if the file is already in the top layer
    /// 2. If not, looks up the complete path to the file
    /// 3. Copies the file and all its parent directories to the top layer
    /// 4. Returns the inode data for the copied file
    ///
    /// ### Arguments
    /// * `inode_data` - The inode data for the file to copy up
    ///
    /// ### Returns
    /// * `Ok(InodeData)` - The inode data for the file in the top layer
    /// * `Err(io::Error)` - If the copy-up operation fails
    fn ensure_top_layer(&self, inode_data: Arc<InodeData>) -> io::Result<Arc<InodeData>> {
        let top_layer_idx = self.get_top_layer_idx();

        // If already in top layer, return early
        if inode_data.layer_idx == top_layer_idx {
            return Ok(inode_data);
        }

        // Build the path segments
        let path_segments = inode_data.path.clone();

        // Lookup the file to get all path inodes
        let (_, _, path_inodes) = self.lookup_layer_by_layer(top_layer_idx, &path_segments)?;

        // Copy up the file
        self.copy_up(&path_inodes)?;

        // Get the inode data for the copied file
        self.get_inode_data(inode_data.inode)
    }

    /// Creates a whiteout file for a given parent directory and name.
    /// This is used to hide files that exist in lower layers.
    ///
    /// # Arguments
    /// * `parent` - The inode of the parent directory
    /// * `name` - The name of the file to create a whiteout for
    ///
    /// # Returns
    /// * `Ok(())` if the whiteout was created successfully
    /// * `Err(io::Error)` if there was an error creating the whiteout
    fn create_whiteout_for_lower(&self, parent: Inode, name: &CStr) -> io::Result<()> {
        if let Ok((_, mut path_inodes)) = self.do_lookup(parent, name) {
            // Copy up the parent directory if needed
            path_inodes.pop();
            self.copy_up(&path_inodes)?;
            let parent_fd = self.get_inode_data(parent)?.file.as_raw_fd();

            let whiteout_cpath = self.create_whiteout_path(name)?;
            let fd = unsafe {
                libc::openat(
                    parent_fd,
                    whiteout_cpath.as_ptr(),
                    libc::O_CREAT | libc::O_WRONLY | libc::O_EXCL | libc::O_NOFOLLOW,
                    0o000, // Whiteout files have no permissions
                )
            };

            if fd < 0 {
                return Err(io::Error::last_os_error());
            }

            unsafe { libc::close(fd) };
        }

        Ok(())
    }

    /// Temporarily changes the effective UID and GID of the current thread to the requested values using RAII guards.
    ///
    /// If the requested UID or GID is 0 (root) or already matches the current effective UID/GID (as stored in my_uid and my_gid),
    /// no credential switching is performed and None is returned for that component.
    ///
    /// When credential switching is performed, an RAII guard (ScopedUid or ScopedGid) is returned that will restore the
    /// effective UID or GID to root (0) when dropped. If the process lacks the required capability (CAP_SETUID or CAP_SETGID)
    /// and the requested UID/GID does not match the current credentials, the function returns an EPERM error.
    ///
    /// # Arguments
    /// * `uid` - The requested user ID to switch to.
    /// * `gid` - The requested group ID to switch to.
    ///
    /// # Returns
    /// A tuple `(Option<ScopedUid>, Option<ScopedGid>)` where:
    /// - `Option<ScopedUid>` is Some if the effective UID was changed, or None if no change was needed.
    /// - `Option<ScopedGid>` is Some if the effective GID was changed, or None if no change was needed.
    ///
    /// # Errors
    /// Returns EPERM if the process lacks the required capability to change to a non-matching UID or GID.
    fn set_scoped_credentials(
        &self,
        uid: libc::uid_t,
        gid: libc::gid_t,
    ) -> io::Result<(Option<ScopedUid>, Option<ScopedGid>)> {
        // Handle GID changes first since changing UID to non-root may prevent GID changes
        let scoped_gid = if gid == 0 || self.my_gid == Some(gid) {
            // If the requested GID is 0 (root) or matches our current GID,
            // no credential switching is needed.
            None
        } else if self.my_gid.is_some() {
            // Process doesn't have CAP_SETGID capability and the requested GID
            // does not match our current GID, so we cannot switch.
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        } else {
            // Process has CAP_SETGID capability, attempt to switch to the requested GID
            Some(ScopedGid::new(gid)?)
        };

        // Handle UID changes after GID
        let scoped_uid = if uid == 0 || self.my_uid == Some(uid) {
            // If the requested UID is 0 (root) or matches our current UID,
            // no credential switching is needed.
            None
        } else if self.my_uid.is_some() {
            // Process doesn't have CAP_SETUID capability and the requested UID
            // does not match our current UID, so we cannot switch.
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        } else {
            // Process has CAP_SETUID capability, attempt to switch to the requested UID
            Some(ScopedUid::new(uid)?)
        };

        Ok((scoped_uid, scoped_gid))
    }

    /// Decrements the reference count for an inode and removes it if the count reaches zero
    fn do_forget(&self, inode: Inode, count: u64) {
        let mut inodes = self.inodes.write().unwrap();
        if let Some(data) = inodes.get(&inode) {
            // Acquiring the write lock on the inode map prevents new lookups from incrementing the
            // refcount but there is the possibility that a previous lookup already acquired a
            // reference to the inode data and is in the process of updating the refcount so we need
            // to loop here until we can decrement successfully.
            loop {
                let refcount = data.refcount.load(Ordering::Relaxed);

                // Saturating sub because it doesn't make sense for a refcount to go below zero and
                // we don't want misbehaving clients to cause integer overflow.
                let new_count = refcount.saturating_sub(count);

                if data
                    .refcount
                    .compare_exchange(refcount, new_count, Ordering::Release, Ordering::Relaxed)
                    .unwrap()
                    == refcount
                {
                    if new_count == 0 {
                        // We just removed the last refcount for this inode. There's no need for an
                        // acquire fence here because we hold a write lock on the inode map and any
                        // thread that is waiting to do a forget on the same inode will have to wait
                        // until we release the lock. So there's is no other release store for us to
                        // synchronize with before deleting the entry.
                        inodes.remove(&inode);
                    }
                    break;
                }
            }
        }
    }

    /// Performs an open operation
    fn do_open(&self, inode: Inode, mut flags: u32) -> io::Result<(Option<Handle>, OpenOptions)> {
        if !self.cap_fowner {
            // O_NOATIME can only be used with CAP_FOWNER or if we are the file
            // owner. Not worth checking the latter, just drop it if we don't
            // have the cap. This makes overlayfs mounts with virtiofs lower dirs
            // work.
            flags &= !(libc::O_NOATIME as u32);
        }

        // Get the inode data
        let inode_data = self.get_inode_data(inode)?;

        // Ensure the file is in the top layer
        let inode_data = self.ensure_top_layer(inode_data)?;

        // Open the file with the appropriate flags and generate a new unique handle ID
        let file = RwLock::new(self.open_inode(inode_data.inode, flags as i32)?);
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);

        // Create handle data structure with file and empty dirstream
        let data = HandleData {
            inode,
            file,
            exported: Default::default(),
        };

        // Store the handle data in the handles map
        self.handles.write().unwrap().insert(handle, Arc::new(data));

        // Set up OpenOptions based on the cache policy configuration
        let mut opts = OpenOptions::empty();
        match self.config.cache_policy {
            // For CachePolicy::Never, set DIRECT_IO to bypass kernel caching for files (not directories)
            CachePolicy::Never => opts.set(
                OpenOptions::DIRECT_IO,
                flags & (libc::O_DIRECTORY as u32) == 0,
            ),

            // For CachePolicy::Always, set different caching options based on whether it's a file or directory
            CachePolicy::Always => {
                if flags & (libc::O_DIRECTORY as u32) == 0 {
                    // For files: KEEP_CACHE maintains kernel cache between open/close operations
                    opts |= OpenOptions::KEEP_CACHE;
                } else {
                    // For directories: CACHE_DIR enables caching of directory entries
                    opts |= OpenOptions::CACHE_DIR;
                }
            }

            // For CachePolicy::Auto, use default caching behavior
            _ => {}
        };

        // Return the handle and options
        Ok((Some(handle), opts))
    }

    /// Performs a release operation
    fn do_release(&self, inode: Inode, handle: Handle) -> io::Result<()> {
        let mut handles = self.handles.write().unwrap();

        if let btree_map::Entry::Occupied(e) = handles.entry(handle) {
            if e.get().inode == inode {
                if e.get().exported.load(Ordering::Relaxed) {
                    self.config
                        .export_table
                        .as_ref()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .remove(&(self.config.export_fsid, handle));
                }

                // We don't need to close the file here because that will happen automatically when
                // the last `Arc` is dropped.
                e.remove();
                return Ok(());
            }
        }

        Err(ebadf())
    }

    /// Performs a mkdir operation
    fn do_mkdir(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        if extensions.secctx.is_some() {
            unimplemented!("SECURITY_CTX is not supported and should not be used by the guest");
        }

        // Set the credentials for the operation
        let (_uid, _gid) = self.set_scoped_credentials(ctx.uid, ctx.gid)?;

        // Check if an entry with the same name already exists in the parent directory
        match self.do_lookup(parent, name) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Entry already exists",
                ))
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                // Expected ENOENT means it does not exist, so continue.
            }
            Err(e) => return Err(e),
        }

        // Ensure parent directory is in the top layer
        let parent_data = self.get_inode_data(parent)?;
        let parent_data = self.ensure_top_layer(parent_data)?;

        // Get the parent file descriptor
        let parent_fd = parent_data.file.as_raw_fd();

        // Create the directory
        let res = unsafe { libc::mkdirat(parent_fd, name.as_ptr(), mode & !umask) };
        if res == 0 {
            let file = Self::open_path_file_at(parent_fd, name)?;
            let (stat, mnt_id) = Self::statx(file.as_raw_fd(), None)?;

            let mut path = parent_data.path.clone();
            path.push(self.intern_name(name)?);

            // Create the inode for the newly created directory
            let (inode, _) = self.create_inode(
                file,
                stat.st_ino,
                stat.st_dev,
                mnt_id,
                path,
                parent_data.layer_idx,
            );

            // Create the entry for the newly created directory
            let entry = self.create_entry(inode, stat);

            return Ok(entry);
        }

        // Return the error
        Err(io::Error::last_os_error())
    }

    /// Performs an unlink operation
    fn do_unlink(&self, parent: Inode, name: &CStr, flags: libc::c_int) -> io::Result<()> {
        let top_layer_idx = self.get_top_layer_idx();
        let (entry, _) = self.do_lookup(parent, name)?;

        // If the inode is in the top layer. the parent will also be in the top layer, we need to unlink it.
        let entry_data = self.get_inode_data(entry.inode)?;
        if entry_data.layer_idx == top_layer_idx {
            let parent_fd = self.get_inode_data(parent)?.file.as_raw_fd();

            // Remove the inode from the overlayfs
            let res = unsafe { libc::unlinkat(parent_fd, name.as_ptr(), flags) };
            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // If after an unlink, the entry still exists in a lower layer, we need to add a whiteout
        self.create_whiteout_for_lower(parent, name)?;

        Ok(())
    }

    /// Returns an iterator over all valid entries in the directory across all layers.
    ///
    /// Note: OverlayFs is a high-level, layered filesystem. A simple readdir on a single directory does not produce the complete view.
    /// This function traverses the directory across multiple layers, merging entries while handling duplicates,
    /// whiteout files, and opaque markers.
    ///
    /// ## Arguments
    /// * `dir` - The inode of the directory to iterate over.
    /// * `add_entry` - A callback function that processes each directory entry. If the callback returns 0,
    ///                it signals that the directory buffer is full and iteration should stop.
    ///
    /// ## Returns
    /// * `Ok(())` if the directory was iterated successfully.
    /// * `Err(io::Error)` if an error occurred during iteration.
    pub(super) fn process_dir_entries<F>(&self, dir: Inode, mut add_entry: F) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        // Local state to track iteration over layers
        struct LazyReaddirState {
            current_layer: isize, // current layer (top-down)
            inode_data: Option<Arc<InodeData>>,
            current_iter: Option<std::fs::ReadDir>,
            seen: HashSet<Vec<u8>>,
        }

        let inode_data = self.get_inode_data(dir)?;
        let top_layer = self.get_top_layer_idx() as isize;
        let path = inode_data.path.clone();
        let mut state = LazyReaddirState {
            current_layer: top_layer,
            inode_data: None,
            current_iter: None,
            seen: HashSet::new(),
        };

        let mut current_offset = 0u64;
        let mut opaque_marker_found = false;
        loop {
            // If no current iterator, attempt to initialize one for the current layer
            if state.current_iter.is_none() {
                if state.current_layer < 0 {
                    break; // All layers exhausted
                }

                let layer_root = self.get_layer_root(state.current_layer as usize)?;
                let mut path_inodes = vec![layer_root.clone()];

                match self.lookup_segment_by_segment(&layer_root, &path, &mut path_inodes) {
                    Some(Ok(_)) => {
                        let last_inode = path_inodes.last().unwrap();
                        let path = Self::data_to_path(last_inode)?;
                        let dir_str = path.as_c_str().to_str().map_err(|_| {
                            io::Error::new(io::ErrorKind::Other, "Invalid path string")
                        })?;

                        state.inode_data = Some(last_inode.clone());
                        state.current_iter = Some(std::fs::read_dir(dir_str)?);
                    }
                    Some(Err(e)) if e.kind() == io::ErrorKind::NotFound => {
                        state.current_layer -= 1;
                        continue;
                    }
                    Some(Err(e)) => return Err(e),
                    None => {
                        state.current_layer = -1;
                        continue;
                    }
                }
            }

            if let Some(iter) = state.current_iter.as_mut() {
                if let Some(entry_result) = iter.next() {
                    let entry = entry_result?;
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();

                    if state.seen.contains(name.as_bytes()) {
                        continue;
                    }

                    // Handle opaque marker and whiteout files
                    if name_str == OPAQUE_MARKER {
                        // Opaque marker found; mark it and skip this entry
                        opaque_marker_found = true;
                        continue;
                    } else if name_str.starts_with(WHITEOUT_PREFIX) {
                        // Whiteout file; skip it
                        let actual = &name_str[WHITEOUT_PREFIX.len()..];
                        state.seen.insert(actual.as_bytes().to_vec());
                        continue;
                    } else {
                        state.seen.insert(name.as_bytes().to_vec());
                    }

                    let metadata = entry.metadata()?;
                    let mode = metadata.mode() as u32;
                    let s_ifmt = libc::S_IFMT as u32;
                    let type_ = if mode & s_ifmt == (libc::S_IFDIR as u32) {
                        libc::DT_DIR
                    } else if mode & s_ifmt == (libc::S_IFREG as u32) {
                        libc::DT_REG
                    } else if mode & s_ifmt == (libc::S_IFLNK as u32) {
                        libc::DT_LNK
                    } else if mode & s_ifmt == (libc::S_IFIFO as u32) {
                        libc::DT_FIFO
                    } else if mode & s_ifmt == (libc::S_IFCHR as u32) {
                        libc::DT_CHR
                    } else if mode & s_ifmt == (libc::S_IFBLK as u32) {
                        libc::DT_BLK
                    } else if mode & s_ifmt == (libc::S_IFSOCK as u32) {
                        libc::DT_SOCK
                    } else {
                        libc::DT_UNKNOWN
                    };

                    current_offset += 1;

                    let dir_entry = DirEntry {
                        ino: metadata.ino(),
                        offset: current_offset,
                        type_: type_ as u32,
                        name: name.as_bytes(),
                    };

                    if add_entry(dir_entry)? == 0 {
                        return Ok(());
                    }
                } else {
                    state.current_iter = None;
                    if opaque_marker_found {
                        break;
                    }
                    state.current_layer -= 1;
                    continue;
                }
            }
        }

        Ok(())
    }

    /// Reads directory entries for the given inode by merging entries from all underlying layers.
    ///
    /// Unlike conventional filesystems that simply call readdir on a directory file descriptor,
    /// OverlayFs must aggregate entries from multiple layers. The `offset` parameter specifies the starting
    /// index in the merged list of directory entries. The provided `add_entry` callback is invoked for each
    /// entry; a return value of 0 indicates that the directory buffer is full and reading should cease.
    ///
    /// NOTE: The current implementation of offset does not entirely follow FUSE expected behaviors.
    /// Changes to entries in the write layer can affect the offset, potentially causing inconsistencies
    /// in directory listing between calls.
    ///
    /// TODO: Implement a more robust offset handling mechanism that maintains consistency even when
    /// the underlying directory structure changes. One way is making offset a composite value of
    /// layer (1 MSB) + offset (7 LSB). This will also require having multiple open dirs from lower layers
    /// in [HandleData].
    pub(super) fn do_readdir<F>(
        &self,
        inode: Inode,
        size: u32,
        offset: u64,
        mut add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        if size == 0 {
            return Ok(());
        }

        let mut current_offset = 0u64;
        self.process_dir_entries(inode, |entry| {
            if current_offset < offset {
                current_offset += 1;
                return Ok(1);
            }

            add_entry(entry)
        })
    }

    fn do_create(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        flags: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<(Entry, Option<Handle>, OpenOptions)> {
        if extensions.secctx.is_some() {
            unimplemented!("SECURITY_CTX is not supported and should not be used by the guest");
        }

        // Set the credentials for the operation
        let (_uid, _gid) = self.set_scoped_credentials(ctx.uid, ctx.gid)?;

        // Check if an entry with the same name already exists in the parent directory
        match self.do_lookup(parent, name) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Entry already exists",
                ))
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                // Expected ENOENT means it does not exist, so continue.
            }
            Err(e) => return Err(e),
        }

        // Ensure parent directory is in the top layer
        let parent_data = self.get_inode_data(parent)?;
        let parent_data = self.ensure_top_layer(parent_data)?;

        // Get the parent file descriptor
        let parent_fd = parent_data.file.as_raw_fd();

        // Safe because this doesn't modify any memory and we check the return value. We don't
        // really check `flags` because if the kernel can't handle poorly specified flags then we
        // have much bigger problems.
        let fd = unsafe {
            libc::openat(
                parent_fd,
                name.as_ptr(),
                flags as i32 | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                mode & !(umask & 0o777),
            )
        };

        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        let (stat, mnt_id) = Self::statx(fd, None)?;

        let mut path = parent_data.path.clone();
        path.push(self.intern_name(name)?);

        // Create the inode for the newly created file
        let file = unsafe { File::from_raw_fd(fd) };
        let (inode, _) = self.create_inode(
            file.try_clone()?,
            stat.st_ino,
            stat.st_dev,
            mnt_id,
            path,
            parent_data.layer_idx,
        );

        // Create the entry for the newly created file
        let entry = self.create_entry(inode, stat);

        // Create the handle for the newly created file
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);
        let data = HandleData {
            inode: entry.inode,
            file: RwLock::new(file),
            exported: Default::default(),
        };

        self.handles.write().unwrap().insert(handle, Arc::new(data));

        let mut opts = OpenOptions::empty();
        match self.config.cache_policy {
            CachePolicy::Never => opts |= OpenOptions::DIRECT_IO,
            CachePolicy::Always => opts |= OpenOptions::KEEP_CACHE,
            _ => {}
        };

        Ok((entry, Some(handle), opts))
    }

    fn do_getattr(&self, inode: Inode) -> io::Result<(libc::stat64, Duration)> {
        let fd = self.get_inode_data(inode)?.file.as_raw_fd();
        let (st, _) = Self::statx(fd, None)?;

        Ok((st, self.config.attr_timeout))
    }

    fn do_rename(
        &self,
        old_parent: Inode,
        old_name: &CStr,
        new_parent: Inode,
        new_name: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        // Copy up the old path to the top layer if not already in the top layer
        let (_, old_path_inodes) = self.do_lookup(old_parent, old_name)?;
        self.copy_up(&old_path_inodes)?;
        let old_parent_data = self.get_inode_data(old_parent)?;

        // Copy up the new parent to the top layer if not already in the top layer
        let new_parent_data = self.ensure_top_layer(self.get_inode_data(new_parent)?)?;

        // Perform the rename
        let res = unsafe {
            #[cfg(any(target_env = "gnu", target_env = "musl"))]
            {
                libc::renameat2(
                    old_parent_data.file.as_raw_fd(),
                    old_name.as_ptr(),
                    new_parent_data.file.as_raw_fd(),
                    new_name.as_ptr(),
                    flags,
                )
            }
        };

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        // After successful rename, check if we need to add a whiteout for the old path
        self.create_whiteout_for_lower(old_parent, old_name)?;

        Ok(())
    }

    fn do_mknod(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        if extensions.secctx.is_some() {
            unimplemented!("SECURITY_CTX is not supported and should not be used by the guest");
        }

        // Set the credentials for the operation
        let (_uid, _gid) = self.set_scoped_credentials(ctx.uid, ctx.gid)?;

        // Check if an entry with the same name already exists in the parent directory
        match self.do_lookup(parent, name) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Entry already exists",
                ))
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                // Expected ENOENT means it does not exist, so continue.
            }
            Err(e) => return Err(e),
        }

        // Ensure parent directory is in the top layer
        let parent_data = self.get_inode_data(parent)?;
        let parent_data = self.ensure_top_layer(parent_data)?;

        // Get the parent file descriptor
        let parent_fd = parent_data.file.as_raw_fd();

        // Create the node device
        let res = unsafe {
            libc::mknodat(
                parent_fd,
                name.as_ptr(),
                (mode & !umask) as libc::mode_t,
                u64::from(rdev),
            )
        };

        if res == 0 {
            let file = Self::open_path_file_at(parent_fd, name)?;
            let (stat, mnt_id) = Self::statx(file.as_raw_fd(), None)?;

            let mut path = parent_data.path.clone();
            path.push(self.intern_name(name)?);

            // Create the inode for the newly created directory
            let (inode, _) = self.create_inode(
                file,
                stat.st_ino,
                stat.st_dev,
                mnt_id,
                path,
                parent_data.layer_idx,
            );

            // Create the entry for the newly created directory
            let entry = self.create_entry(inode, stat);

            return Ok(entry);
        }

        // Return the error
        Err(io::Error::last_os_error())
    }

    fn do_link(&self, inode: Inode, newparent: Inode, newname: &CStr) -> io::Result<Entry> {
        // Get the fd for the source file.
        let inode_data = self.get_inode_data(inode)?;

        // Copy up the source file to the top layer if needed
        let inode_data = self.ensure_top_layer(inode_data)?;
        let old_fd_str = Self::data_to_fd_str(&inode_data)?;

        // Extraneous check to ensure the source file is not a symlink
        let stat = Self::statx(inode_data.file.as_raw_fd(), None)?.0;
        if stat.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot link to a symlink",
            ));
        }

        // Get and ensure new parent is in top layer
        let new_parent_data = self.ensure_top_layer(self.get_inode_data(newparent)?)?;
        let new_parent_fd = new_parent_data.file.as_raw_fd();

        // Safety: It is expected that old_fd_str has been checked by the kernel to not be a symlink.
        let res = unsafe {
            libc::linkat(
                self.proc_self_fd.as_raw_fd(),
                old_fd_str.as_ptr(),
                new_parent_fd,
                newname.as_ptr(),
                libc::AT_SYMLINK_FOLLOW, // Follow is needed to handle /proc/self/fd/ symlink
            )
        };

        if res == 0 {
            let file = Self::open_path_file_at(new_parent_fd, newname)?;
            let (stat, mnt_id) = Self::statx(file.as_raw_fd(), None)?;

            let mut path = new_parent_data.path.clone();
            path.push(self.intern_name(newname)?);

            // Create the inode for the newly created directory
            let (inode, _) = self.create_inode(
                file,
                stat.st_ino,
                stat.st_dev,
                mnt_id,
                path,
                new_parent_data.layer_idx,
            );

            // Create the entry for the newly created directory
            let entry = self.create_entry(inode, stat);

            return Ok(entry);
        }

        // Return the error
        Err(io::Error::last_os_error())
    }

    fn do_symlink(
        &self,
        ctx: Context,
        linkname: &CStr,
        parent: Inode,
        name: &CStr,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        if extensions.secctx.is_some() {
            unimplemented!("SECURITY_CTX is not supported and should not be used by the guest");
        }

        // Set the credentials for the operation
        let (_uid, _gid) = self.set_scoped_credentials(ctx.uid, ctx.gid)?;

        // Check if an entry with the same name already exists in the parent directory
        match self.do_lookup(parent, name) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Entry already exists",
                ))
            }
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => {
                // Expected ENOENT means it does not exist, so continue.
            }
            Err(e) => return Err(e),
        }

        // Ensure parent directory is in the top layer
        let parent_data = self.get_inode_data(parent)?;
        let parent_data = self.ensure_top_layer(parent_data)?;

        // Get the parent file descriptor
        let parent_fd = parent_data.file.as_raw_fd();

        // Create the node device
        let res = unsafe { libc::symlinkat(linkname.as_ptr(), parent_fd, name.as_ptr()) };

        if res == 0 {
            let file = Self::open_path_file_at(parent_fd, name)?;
            let (stat, mnt_id) = Self::statx(file.as_raw_fd(), None)?;

            let mut path = parent_data.path.clone();
            path.push(self.intern_name(name)?);

            // Create the inode for the newly created directory
            let (inode, _) = self.create_inode(
                file,
                stat.st_ino,
                stat.st_dev,
                mnt_id,
                path,
                parent_data.layer_idx,
            );

            // Create the entry for the newly created directory
            let entry = self.create_entry(inode, stat);

            return Ok(entry);
        }

        // Return the error
        Err(io::Error::last_os_error())
    }

    fn do_readlink(&self, inode: Inode) -> io::Result<Vec<u8>> {
        // Get the path for this inode
        let inode_data = self.get_inode_data(inode)?;

        // Allocate a buffer for the link target
        let mut buf = vec![0; libc::PATH_MAX as usize];

        // Safe because this will only modify the contents of `buf` and we check the return value.
        let res = unsafe {
            libc::readlinkat(
                inode_data.file.as_raw_fd(),
                EMPTY_CSTR.as_ptr(),
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
            )
        };

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        // Resize the buffer to the actual length of the link target
        buf.resize(res as usize, 0);
        Ok(buf)
    }

    fn do_setxattr(&self, inode: Inode, name: &CStr, value: &[u8], flags: u32) -> io::Result<()> {
        // Check if extended attributes are enabled
        if !self.config.xattr {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }

        // Get the inode data
        let inode_data = self.get_inode_data(inode)?;

        // Ensure the file is in the top layer before modifying attributes
        let inode_data = self.ensure_top_layer(inode_data)?;

        // The f{set,get,remove,list}xattr functions don't work on an fd opened with `O_PATH` so we
        // need to get a new fd. This doesn't work for symlinks, so we use the l* family of
        // functions in that case.
        let res =
            match self.open_inode_or_path(inode_data.inode, libc::O_RDONLY | libc::O_NONBLOCK)? {
                FileOrPath::File(file) => {
                    // Safe because this doesn't modify any memory and we check the return value.
                    unsafe {
                        libc::fsetxattr(
                            file.as_raw_fd(),
                            name.as_ptr(),
                            value.as_ptr() as *const libc::c_void,
                            value.len(),
                            flags as libc::c_int,
                        )
                    }
                }
                FileOrPath::Path(path) => {
                    // Safe because this doesn't modify any memory and we check the return value.
                    unsafe {
                        libc::lsetxattr(
                            path.as_ptr(),
                            name.as_ptr(),
                            value.as_ptr() as *const libc::c_void,
                            value.len(),
                            flags as libc::c_int,
                        )
                    }
                }
            };
            
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn do_getxattr(&self, inode: Inode, name: &CStr, size: u32) -> io::Result<GetxattrReply> {
        // Check if extended attributes are enabled
        if !self.config.xattr {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }

        // Don't allow getting attributes for init
        if inode == self.init_inode {
            return Err(io::Error::from_raw_os_error(libc::ENODATA));
        }

        // Safe because this will only modify the contents of `buf`
        let mut buf = vec![0; size as usize];

        // The f{set,get,remove,list}xattr functions don't work on an fd opened with `O_PATH` so we
        // need to get a new fd. This doesn't work for symlinks, so we use the l* family of
        // functions in that case.
        let res = match self.open_inode_or_path(inode, libc::O_RDONLY | libc::O_NONBLOCK)? {
            FileOrPath::File(file) => {
                // Safe because this will only modify the contents of `buf`.
                unsafe {
                    libc::fgetxattr(
                        file.as_raw_fd(),
                        name.as_ptr(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        size as libc::size_t,
                    )
                }
            }
            FileOrPath::Path(path) => {
                // Safe because this will only modify the contents of `buf`.
                unsafe {
                    libc::lgetxattr(
                        path.as_ptr(),
                        name.as_ptr(),
                        buf.as_mut_ptr() as *mut libc::c_void,
                        size as libc::size_t,
                    )
                }
            }
        };

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        if size == 0 {
            Ok(GetxattrReply::Count(res as u32))
        } else {
            // Truncate the buffer to the actual length of the value
            buf.resize(res as usize, 0);
            Ok(GetxattrReply::Value(buf))
        }
    }

    fn do_listxattr(&self, inode: Inode, size: u32) -> io::Result<ListxattrReply> {
        // Check if extended attributes are enabled
        if !self.config.xattr {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }

        // Don't allow getting attributes for init
        if inode == self.init_inode {
            return Err(io::Error::from_raw_os_error(libc::ENODATA));
        }

        // Safe because this will only modify the contents of `buf`
        let mut buf = vec![0; size as usize];

        // The f{set,get,remove,list}xattr functions don't work on an fd opened with `O_PATH` so we
        // need to get a new fd. This doesn't work for symlinks, so we use the l* family of
        // functions in that case.
        let res = match self.open_inode_or_path(inode, libc::O_RDONLY | libc::O_NONBLOCK)? {
            FileOrPath::File(file) => {
                // Safe because this will only modify the contents of `buf`.
                unsafe {
                    libc::flistxattr(
                        file.as_raw_fd(),
                        buf.as_mut_ptr() as *mut libc::c_char,
                        size as libc::size_t,
                    )
                }
            }
            FileOrPath::Path(path) => {
                // Safe because this will only modify the contents of `buf`.
                unsafe {
                    libc::llistxattr(
                        path.as_ptr(),
                        buf.as_mut_ptr() as *mut libc::c_char,
                        size as libc::size_t,
                    )
                }
            }
        };

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        if size == 0 {
            Ok(ListxattrReply::Count(res as u32))
        } else {
            // Truncate the buffer to the actual length of the value
            buf.resize(res as usize, 0);
            Ok(ListxattrReply::Names(buf))
        }
    }

    fn do_removexattr(&self, inode: Inode, name: &CStr) -> io::Result<()> {
        // Check if extended attributes are enabled
        if !self.config.xattr {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }

        // Get the inode data
        let inode_data = self.get_inode_data(inode)?;

        // Ensure the file is in the top layer before modifying attributes
        let inode_data = self.ensure_top_layer(inode_data)?;

        // The f{set,get,remove,list}xattr functions don't work on an fd opened with `O_PATH` so we
        // need to get a new fd. This doesn't work for symlinks, so we use the l* family of
        // functions in that case.
        let res =
            match self.open_inode_or_path(inode_data.inode, libc::O_RDONLY | libc::O_NONBLOCK)? {
                FileOrPath::File(file) => {
                    // Safe because this doesn't modify any memory and we check the return value.
                    unsafe { libc::fremovexattr(file.as_raw_fd(), name.as_ptr()) }
                }
                FileOrPath::Path(path) => {
                    // Safe because this doesn't modify any memory and we check the return value.
                    unsafe { libc::lremovexattr(path.as_ptr(), name.as_ptr()) }
                }
            };

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn do_fallocate(
        &self,
        inode: Inode,
        handle: Handle,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        let data = self.get_inode_handle_data(inode, handle)?;
        let fd = data.file.write().unwrap().as_raw_fd();

        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe {
            libc::fallocate64(
                fd,
                mode as libc::c_int,
                offset as libc::off64_t,
                length as libc::off64_t,
            )
        };

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn do_lseek(&self, inode: Inode, handle: Handle, offset: u64, whence: u32) -> io::Result<u64> {
        let data = self.get_inode_handle_data(inode, handle)?;
        let fd = data.file.write().unwrap().as_raw_fd();

        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe { libc::lseek64(fd, offset as libc::off64_t, whence as libc::c_int) };

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(res as u64)
    }

    fn do_copyfilerange(
        &self,
        inode_in: Inode,
        handle_in: Handle,
        offset_in: u64,
        inode_out: Inode,
        handle_out: Handle,
        offset_out: u64,
        len: u64,
        flags: u64,
    ) -> io::Result<usize> {
        let data_in = self.get_inode_handle_data(inode_in, handle_in)?;
        let data_out = self.get_inode_handle_data(inode_out, handle_out)?;
        let fd_in = data_in.file.write().unwrap().as_raw_fd();
        let fd_out = data_out.file.write().unwrap().as_raw_fd();

        // Safe because this doesn't modify any memory and we check the return value.
        let res = unsafe {
            libc::copy_file_range(
                fd_in,
                &mut (offset_in as i64) as &mut _ as *mut _,
                fd_out,
                &mut (offset_out as i64) as &mut _ as *mut _,
                len.try_into().unwrap(),
                flags.try_into().unwrap(),
            )
        };

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(res as usize)
    }

    fn do_setupmapping(
        &self,
        inode: Inode,
        foffset: u64,
        len: u64,
        flags: u64,
        moffset: u64,
        host_shm_base: u64,
        shm_size: u64,
    ) -> io::Result<()> {
        let open_flags = if (flags & fuse::SetupmappingFlags::WRITE.bits()) != 0 {
            libc::O_RDWR
        } else {
            libc::O_RDONLY
        };

        let prot_flags = if (flags & fuse::SetupmappingFlags::WRITE.bits()) != 0 {
            libc::PROT_READ | libc::PROT_WRITE
        } else {
            libc::PROT_READ
        };

        if (moffset + len) > shm_size {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }

        let addr = host_shm_base + moffset;

        if inode == self.init_inode {
            let ret = unsafe {
                libc::mmap(
                    addr as *mut libc::c_void,
                    len as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_FIXED,
                    -1,
                    0,
                )
            };

            if ret == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }

            let to_copy = if len as usize > INIT_BINARY.len() {
                INIT_BINARY.len()
            } else {
                len as usize
            };

            unsafe {
                libc::memcpy(
                    addr as *mut libc::c_void,
                    INIT_BINARY.as_ptr() as *const _,
                    to_copy,
                )
            };

            return Ok(());
        }

        // Ensure the inode is in the top layer
        let inode_data = self.get_inode_data(inode)?;
        let inode_data = self.ensure_top_layer(inode_data)?;

        let file = self.open_inode(inode_data.inode, open_flags)?;
        let fd = file.as_raw_fd();

        let ret = unsafe {
            libc::mmap(
                addr as *mut libc::c_void,
                len as usize,
                prot_flags,
                libc::MAP_SHARED | libc::MAP_FIXED,
                fd,
                foffset as libc::off_t,
            )
        };

        if ret == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn do_removemapping(
        &self,
        requests: Vec<fuse::RemovemappingOne>,
        host_shm_base: u64,
        shm_size: u64,
    ) -> io::Result<()> {
        for req in requests {
            let addr = host_shm_base + req.moffset;
            if (req.moffset + req.len) > shm_size {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            debug!("removemapping: addr={:x} len={:?}", addr, req.len);
            let ret = unsafe {
                libc::mmap(
                    addr as *mut libc::c_void,
                    req.len as usize,
                    libc::PROT_NONE,
                    libc::MAP_ANONYMOUS | libc::MAP_PRIVATE | libc::MAP_FIXED,
                    -1,
                    0_i64,
                )
            };
            if ret == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
        }

        Ok(())
    }

    fn do_ioctl(
        &self,
        inode: Inode,
        handle: Handle,
        cmd: u32,
        out_size: u32,
    ) -> io::Result<Vec<u8>> {
        const VIRTIO_IOC_MAGIC: u8 = b'v';
        const VIRTIO_IOC_TYPE_EXPORT_FD: u8 = 1;
        const VIRTIO_IOC_EXPORT_FD_SIZE: usize = 2 * mem::size_of::<u64>();
        const VIRTIO_IOC_EXPORT_FD_REQ: u32 = request_code_read!(
            VIRTIO_IOC_MAGIC,
            VIRTIO_IOC_TYPE_EXPORT_FD,
            VIRTIO_IOC_EXPORT_FD_SIZE
        ) as u32;

        match cmd {
            VIRTIO_IOC_EXPORT_FD_REQ => {
                if out_size as usize != VIRTIO_IOC_EXPORT_FD_SIZE {
                    return Err(io::Error::from_raw_os_error(libc::EINVAL));
                }

                let mut exports = self
                    .config
                    .export_table
                    .as_ref()
                    .ok_or(io::Error::from_raw_os_error(libc::EOPNOTSUPP))?
                    .lock()
                    .unwrap();

                let handles = self.handles.read().unwrap();
                let data = handles
                    .get(&handle)
                    .filter(|hd| hd.inode == inode)
                    .ok_or_else(ebadf)?;

                data.exported.store(true, Ordering::Relaxed);

                let fd = data.file.read().unwrap().try_clone()?;

                exports.insert((self.config.export_fsid, handle), fd);

                let mut ret: Vec<_> = self.config.export_fsid.to_ne_bytes().into();
                ret.extend_from_slice(&handle.to_ne_bytes());
                Ok(ret)
            }
            _ => Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP)),
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Returns a "bad file descriptor" error
fn ebadf() -> io::Error {
    io::Error::from_raw_os_error(libc::EBADF)
}

/// Returns an "invalid argument" error
fn einval() -> io::Error {
    io::Error::from_raw_os_error(libc::EINVAL)
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl FileSystem for OverlayFs {
    type Inode = Inode;
    type Handle = Handle;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        // Set the umask to 0 to ensure that all file permissions are set correctly
        unsafe { libc::umask(0o000) };

        // Enable readdirplus if supported
        let mut opts = FsOptions::DO_READDIRPLUS | FsOptions::READDIRPLUS_AUTO;

        // Enable writeback caching if requested and supported
        if self.config.writeback && capable.contains(FsOptions::WRITEBACK_CACHE) {
            opts |= FsOptions::WRITEBACK_CACHE;
            self.writeback.store(true, Ordering::Relaxed);
        }

        // Enable submounts if supported
        if capable.contains(FsOptions::SUBMOUNTS) {
            opts |= FsOptions::SUBMOUNTS;
            self.announce_submounts.store(true, Ordering::Relaxed);
        }

        Ok(opts)
    }

    fn destroy(&self) {
        // Clear all handles
        self.handles.write().unwrap().clear();

        // Clear all inodes
        self.inodes.write().unwrap().clear();
    }

    fn statfs(&self, _ctx: Context, inode: Inode) -> io::Result<libc::statvfs64> {
        // Get the inode data
        let data = self.get_inode_data(inode)?;

        // Call statvfs64 to get filesystem statistics
        // Safe because this will only modify `out` and we check the return value.
        let mut out = MaybeUninit::<bindings::statvfs64>::zeroed();
        let res = unsafe { libc::fstatvfs64(data.file.as_raw_fd(), out.as_mut_ptr()) };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        // Safe because statvfs64 initialized the struct
        Ok(unsafe { out.assume_init() })
    }

    fn lookup(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<Entry> {
        Self::validate_name(name)?;

        #[cfg(not(feature = "efi"))]
        let init_name = unsafe { CStr::from_bytes_with_nul_unchecked(INIT_CSTR) };

        #[cfg(not(feature = "efi"))]
        if self.init_inode != 0 && name == init_name {
            let mut st: bindings::stat64 = unsafe { std::mem::zeroed() };
            st.st_size = INIT_BINARY.len() as i64;
            st.st_ino = self.init_inode;
            st.st_mode = 0o100_755;

            return Ok(Entry {
                inode: self.init_inode,
                generation: 0,
                attr: st,
                attr_flags: 0,
                attr_timeout: self.config.attr_timeout,
                entry_timeout: self.config.entry_timeout,
            });
        }

        let (entry, _) = self.do_lookup(parent, name)?;
        self.bump_refcount(entry.inode);
        Ok(entry)
    }

    fn forget(&self, _ctx: Context, inode: Inode, count: u64) {
        self.do_forget(inode, count);
    }

    fn opendir(
        &self,
        _ctx: Context,
        inode: Inode,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        self.do_open(inode, flags | (libc::O_DIRECTORY as u32))
    }

    fn releasedir(
        &self,
        _ctx: Context,
        inode: Inode,
        _flags: u32,
        handle: Handle,
    ) -> io::Result<()> {
        self.do_release(inode, handle)
    }

    fn mkdir(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        Self::validate_name(name)?;
        let entry = self.do_mkdir(ctx, parent, name, mode, umask, extensions)?;
        self.bump_refcount(entry.inode);
        Ok(entry)
    }

    fn rmdir(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.do_unlink(parent, name, libc::AT_REMOVEDIR)
    }

    fn readdir<F>(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Handle,
        size: u32,
        offset: u64,
        add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(filesystem::DirEntry<'_>) -> io::Result<usize>,
    {
        self.do_readdir(inode, size, offset, add_entry)
    }

    fn readdirplus<F>(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        mut add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(filesystem::DirEntry<'_>, Entry) -> io::Result<usize>,
    {
        let _ = self.get_inode_handle_data(inode, handle)?;
        self.do_readdir(inode, size, offset, |dir_entry| {
            let (entry, _) = self.do_lookup(inode, &CString::new(dir_entry.name).unwrap())?;
            add_entry(dir_entry, entry)
        })
    }

    fn open(
        &self,
        _ctx: Context,
        inode: Inode,
        flags: u32,
    ) -> io::Result<(Option<Handle>, OpenOptions)> {
        if inode == self.init_inode {
            Ok((Some(self.init_handle), OpenOptions::empty()))
        } else {
            self.do_open(inode, flags)
        }
    }

    fn release(
        &self,
        _ctx: Context,
        inode: Inode,
        _flags: u32,
        handle: Handle,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        self.do_release(inode, handle)
    }

    fn create(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        flags: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<(Entry, Option<Handle>, OpenOptions)> {
        Self::validate_name(name)?;
        let (entry, handle, opts) =
            self.do_create(ctx, parent, name, mode, flags, umask, extensions)?;
        self.bump_refcount(entry.inode);
        Ok((entry, handle, opts))
    }

    fn unlink(&self, _ctx: Context, parent: Inode, name: &CStr) -> io::Result<()> {
        self.do_unlink(parent, name, 0)
    }

    fn read<W: io::Write + ZeroCopyWriter>(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        mut w: W,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        #[cfg(not(feature = "efi"))]
        if inode == self.init_inode {
            return w.write(&INIT_BINARY[offset as usize..(offset + (size as u64)) as usize]);
        }

        let data = self.get_inode_handle_data(inode, handle)?;

        let f = data.file.read().unwrap();
        w.write_from(&f, size as usize, offset)
    }

    fn write<R: io::Read + ZeroCopyReader>(
        &self,
        ctx: Context,
        inode: Inode,
        handle: Handle,
        mut r: R,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        kill_priv: bool,
        _flags: u32,
    ) -> io::Result<usize> {
        if kill_priv {
            // We need to change credentials during a write so that the kernel will remove setuid
            // or setgid bits from the file if it was written to by someone other than the owner.
            let (_uid, _gid) = self.set_scoped_credentials(ctx.uid, ctx.gid)?;
        }

        let data = self.get_inode_handle_data(inode, handle)?;
        let f = data.file.read().unwrap();
        r.read_to(&f, size as usize, offset)
    }

    fn getattr(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Option<Handle>,
    ) -> io::Result<(libc::stat64, Duration)> {
        self.do_getattr(inode)
    }

    fn setattr(
        &self,
        _ctx: Context,
        inode: Inode,
        attr: libc::stat64,
        handle: Option<Handle>,
        valid: SetattrValid,
    ) -> io::Result<(libc::stat64, Duration)> {
        // Get the inode data
        let inode_data = self.get_inode_data(inode)?;

        // Ensure the file is in the top layer before modifying attributes
        let inode_data = self.ensure_top_layer(inode_data)?;

        // Get the file identifier - either from handle or path
        let file_id = if let Some(handle) = handle {
            // Get the handle data
            let handles = self.handles.read().unwrap();
            let handle_data = handles.get(&handle).ok_or_else(ebadf)?;
            let file = handle_data.file.read().unwrap();
            FileId::Fd(file.as_raw_fd())
        } else {
            let fd_str = Self::data_to_fd_str(&inode_data)?;
            FileId::Path(fd_str)
        };

        // Handle mode changes
        if valid.contains(SetattrValid::MODE) {
            // Safe because this doesn't modify any memory and we check the return value.
            let res = unsafe {
                match file_id {
                    FileId::Fd(fd) => libc::fchmod(fd, attr.st_mode),
                    FileId::Path(ref p) => {
                        libc::fchmodat(self.proc_self_fd.as_raw_fd(), p.as_ptr(), attr.st_mode, 0)
                    }
                }
            };

            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // Handle ownership changes
        if valid.intersects(SetattrValid::UID | SetattrValid::GID) {
            let uid = if valid.contains(SetattrValid::UID) {
                attr.st_uid
            } else {
                // Cannot use -1 here because these are unsigned values.
                u32::MAX
            };

            let gid = if valid.contains(SetattrValid::GID) {
                attr.st_gid
            } else {
                // Cannot use -1 here because these are unsigned values.
                u32::MAX
            };

            // Safe because this doesn't modify any memory and we check the return value.
            let res = unsafe {
                libc::fchownat(
                    inode_data.file.as_raw_fd(),
                    EMPTY_CSTR.as_ptr(),
                    uid,
                    gid,
                    libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
                )
            };

            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // Handle size changes
        if valid.contains(SetattrValid::SIZE) {
            // Safe because this doesn't modify any memory and we check the return value.
            let res = match file_id {
                FileId::Fd(fd) => unsafe { libc::ftruncate(fd, attr.st_size) },
                _ => {
                    // There is no `ftruncateat` so we need to get a new fd and truncate it.
                    let f = self.open_inode(inode, libc::O_NONBLOCK | libc::O_RDWR)?;
                    unsafe { libc::ftruncate(f.as_raw_fd(), attr.st_size) }
                }
            };

            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // Handle timestamp changes
        if valid.intersects(SetattrValid::ATIME | SetattrValid::MTIME) {
            let mut tvs = [
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
                libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                },
            ];

            if valid.contains(SetattrValid::ATIME_NOW) {
                tvs[0].tv_nsec = libc::UTIME_NOW;
            } else if valid.contains(SetattrValid::ATIME) {
                tvs[0].tv_sec = attr.st_atime;
                tvs[0].tv_nsec = attr.st_atime_nsec;
            }

            if valid.contains(SetattrValid::MTIME_NOW) {
                tvs[1].tv_nsec = libc::UTIME_NOW;
            } else if valid.contains(SetattrValid::MTIME) {
                tvs[1].tv_sec = attr.st_mtime;
                tvs[1].tv_nsec = attr.st_mtime_nsec;
            }

            // Safe because this doesn't modify any memory and we check the return value
            let res = match file_id {
                FileId::Fd(fd) => unsafe { libc::futimens(fd, tvs.as_ptr()) },
                FileId::Path(ref p) => unsafe {
                    libc::utimensat(self.proc_self_fd.as_raw_fd(), p.as_ptr(), tvs.as_ptr(), 0)
                },
            };

            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // Return the updated attributes and timeout
        self.do_getattr(inode)
    }

    fn rename(
        &self,
        _ctx: Context,
        olddir: Inode,
        oldname: &CStr,
        newdir: Inode,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        Self::validate_name(oldname)?;
        Self::validate_name(newname)?;
        self.do_rename(olddir, oldname, newdir, newname, flags)
    }

    fn mknod(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        Self::validate_name(name)?;
        let entry = self.do_mknod(ctx, parent, name, mode, rdev, umask, extensions)?;
        self.bump_refcount(entry.inode);
        Ok(entry)
    }

    fn link(
        &self,
        _ctx: Context,
        inode: Inode,
        newparent: Inode,
        newname: &CStr,
    ) -> io::Result<Entry> {
        Self::validate_name(newname)?;
        let entry = self.do_link(inode, newparent, newname)?;
        self.bump_refcount(entry.inode);
        Ok(entry)
    }

    fn symlink(
        &self,
        ctx: Context,
        linkname: &CStr,
        parent: Inode,
        name: &CStr,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        Self::validate_name(name)?;
        let entry = self.do_symlink(ctx, linkname, parent, name, extensions)?;
        self.bump_refcount(entry.inode);
        Ok(entry)
    }

    fn readlink(&self, _ctx: Context, inode: Inode) -> io::Result<Vec<u8>> {
        self.do_readlink(inode)
    }

    fn flush(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        _lock_owner: u64,
    ) -> io::Result<()> {
        let data = self.get_inode_handle_data(inode, handle)?;

        // Since this method is called whenever an fd is closed in the client, we can emulate that
        // behavior by doing the same thing (dup-ing the fd and then immediately closing it). Safe
        // because this doesn't modify any memory and we check the return values.
        unsafe {
            let newfd = libc::dup(data.file.write().unwrap().as_raw_fd());
            if newfd < 0 {
                return Err(io::Error::last_os_error());
            }

            if libc::close(newfd) < 0 {
                return Err(io::Error::last_os_error());
            }

            Ok(())
        }
    }

    fn fsync(&self, _ctx: Context, inode: Inode, datasync: bool, handle: Handle) -> io::Result<()> {
        let data = self.get_inode_handle_data(inode, handle)?;
        let fd = data.file.write().unwrap().as_raw_fd();

        // Safe because this doesn't modify any memory and we check the return values.
        let res = unsafe {
            if datasync {
                libc::fdatasync(fd)
            } else {
                libc::fsync(fd)
            }
        };

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    fn fsyncdir(
        &self,
        ctx: Context,
        inode: Inode,
        datasync: bool,
        handle: Handle,
    ) -> io::Result<()> {
        self.fsync(ctx, inode, datasync, handle)
    }

    fn access(&self, ctx: Context, inode: Inode, mask: u32) -> io::Result<()> {
        let inode_data = self.get_inode_data(inode)?;
        let fd = inode_data.file.as_raw_fd();

        let (st, _) = Self::statx(fd, None)?;
        let mode = mask as i32 & (libc::R_OK | libc::W_OK | libc::X_OK);

        if mode == libc::F_OK {
            // The file exists since we were able to call `stat(2)` on it.
            return Ok(());
        }

        if (mode & libc::R_OK) != 0
            && ctx.uid != 0
            && (st.st_uid != ctx.uid || st.st_mode & 0o400 == 0)
            && (st.st_gid != ctx.gid || st.st_mode & 0o040 == 0)
            && st.st_mode & 0o004 == 0
        {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }

        if (mode & libc::W_OK) != 0
            && ctx.uid != 0
            && (st.st_uid != ctx.uid || st.st_mode & 0o200 == 0)
            && (st.st_gid != ctx.gid || st.st_mode & 0o020 == 0)
            && st.st_mode & 0o002 == 0
        {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }

        // root can only execute something if it is executable by one of the owner, the group, or
        // everyone.
        if (mode & libc::X_OK) != 0
            && (ctx.uid != 0 || st.st_mode & 0o111 == 0)
            && (st.st_uid != ctx.uid || st.st_mode & 0o100 == 0)
            && (st.st_gid != ctx.gid || st.st_mode & 0o010 == 0)
            && st.st_mode & 0o001 == 0
        {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }

        Ok(())
    }

    fn setxattr(
        &self,
        _ctx: Context,
        inode: Inode,
        name: &CStr,
        value: &[u8],
        flags: u32,
    ) -> io::Result<()> {
        self.do_setxattr(inode, name, value, flags)
    }

    fn getxattr(
        &self,
        _ctx: Context,
        inode: Inode,
        name: &CStr,
        size: u32,
    ) -> io::Result<GetxattrReply> {
        self.do_getxattr(inode, name, size)
    }

    fn listxattr(&self, _ctx: Context, inode: Inode, size: u32) -> io::Result<ListxattrReply> {
        self.do_listxattr(inode, size)
    }

    fn removexattr(&self, _ctx: Context, inode: Inode, name: &CStr) -> io::Result<()> {
        self.do_removexattr(inode, name)
    }

    fn fallocate(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        self.do_fallocate(inode, handle, mode, offset, length)
    }

    fn lseek(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        offset: u64,
        whence: u32,
    ) -> io::Result<u64> {
        self.do_lseek(inode, handle, offset, whence)
    }

    fn copyfilerange(
        &self,
        _ctx: Context,
        inode_in: Inode,
        handle_in: Handle,
        offset_in: u64,
        inode_out: Inode,
        handle_out: Handle,
        offset_out: u64,
        len: u64,
        flags: u64,
    ) -> io::Result<usize> {
        self.do_copyfilerange(
            inode_in, handle_in, offset_in, inode_out, handle_out, offset_out, len, flags,
        )
    }

    fn setupmapping(
        &self,
        _ctx: Context,
        inode: Inode,
        _handle: Handle,
        foffset: u64,
        len: u64,
        flags: u64,
        moffset: u64,
        host_shm_base: u64,
        shm_size: u64,
    ) -> io::Result<()> {
        self.do_setupmapping(inode, foffset, len, flags, moffset, host_shm_base, shm_size)
    }

    fn removemapping(
        &self,
        _ctx: Context,
        requests: Vec<fuse::RemovemappingOne>,
        host_shm_base: u64,
        shm_size: u64,
    ) -> io::Result<()> {
        self.do_removemapping(requests, host_shm_base, shm_size)
    }

    fn ioctl(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        _flags: u32,
        cmd: u32,
        _arg: u64,
        _in_size: u32,
        out_size: u32,
    ) -> io::Result<Vec<u8>> {
        self.do_ioctl(inode, handle, cmd, out_size)
    }
}

impl Drop for ScopedGid {
    fn drop(&mut self) {
        let res = unsafe { libc::syscall(libc::SYS_setresgid, -1, 0, -1) };
        if res != 0 {
            log::error!(
                "failed to restore gid back to root: {}",
                io::Error::last_os_error()
            );
        }
    }
}

impl Drop for ScopedUid {
    fn drop(&mut self) {
        let res = unsafe { libc::syscall(libc::SYS_setresuid, -1, 0, -1) };
        if res != 0 {
            log::error!(
                "failed to restore uid back to root: {}",
                io::Error::last_os_error()
            );
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Config {
            entry_timeout: Duration::from_secs(5),
            attr_timeout: Duration::from_secs(5),
            cache_policy: Default::default(),
            writeback: false,
            root_dir: String::from("/"),
            xattr: true,
            proc_sfd_rawfd: None,
            export_fsid: 0,
            export_table: None,
            layers: vec![],
        }
    }
}
