use std::collections::{btree_map, BTreeMap, HashMap};
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::result::Result;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crossbeam_channel::{unbounded, Sender};
use hvf::MemoryMapping;
use intaglio::cstr::SymbolTable;
use intaglio::Symbol;

use crate::virtio::bindings;
use crate::virtio::fs::filesystem::{
    Context, DirEntry, Entry, ExportTable, Extensions, FileSystem, FsOptions, GetxattrReply,
    ListxattrReply, OpenOptions, SecContext, SetattrValid, ZeroCopyReader, ZeroCopyWriter,
};
use crate::virtio::fs::fuse;
use crate::virtio::fs::multikey::MultikeyBTreeMap;
use crate::virtio::linux_errno::linux_error;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// The prefix for whiteout files
const WHITEOUT_PREFIX: &str = ".wh.";

/// The marker for opaque directories
const OPAQUE_MARKER: &str = ".wh..wh..opq";

/// The volume directory
const VOL_DIR: &str = ".vol";

/// The owner and permissions attribute
const OWNER_PERMS_XATTR_KEY: &[u8] = b"user.vm.owner_perms\0";

#[cfg(not(feature = "efi"))]
static INIT_BINARY: &[u8] = include_bytes!("../../../../../../../init/init");

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
    ino: u64,

    /// The device ID from the host filesystem
    dev: i32,
}

/// Data associated with an inode
#[derive(Debug)]
pub(crate) struct InodeData {
    /// The inode number in the overlay filesystem
    pub(crate) inode: Inode,

    /// The inode number from the host filesystem
    pub(crate) ino: u64,

    /// The device ID from the host filesystem
    pub(crate) dev: i32,

    /// Reference count for this inode from the perspective of [`FileSystem::lookup`]
    pub(crate) refcount: AtomicU64,

    /// Path to inode
    pub(crate) path: Vec<Symbol>,

    /// The layer index this inode belongs to
    pub(crate) layer_idx: usize,
}

/// State for directory stream iteration
#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
struct DirStream {
    /// Opaque handle for the directory stream
    stream: u64,

    /// Current position in the directory stream
    offset: i64,
}

/// The caching policy that the file system should report to the FUSE client. By default the FUSE
/// protocol uses close-to-open consistency. This means that any cached contents of the file are
/// invalidated the next time that file is opened.
#[derive(Debug, Default, Clone)]
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

/// Data associated with an open file handle
#[derive(Debug)]
pub(crate) struct HandleData {
    /// The inode this handle refers to
    inode: Inode,

    /// The underlying file object
    file: RwLock<std::fs::File>,

    /// Directory stream state (used for directory handles)
    dirstream: Mutex<DirStream>,
}

/// Represents either a file descriptor or a path
#[derive(Clone)]
enum FileId {
    /// A file descriptor
    Fd(RawFd),

    /// A path
    Path(CString),
}

/// Configuration for the overlay filesystem
#[derive(Debug)]
pub struct Config {
    /// How long the FUSE client should consider directory entries to be valid.
    /// If the contents of a directory can only be modified by the FUSE client,
    /// this should be a large value.
    pub entry_timeout: Duration,

    /// How long the FUSE client should consider file and directory attributes to be valid.
    /// If the attributes of a file or directory can only be modified by the FUSE client,
    /// this should be a large value.
    pub attr_timeout: Duration,

    /// The caching policy the file system should use.
    pub cache_policy: CachePolicy,

    /// Whether writeback caching is enabled.
    /// This can improve performance but increases the risk of data corruption if file
    /// contents can change without the knowledge of the FUSE client.
    pub writeback: bool,

    /// Whether the filesystem should support Extended Attributes (xattr).
    /// Enabling this feature may have a significant impact on performance.
    pub xattr: bool,

    /// Optional file descriptor for /proc/self/fd.
    /// This is useful for sandboxing scenarios.
    pub proc_sfd_rawfd: Option<RawFd>,

    /// ID of this filesystem to uniquely identify exports.
    pub export_fsid: u64,

    /// Table of exported FDs to share with other subsystems.
    pub export_table: Option<ExportTable>,
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
pub struct OverlayFs {
    /// Map of inodes by ID and alternative keys
    inodes: RwLock<MultikeyBTreeMap<Inode, InodeAltKey, Arc<InodeData>>>,

    /// Counter for generating the next inode ID
    next_inode: AtomicU64,

    /// The initial inode ID (typically 1 for the root directory)
    init_inode: u64,

    /// Map of open file handles by ID
    handles: RwLock<BTreeMap<Handle, Arc<HandleData>>>,

    /// Counter for generating the next handle ID
    next_handle: AtomicU64,

    /// The initial handle ID
    init_handle: u64,

    /// Map of memory-mapped windows
    map_windows: Mutex<HashMap<u64, u64>>,

    /// Whether writeback caching is enabled
    writeback: AtomicBool,

    /// Whether to announce submounts
    announce_submounts: AtomicBool,

    /// Configuration options
    config: Config,

    /// Symbol table for interned filenames
    filenames: Arc<RwLock<SymbolTable>>,

    /// Root inodes for each layer, ordered from bottom to top
    layer_roots: Arc<RwLock<Vec<Inode>>>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl InodeAltKey {
    fn new(ino: u64, dev: i32) -> Self {
        Self { ino, dev }
    }
}

impl OverlayFs {
    /// Creates a new OverlayFs with the given layers
    pub fn new(layers: Vec<PathBuf>, config: Config) -> io::Result<Self> {
        if layers.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one layer must be provided",
            ));
        }

        let mut next_inode = 1;
        let mut inodes = MultikeyBTreeMap::new();

        // Initialize the root inodes for all layers
        let layer_roots = Self::init_root_inodes(&layers, &mut inodes, &mut next_inode)?;

        Ok(OverlayFs {
            inodes: RwLock::new(inodes),
            next_inode: AtomicU64::new(next_inode),
            init_inode: 1,
            handles: RwLock::new(BTreeMap::new()),
            next_handle: AtomicU64::new(1),
            init_handle: 0,
            map_windows: Mutex::new(HashMap::new()),
            writeback: AtomicBool::new(false),
            announce_submounts: AtomicBool::new(false),
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
            let st = Self::unpatched_stat(&FileId::Path(c_path))?;

            // Create the alt key for this inode
            let alt_key = InodeAltKey::new(st.st_ino, st.st_dev as i32);

            // Create the inode data
            let inode_id = *next_inode;
            *next_inode += 1;

            let inode_data = Arc::new(InodeData {
                inode: inode_id,
                ino: st.st_ino,
                dev: st.st_dev as i32,
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
        ino: u64,
        dev: i32,
        path: Vec<Symbol>,
        layer_idx: usize,
    ) -> (Inode, Arc<InodeData>) {
        let inode = self.next_inode.fetch_add(1, Ordering::SeqCst);

        let data = Arc::new(InodeData {
            inode,
            ino,
            dev,
            refcount: AtomicU64::new(1),
            path,
            layer_idx,
        });

        let alt_key = InodeAltKey::new(ino, dev);
        self.inodes
            .write()
            .unwrap()
            .insert(inode, alt_key, data.clone());

        (inode, data)
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

    /// Converts a dev/ino pair to a volume path
    fn dev_ino_to_vol_path(&self, dev: i32, ino: u64) -> io::Result<CString> {
        let path = format!("/{}/{}/{}", VOL_DIR, dev, ino);
        CString::new(path).map_err(|_| einval())
    }

    /// Converts a dev/ino pair and name to a volume path
    fn dev_ino_and_name_to_vol_path(&self, dev: i32, ino: u64, name: &CStr) -> io::Result<CString> {
        let path = format!("/{}/{}/{}/{}", VOL_DIR, dev, ino, name.to_string_lossy());
        CString::new(path).map_err(|_| einval())
    }

    fn dev_ino_and_name_to_vol_whiteout_path(
        &self,
        dev: i32,
        ino: u64,
        name: &CStr,
    ) -> io::Result<CString> {
        // Create whiteout file (.wh.<name>) in parent directory
        let whiteout_name = format!(
            "{}{}",
            WHITEOUT_PREFIX,
            name.to_str().map_err(|_| einval())?
        );

        let whiteout_cstr = CString::new(whiteout_name).map_err(|_| einval())?;

        // Get full path for whiteout file
        self.dev_ino_and_name_to_vol_path(dev, ino, &whiteout_cstr)
    }

    /// Converts an inode number to a volume path
    fn inode_number_to_vol_path(&self, inode: Inode) -> io::Result<CString> {
        let data = self.get_inode_data(inode)?;
        self.dev_ino_to_vol_path(data.dev, data.ino)
    }

    /// Turns an inode into an opened file.
    fn open_inode(&self, inode: Inode, mut flags: i32) -> io::Result<File> {
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

        let c_path = self.inode_number_to_vol_path(inode)?;

        let fd = unsafe {
            libc::open(
                c_path.as_ptr(),
                (flags | libc::O_CLOEXEC) & (!libc::O_NOFOLLOW) & (!libc::O_EXLOCK),
            )
        };

        if fd < 0 {
            return Err(linux_error(io::Error::last_os_error()));
        }

        // Safe because we just opened this fd.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    /// Parses open flags
    fn parse_open_flags(&self, flags: i32) -> i32 {
        let mut mflags: i32 = flags & 0b11;

        if (flags & bindings::LINUX_O_NONBLOCK) != 0 {
            mflags |= libc::O_NONBLOCK;
        }
        if (flags & bindings::LINUX_O_APPEND) != 0 {
            mflags |= libc::O_APPEND;
        }
        if (flags & bindings::LINUX_O_CREAT) != 0 {
            mflags |= libc::O_CREAT;
        }
        if (flags & bindings::LINUX_O_TRUNC) != 0 {
            mflags |= libc::O_TRUNC;
        }
        if (flags & bindings::LINUX_O_EXCL) != 0 {
            mflags |= libc::O_EXCL;
        }
        if (flags & bindings::LINUX_O_NOFOLLOW) != 0 {
            mflags |= libc::O_NOFOLLOW;
        }
        if (flags & bindings::LINUX_O_CLOEXEC) != 0 {
            mflags |= libc::O_CLOEXEC;
        }

        mflags
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

    /// Checks for whiteout file in top layer
    fn check_whiteout(&self, parent_path: &CStr, name: &CStr) -> io::Result<bool> {
        let parent_str = parent_path.to_str().map_err(|_| einval())?;
        let name_str = name.to_str().map_err(|_| einval())?;

        let whiteout_path = format!("{}/{}{}", parent_str, WHITEOUT_PREFIX, name_str);
        let whiteout_cpath = CString::new(whiteout_path).map_err(|_| einval())?;

        match Self::unpatched_stat(&FileId::Path(whiteout_cpath)) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
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

    /// Checks for an opaque directory marker in the given parent directory path.
    fn check_opaque_marker(&self, parent_path: &CStr) -> io::Result<bool> {
        let parent_str = parent_path.to_str().map_err(|_| einval())?;
        let opaque_path = format!("{}/{}", parent_str, OPAQUE_MARKER);
        let opaque_cpath = CString::new(opaque_path).map_err(|_| einval())?;
        match Self::unpatched_stat(&FileId::Path(opaque_cpath)) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
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
    ) -> Option<io::Result<bindings::stat64>> {
        let mut current_stat;
        let mut parent_dev = layer_root.dev;
        let mut parent_ino = layer_root.ino;
        let mut opaque_marker_found = false;

        // Start from layer root
        let root_vol_path = match self.dev_ino_to_vol_path(parent_dev, parent_ino) {
            Ok(path) => path,
            Err(e) => return Some(Err(e)),
        };

        current_stat = match Self::patched_stat(&FileId::Path(root_vol_path)) {
            Ok(stat) => stat,
            Err(e) => return Some(Err(e)),
        };

        // Traverse each path segment
        for (depth, segment) in path_segments.iter().enumerate() {
            // Get the current segment name and parent vol path
            let filenames = self.filenames.read().unwrap();
            let segment_name = filenames.get(*segment).unwrap();
            let parent_vol_path = match self.dev_ino_to_vol_path(parent_dev, parent_ino) {
                Ok(path) => path,
                Err(e) => return Some(Err(e)),
            };

            // Check for whiteout at current level
            match self.check_whiteout(&parent_vol_path, segment_name) {
                Ok(true) => return None, // Found whiteout, stop searching
                Ok(false) => (),         // No whiteout, continue
                Err(e) => return Some(Err(e)),
            }

            // Check for opaque marker at current level
            match self.check_opaque_marker(&parent_vol_path) {
                Ok(true) => {
                    opaque_marker_found = true;
                }
                Ok(false) => (),
                Err(e) => return Some(Err(e)),
            }

            // Try to stat the current segment using parent dev/ino
            let current_vol_path =
                match self.dev_ino_and_name_to_vol_path(parent_dev, parent_ino, segment_name) {
                    Ok(path) => path,
                    Err(e) => return Some(Err(e)),
                };

            drop(filenames); // Now safe to drop filenames lock

            match Self::patched_stat(&FileId::Path(current_vol_path)) {
                Ok(st) => {
                    // Update parent dev/ino for next iteration
                    parent_dev = st.st_dev as i32;
                    parent_ino = st.st_ino;
                    current_stat = st;

                    // Create or get inode for this path segment
                    let alt_key = InodeAltKey::new(st.st_ino, st.st_dev as i32);
                    let inode_data = {
                        let inodes = self.inodes.read().unwrap();
                        if let Some(data) = inodes.get_alt(&alt_key) {
                            data.clone()
                        } else {
                            drop(inodes); // Drop read lock before write lock

                            let mut path = path_inodes[depth].path.clone();
                            path.push(*segment);

                            let (_, data) = self.create_inode(
                                st.st_ino,
                                st.st_dev as i32,
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
                Err(e) => return Some(Err(e)),
            }
        }

        Some(Ok(current_stat))
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
    ) -> io::Result<(Entry, Vec<Arc<InodeData>>)> {
        let mut path_inodes = vec![];

        // Start from the start_layer_idx and try each layer down to layer 0
        for layer_idx in (0..=start_layer_idx).rev() {
            let layer_root = self.get_layer_root(layer_idx)?;

            // If path_inodes has only the root inode or is empty, we need to restart the lookup with the new layer root.
            if path_inodes.len() < 2 {
                path_inodes = vec![layer_root.clone()];
            }

            match self.lookup_segment_by_segment(&layer_root, &path_segments, &mut path_inodes) {
                Some(Ok(st)) => {
                    let alt_key = InodeAltKey::new(st.st_ino, st.st_dev as i32);

                    // Check if we already have this inode
                    let inodes = self.inodes.read().unwrap();
                    if let Some(data) = inodes.get_alt(&alt_key) {
                        return Ok((self.create_entry(data.inode, st), path_inodes));
                    }

                    drop(inodes);

                    // Create new inode
                    let (inode, data) = self.create_inode(
                        st.st_ino,
                        st.st_dev as i32,
                        path_segments.to_vec(),
                        layer_idx,
                    );
                    path_inodes.push(data.clone());
                    return Ok((self.create_entry(inode, st), path_inodes));
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

        self.lookup_layer_by_layer(parent_data.layer_idx, &path_segments)
    }

    /// Performs a raw stat syscall without any modifications to the returned stat structure.
    ///
    /// This function directly calls the OS's stat syscall and returns the raw stat information
    /// exactly as provided by the filesystem. It does not apply any overlayfs-specific
    /// modifications like owner/permission overrides from extended attributes.
    ///
    /// ## Arguments
    /// * `file` - A FileId containing either a path or file descriptor to stat
    ///
    /// ## Returns
    /// * `io::Result<bindings::stat64>` - The raw stat information from the filesystem
    ///
    /// ## Safety
    /// This function performs raw syscalls but handles all unsafe operations internally.
    fn unpatched_stat(file: &FileId) -> io::Result<bindings::stat64> {
        let mut st = MaybeUninit::<bindings::stat64>::zeroed();

        let ret = unsafe {
            match file {
                FileId::Path(path) => {
                    libc::lstat(path.as_ptr(), st.as_mut_ptr() as *mut libc::stat)
                }
                FileId::Fd(fd) => libc::fstat(*fd, st.as_mut_ptr() as *mut libc::stat),
            }
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(unsafe { st.assume_init() })
    }

    /// Performs a stat syscall and patches the returned stat structure with overlayfs metadata.
    ///
    /// This function extends unpatched_stat by applying overlayfs-specific modifications:
    /// 1. Gets the raw stat information using unpatched_stat
    /// 2. Reads extended attributes storing overlayfs owner/permission overrides
    /// 3. Updates the stat structure with any owner (uid/gid) overrides found
    /// 4. Updates the permission bits with any mode overrides found
    ///
    /// This provides the overlayfs view of file metadata, where file ownership and permissions
    /// can be modified independently of the underlying filesystem.
    ///
    /// ## Arguments
    /// * `file` - A FileId containing either a path or file descriptor to stat
    ///
    /// ## Returns
    /// * `io::Result<bindings::stat64>` - The stat information with overlayfs patches applied
    ///
    /// ## Safety
    /// This function performs raw syscalls but handles all unsafe operations internally.
    fn patched_stat(file: &FileId) -> io::Result<bindings::stat64> {
        let mut stat = Self::unpatched_stat(file)?;

        // Get owner and permissions from xattr
        if let Ok(Some((uid, gid, mode))) = Self::get_owner_perms_attr(file, &stat) {
            // Update the stat with the xattr values if available
            stat.st_uid = uid;
            stat.st_gid = gid;
            // Make sure we only modify the permission bits (lower 12 bits)
            stat.st_mode = (stat.st_mode & !0o7777u16) | mode;
        }

        Ok(stat)
    }

    fn get_owner_perms_attr(
        file: &FileId,
        st: &bindings::stat64,
    ) -> io::Result<Option<(u32, u32, u16)>> {
        // Try to get the owner and permissions from xattr
        let mut buf: Vec<u8> = vec![0; 32];

        // Get options based on file type
        let options = if (st.st_mode & libc::S_IFMT) == libc::S_IFLNK {
            libc::XATTR_NOFOLLOW
        } else {
            0
        };

        // Helper function to convert byte slice to u32 value
        fn item_to_value(item: &[u8], radix: u32) -> Option<u32> {
            match std::str::from_utf8(item) {
                Ok(val) => match u32::from_str_radix(val, radix) {
                    Ok(i) => Some(i),
                    Err(_) => None,
                },
                Err(_) => None,
            }
        }

        // Get the xattr
        let res = match file {
            FileId::Path(path) => unsafe {
                libc::getxattr(
                    path.as_ptr(),
                    OWNER_PERMS_XATTR_KEY.as_ptr() as *const i8,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                    options,
                )
            },
            FileId::Fd(fd) => unsafe {
                libc::fgetxattr(
                    *fd,
                    OWNER_PERMS_XATTR_KEY.as_ptr() as *const i8,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                    options,
                )
            },
        };

        if res < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOATTR) {
                return Ok(None);
            }
            return Err(err);
        }

        let len = res as usize;
        buf.truncate(len);

        // Parse the xattr value
        let parts: Vec<&[u8]> = buf.split(|&b| b == b':').collect();
        if parts.len() != 3 {
            return Ok(None);
        }

        let uid = item_to_value(parts[0], 10).unwrap_or(st.st_uid);
        let gid = item_to_value(parts[1], 10).unwrap_or(st.st_gid);
        let mode = item_to_value(parts[2], 8).unwrap_or(st.st_mode as u32) as u16;

        Ok(Some((uid, gid, mode)))
    }

    fn set_owner_perms_attr(
        file: &FileId,
        st: &bindings::stat64,
        owner: Option<(u32, u32)>,
        mode: Option<u16>,
    ) -> io::Result<()> {
        // Get the current values to use as defaults
        let (uid, gid) = if let Some((uid, gid)) = owner {
            (uid, gid)
        } else {
            (st.st_uid, st.st_gid)
        };

        let mode = mode.unwrap_or(st.st_mode);

        // Format the xattr value
        let value = format!("{}:{}:{:o}", uid, gid, mode & 0o7777);
        let value_bytes = value.as_bytes();

        // Get options based on file type
        let options = if (st.st_mode & libc::S_IFMT) == libc::S_IFLNK {
            libc::XATTR_NOFOLLOW
        } else {
            0
        };

        // Set the xattr
        let res = match file {
            FileId::Path(path) => unsafe {
                libc::setxattr(
                    path.as_ptr(),
                    OWNER_PERMS_XATTR_KEY.as_ptr() as *const i8,
                    value_bytes.as_ptr() as *const libc::c_void,
                    value_bytes.len(),
                    0,
                    options,
                )
            },
            FileId::Fd(fd) => unsafe {
                libc::fsetxattr(
                    *fd,
                    OWNER_PERMS_XATTR_KEY.as_ptr() as *const i8,
                    value_bytes.as_ptr() as *const libc::c_void,
                    value_bytes.len(),
                    0,
                    options,
                )
            },
        };

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    }

    /// Copies up a file or directory from a lower layer to the top layer
    pub(crate) fn copy_up(&self, path_inodes: &[Arc<InodeData>]) -> io::Result<()> {
        // Get the top layer root
        let top_layer_idx = self.get_top_layer_idx();
        let top_layer_root = self.get_layer_root(top_layer_idx)?;

        // Start from root and copy up each segment that's not in the top layer
        let mut parent_dev = top_layer_root.dev;
        let mut parent_ino = top_layer_root.ino;

        // Skip the root inode
        for inode_data in path_inodes.iter().skip(1) {
            // Skip if this segment is already in the top layer
            if inode_data.layer_idx == top_layer_idx {
                parent_dev = inode_data.dev;
                parent_ino = inode_data.ino;
                continue;
            }

            // Get the current segment name
            let segment_name = {
                let name = inode_data.path.last().unwrap();
                let filenames = self.filenames.read().unwrap();
                filenames.get(*name).unwrap().to_owned()
            };

            // Get source and destination paths
            let src_path = self.dev_ino_to_vol_path(inode_data.dev, inode_data.ino)?;
            let dst_path =
                self.dev_ino_and_name_to_vol_path(parent_dev, parent_ino, &segment_name)?;

            // Get source file/directory stats
            let src_stat = Self::patched_stat(&FileId::Path(src_path.clone()))?;
            let file_type = src_stat.st_mode & libc::S_IFMT;

            // Copy up the file/directory
            match file_type {
                libc::S_IFREG => {
                    // Regular file: use clonefile for COW semantics if available
                    // Use clonefile for COW semantics
                    let result = unsafe { clonefile(src_path.as_ptr(), dst_path.as_ptr(), 0) };

                    if result < 0 {
                        let err = io::Error::last_os_error();
                        // If clonefile fails (e.g., across filesystems), fall back to regular copy
                        if err.raw_os_error() == Some(libc::EXDEV)
                            || err.raw_os_error() == Some(libc::ENOTSUP)
                        {
                            // Fall back to regular copy
                            self.copy_file_contents(
                                &src_path,
                                &dst_path,
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
                        if libc::mkdir(dst_path.as_ptr(), src_stat.st_mode & 0o777) < 0 {
                            return Err(io::Error::last_os_error());
                        }

                        // Explicitly set directory permissions to match source
                        if libc::chmod(dst_path.as_ptr(), src_stat.st_mode & 0o777) < 0 {
                            return Err(io::Error::last_os_error());
                        }
                    }
                }
                libc::S_IFLNK => {
                    // Symbolic link: read target and recreate link
                    let mut buf = vec![0u8; libc::PATH_MAX as usize];
                    let len = unsafe {
                        libc::readlink(src_path.as_ptr(), buf.as_mut_ptr() as *mut _, buf.len())
                    };
                    if len < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    buf.truncate(len as usize);

                    unsafe {
                        if libc::symlink(buf.as_ptr() as *const _, dst_path.as_ptr()) < 0 {
                            return Err(io::Error::last_os_error());
                        }

                        // Note: macOS doesn't allow setting permissions on symlinks directly
                        // The permissions of symlinks are typically ignored by the system
                    }
                }
                _ => {
                    // Other types (devices, sockets, etc.) are not supported
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "unsupported file type for copy up",
                    ));
                }
            }

            // Update parent dev/ino for next iteration
            let new_stat = Self::unpatched_stat(&FileId::Path(dst_path))?;
            parent_dev = new_stat.st_dev as i32;
            parent_ino = new_stat.st_ino;

            // Update the inode entry to point to the new copy in the top layer
            let alt_key = InodeAltKey::new(new_stat.st_ino, new_stat.st_dev as i32);
            let mut inodes = self.inodes.write().unwrap();

            // Create new inode data with updated dev/ino/layer_idx but same path and refcount
            let new_data = Arc::new(InodeData {
                inode: inode_data.inode,
                ino: new_stat.st_ino,
                dev: new_stat.st_dev as i32,
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
    fn copy_file_contents(
        &self,
        src_path: &CString,
        dst_path: &CString,
        mode: u32,
    ) -> io::Result<()> {
        unsafe {
            let src_file = libc::open(src_path.as_ptr(), libc::O_RDONLY);
            if src_file < 0 {
                return Err(io::Error::last_os_error());
            }

            let dst_file = libc::open(
                dst_path.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                mode,
            );
            if dst_file < 0 {
                libc::close(src_file);
                return Err(io::Error::last_os_error());
            }

            // Copy file contents
            let mut buf = [0u8; 8192];
            loop {
                let n_read = libc::read(src_file, buf.as_mut_ptr() as *mut _, buf.len());
                if n_read <= 0 {
                    break;
                }
                let mut pos = 0;
                while pos < n_read {
                    let n_written = libc::write(
                        dst_file,
                        buf.as_ptr().add(pos as usize) as *const _,
                        (n_read - pos) as usize,
                    );
                    if n_written <= 0 {
                        libc::close(src_file);
                        libc::close(dst_file);
                        return Err(io::Error::last_os_error());
                    }
                    pos += n_written;
                }
            }

            // Explicitly set permissions to match source file
            // This will override any effects from the umask
            if libc::fchmod(dst_file, mode as libc::mode_t) < 0 {
                libc::close(src_file);
                libc::close(dst_file);
                return Err(io::Error::last_os_error());
            }

            libc::close(src_file);
            libc::close(dst_file);
        }

        Ok(())
    }

    /// Performs a readdir operation
    fn do_readdir<F>(
        &self,
        inode: Inode,
        handle: Handle,
        size: u32,
        offset: u64,
        add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        // TODO: Implement do_readdir
        todo!("implement do_readdir")
    }

    /// Performs an open operation
    fn do_open(&self, inode: Inode, flags: u32) -> io::Result<(Option<Handle>, OpenOptions)> {
        // Parse and normalize the open flags
        let flags = self.parse_open_flags(flags as i32);

        // Get the inode data
        let inode_data = self.get_inode_data(inode)?;

        // Only ensure top layer if write access is requested
        let inode_data = if (flags & libc::O_ACCMODE) != libc::O_RDONLY {
            self.ensure_top_layer(inode_data)?
        } else {
            inode_data
        };

        // Open the file with the appropriate flags and generate a new unique handle ID
        let file = RwLock::new(self.open_inode(inode_data.inode, flags)?);
        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);

        // Create handle data structure with file and empty dirstream
        let data = HandleData {
            inode,
            file,
            dirstream: Mutex::new(DirStream {
                stream: 0,
                offset: 0,
            }),
        };

        // Store the handle data in the handles map
        self.handles.write().unwrap().insert(handle, Arc::new(data));

        // Set up OpenOptions based on the cache policy configuration
        let mut opts = OpenOptions::empty();
        match self.config.cache_policy {
            // For CachePolicy::Never, set DIRECT_IO to bypass kernel caching for files (not directories)
            CachePolicy::Never => opts.set(OpenOptions::DIRECT_IO, flags & libc::O_DIRECTORY == 0),

            // For CachePolicy::Always, set different caching options based on whether it's a file or directory
            CachePolicy::Always => {
                if flags & libc::O_DIRECTORY == 0 {
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
                // We don't need to close the file here because that will happen automatically when
                // the last `Arc` is dropped.
                e.remove();
                return Ok(());
            }
        }

        Err(ebadf())
    }

    /// Performs a getattr operation
    fn do_getattr(&self, inode: Inode) -> io::Result<(bindings::stat64, Duration)> {
        let c_path = self.inode_number_to_vol_path(inode)?;
        let st = Self::patched_stat(&FileId::Path(c_path))?;

        Ok((st, self.config.attr_timeout))
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
        let (_, path_inodes) = self.lookup_layer_by_layer(top_layer_idx, &path_segments)?;

        // Copy up the file
        self.copy_up(&path_inodes)?;

        // Get the inode data for the copied file
        self.get_inode_data(inode_data.inode)
    }

    /// Performs a setattr operation, copying up the file if needed
    fn do_setattr(
        &self,
        inode: Inode,
        attr: bindings::stat64,
        handle: Option<Handle>,
        valid: SetattrValid,
    ) -> io::Result<(bindings::stat64, Duration)> {
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
            // Use path if no handle available
            let c_path = self.dev_ino_to_vol_path(inode_data.dev, inode_data.ino)?;
            FileId::Path(c_path)
        };

        // Consolidate attribute changes using a single setattrlist call
        let current_stat = Self::patched_stat(&file_id)?;

        // Handle ownership changes
        if valid.intersects(SetattrValid::UID | SetattrValid::GID) {
            let uid = if valid.contains(SetattrValid::UID) {
                Some(attr.st_uid)
            } else {
                None
            };

            let gid = if valid.contains(SetattrValid::GID) {
                Some(attr.st_gid)
            } else {
                None
            };

            if let Some((uid, gid)) = uid
                .zip(gid)
                .or_else(|| uid.map(|u| (u, current_stat.st_gid)))
                .or_else(|| gid.map(|g| (current_stat.st_uid, g)))
            {
                Self::set_owner_perms_attr(&file_id, &current_stat, Some((uid, gid)), None)?;
            }
        }

        // Handle mode changes
        if valid.contains(SetattrValid::MODE) {
            let mode = attr.st_mode & 0o7777;
            Self::set_owner_perms_attr(&file_id, &current_stat, None, Some(mode))?;
        }

        // Handle size changes
        if valid.contains(SetattrValid::SIZE) {
            let res = match file_id {
                FileId::Fd(fd) => unsafe { libc::ftruncate(fd, attr.st_size) },
                FileId::Path(ref c_path) => unsafe {
                    libc::truncate(c_path.as_ptr(), attr.st_size)
                },
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
                FileId::Path(ref c_path) => unsafe {
                    let fd = libc::open(c_path.as_ptr(), libc::O_SYMLINK | libc::O_CLOEXEC);
                    let res = libc::futimens(fd, tvs.as_ptr());
                    libc::close(fd);
                    res
                },
            };

            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // Return the updated attributes and timeout
        self.do_getattr(inode)
    }

    fn do_mkdir(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
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

        // Get the parent inode data
        let parent_data = self.get_inode_data(parent)?;

        // Ensure parent directory is in the top layer
        let parent_data = self.ensure_top_layer(parent_data)?;

        // Get the path for the new directory
        let c_path = self.dev_ino_and_name_to_vol_path(parent_data.dev, parent_data.ino, name)?;

        // Create the directory with initial permissions
        let res = unsafe { libc::mkdir(c_path.as_ptr(), 0o700) };
        if res == 0 {
            // Set security context if provided
            if let Some(secctx) = extensions.secctx {
                Self::set_secctx(&FileId::Path(c_path.clone()), secctx, false)?;
            }

            // Get the initial stat for the directory
            let stat = Self::unpatched_stat(&FileId::Path(c_path.clone()))?;

            // Set ownership and permissions
            Self::set_owner_perms_attr(
                &FileId::Path(c_path.clone()),
                &stat,
                Some((ctx.uid, ctx.gid)),
                Some((mode & !umask) as u16),
            )?;

            // Get the updated stat for the directory
            let updated_stat = Self::patched_stat(&FileId::Path(c_path))?;

            let mut path = parent_data.path.clone();
            path.push(self.intern_name(name)?);

            // Create the inode for the newly created directory
            let (inode, _) = self.create_inode(
                updated_stat.st_ino,
                updated_stat.st_dev,
                path,
                parent_data.layer_idx,
            );

            // Create the entry for the newly created directory
            let entry = self.create_entry(inode, updated_stat);

            return Ok(entry);
        }

        // Return the error
        Err(linux_error(io::Error::last_os_error()))
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
            let parent_data = self.get_inode_data(parent)?;

            // Create the whiteout file
            let whiteout_path =
                self.dev_ino_and_name_to_vol_whiteout_path(parent_data.dev, parent_data.ino, name)?;

            let fd = unsafe {
                libc::open(
                    whiteout_path.as_ptr(),
                    libc::O_CREAT | libc::O_WRONLY | libc::O_EXCL,
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

    /// Performs an unlink operation
    fn do_unlink(&self, parent: Inode, name: &CStr) -> io::Result<()> {
        let top_layer_idx = self.get_top_layer_idx();
        let (entry, _) = self.do_lookup(parent, name)?;

        // If the inode is in the top layer, we need to unlink it.
        let entry_data = self.get_inode_data(entry.inode)?;
        if entry_data.layer_idx == top_layer_idx {
            // Get the path for the inode
            let c_path = self.inode_number_to_vol_path(entry.inode)?;

            // Remove the inode from the overlayfs
            let res = unsafe { libc::unlink(c_path.as_ptr()) };
            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // If after an unlink, the entry still exists in a lower layer, we need to add a whiteout
        self.create_whiteout_for_lower(parent, name)?;

        Ok(())
    }

    /// Performs an rmdir operation
    fn do_rmdir(&self, parent: Inode, name: &CStr) -> io::Result<()> {
        let top_layer_idx = self.get_top_layer_idx();
        let (entry, _) = self.do_lookup(parent, name)?;

        // If the inode is in the top layer, we need to unlink it.
        let entry_data = self.get_inode_data(entry.inode)?;
        if entry_data.layer_idx == top_layer_idx {
            // Get the path for the inode
            let c_path = self.inode_number_to_vol_path(entry.inode)?;

            // Remove the inode from the overlayfs
            let res = unsafe { libc::rmdir(c_path.as_ptr()) };
            if res < 0 {
                return Err(io::Error::last_os_error());
            }
        }

        // If after an rmdir, the entry still exists in a lower layer, we need to add a whiteout
        self.create_whiteout_for_lower(parent, name)?;

        Ok(())
    }

    /// Performs a symlink operation
    fn do_symlink(
        &self,
        ctx: Context,
        linkname: &CStr,
        parent: Inode,
        name: &CStr,
        extensions: Extensions,
    ) -> io::Result<Entry> {
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

        // Get the parent inode data
        let parent_data = self.get_inode_data(parent)?;

        // Ensure parent directory is in the top layer
        let parent_data = self.ensure_top_layer(parent_data)?;

        // Get the path for the new directory
        let c_path = self.dev_ino_and_name_to_vol_path(parent_data.dev, parent_data.ino, name)?;

        // Create the directory with initial permissions
        let res = unsafe { libc::symlink(linkname.as_ptr(), c_path.as_ptr()) };
        if res == 0 {
            // Set security context if provided
            if let Some(secctx) = extensions.secctx {
                Self::set_secctx(&FileId::Path(c_path.clone()), secctx, true)?;
            }

            // Get the initial stat for the directory
            let stat = Self::unpatched_stat(&FileId::Path(c_path.clone()))?;

            // Set ownership and permissions
            let mode = libc::S_IFLNK | 0o777;
            Self::set_owner_perms_attr(
                &FileId::Path(c_path.clone()),
                &stat,
                Some((ctx.uid, ctx.gid)),
                Some(mode),
            )?;

            // Get the updated stat for the directory
            let updated_stat = Self::patched_stat(&FileId::Path(c_path))?;

            let mut path = parent_data.path.clone();
            path.push(self.intern_name(name)?);

            // Create the inode for the newly created directory
            let (inode, _) = self.create_inode(
                updated_stat.st_ino,
                updated_stat.st_dev,
                path,
                parent_data.layer_idx,
            );

            // Create the entry for the newly created directory
            let entry = self.create_entry(inode, updated_stat);

            return Ok(entry);
        }

        // Return the error
        Err(linux_error(io::Error::last_os_error()))
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

        // Get the paths for rename operation
        let old_path =
            self.dev_ino_and_name_to_vol_path(old_parent_data.dev, old_parent_data.ino, old_name)?;
        let new_path =
            self.dev_ino_and_name_to_vol_path(new_parent_data.dev, new_parent_data.ino, new_name)?;

        // Set up rename flags
        let mut mflags: u32 = 0;
        if ((flags as i32) & bindings::LINUX_RENAME_NOREPLACE) != 0 {
            mflags |= libc::RENAME_EXCL;
        }
        if ((flags as i32) & bindings::LINUX_RENAME_EXCHANGE) != 0 {
            mflags |= libc::RENAME_SWAP;
        }

        // Check for invalid flag combinations
        if ((flags as i32) & bindings::LINUX_RENAME_WHITEOUT) != 0
            && ((flags as i32) & bindings::LINUX_RENAME_EXCHANGE) != 0
        {
            return Err(linux_error(io::Error::from_raw_os_error(libc::EINVAL)));
        }

        // Perform the rename
        let res = unsafe { libc::renamex_np(old_path.as_ptr(), new_path.as_ptr(), mflags) };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        // After successful rename, check if we need to add a whiteout for the old path
        self.create_whiteout_for_lower(old_parent, old_name)?;

        // If LINUX_RENAME_WHITEOUT is set, create a character device at the old path location
        if ((flags as i32) & bindings::LINUX_RENAME_WHITEOUT) != 0 {
            let fd = unsafe {
                libc::open(
                    old_path.as_ptr(),
                    libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
                    0o600,
                )
            };

            let stat = Self::unpatched_stat(&FileId::Fd(fd))?;
            Self::set_owner_perms_attr(&FileId::Fd(fd), &stat, None, Some(libc::S_IFCHR | 0o600))?;

            if fd < 0 {
                return Err(io::Error::last_os_error());
            }

            unsafe { libc::close(fd) };
        }

        Ok(())
    }

    fn do_link(&self, inode: Inode, new_parent: Inode, new_name: &CStr) -> io::Result<Entry> {
        // Get the inode data for the source file
        let inode_data = self.get_inode_data(inode)?;

        // Copy up the source file to the top layer if needed
        let inode_data = self.ensure_top_layer(inode_data)?;

        // Get and ensure new parent is in top layer
        let new_parent_data = self.ensure_top_layer(self.get_inode_data(new_parent)?)?;

        // Get source and destination paths
        let src_path = self.dev_ino_to_vol_path(inode_data.dev, inode_data.ino)?;

        let dst_path =
            self.dev_ino_and_name_to_vol_path(new_parent_data.dev, new_parent_data.ino, new_name)?;

        // Create the hard link
        let res = unsafe { libc::link(src_path.as_ptr(), dst_path.as_ptr()) };

        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        // Get the entry for the newly created link
        let mut path = new_parent_data.path.clone();
        path.push(self.intern_name(new_name)?);

        // Get stats for the new link
        let stat = Self::patched_stat(&FileId::Path(dst_path))?;

        // Create new inode for the link pointing to same dev/ino as source
        let (inode, _) = self.create_inode(
            stat.st_ino,
            stat.st_dev as i32,
            path,
            new_parent_data.layer_idx,
        );

        Ok(self.create_entry(inode, stat))
    }

    /// Decrements the reference count for an inode and removes it if the count reaches zero
    fn do_forget(&self, inode: Inode, count: u64) {
        // Skip forgetting the root inode
        if inode == self.init_inode {
            return;
        }

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

    fn do_readlink(&self, inode: Inode) -> io::Result<Vec<u8>> {
        // Get the path for this inode
        let c_path = self.inode_number_to_vol_path(inode)?;

        // Allocate a buffer for the link target
        let mut buf = vec![0; libc::PATH_MAX as usize];

        // Call readlink to get the symlink target
        let res = unsafe {
            libc::readlink(
                c_path.as_ptr(),
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

    fn set_secctx(file: &FileId, secctx: SecContext, symlink: bool) -> io::Result<()> {
        let options = if symlink { libc::XATTR_NOFOLLOW } else { 0 };
        let ret = match file {
            FileId::Path(path) => unsafe {
                libc::setxattr(
                    path.as_ptr(),
                    secctx.name.as_ptr(),
                    secctx.secctx.as_ptr() as *const libc::c_void,
                    secctx.secctx.len(),
                    0,
                    options,
                )
            },
            FileId::Fd(fd) => unsafe {
                libc::fsetxattr(
                    *fd,
                    secctx.name.as_ptr(),
                    secctx.secctx.as_ptr() as *const libc::c_void,
                    secctx.secctx.len(),
                    0,
                    options,
                )
            },
        };

        if ret != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
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
    type Inode = u64;
    type Handle = u64;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        // Set the umask to 0 to ensure that all file permissions are set correctly
        unsafe { libc::umask(0o000) };

        let mut opts = FsOptions::empty();

        // Enable writeback caching if requested and supported
        if self.config.writeback && capable.contains(FsOptions::WRITEBACK_CACHE) {
            opts |= FsOptions::WRITEBACK_CACHE;
            self.writeback.store(true, Ordering::SeqCst);
        }

        // Enable posix ACLs if supported
        if capable.contains(FsOptions::POSIX_ACL) {
            opts |= FsOptions::POSIX_ACL;
        }

        Ok(opts)
    }

    fn destroy(&self) {
        // Clear all handles
        self.handles.write().unwrap().clear();

        // Clear all inodes
        self.inodes.write().unwrap().clear();

        // Clear any memory-mapped windows
        self.map_windows.lock().unwrap().clear();
    }

    fn statfs(&self, _ctx: Context, inode: Self::Inode) -> io::Result<bindings::statvfs64> {
        // Get the path for this inode
        let c_path = self.inode_number_to_vol_path(inode)?;

        // Call statvfs64 to get filesystem statistics
        // Safe because this will only modify `out` and we check the return value.
        let mut out = MaybeUninit::<bindings::statvfs64>::zeroed();
        let res = unsafe { bindings::statvfs64(c_path.as_ptr(), out.as_mut_ptr()) };
        if res < 0 {
            return Err(io::Error::last_os_error());
        }

        // Safe because statvfs64 initialized the struct
        Ok(unsafe { out.assume_init() })
    }

    fn lookup(&self, _ctx: Context, parent: Self::Inode, name: &CStr) -> io::Result<Entry> {
        Self::validate_name(name)?;
        let (entry, _) = self.do_lookup(parent, name)?;
        self.bump_refcount(entry.inode);
        Ok(entry)
    }

    fn forget(&self, _ctx: Context, inode: Self::Inode, count: u64) {
        self.do_forget(inode, count);
    }

    fn getattr(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        _handle: Option<Self::Handle>,
    ) -> io::Result<(bindings::stat64, Duration)> {
        self.do_getattr(inode)
    }

    fn setattr(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        attr: bindings::stat64,
        handle: Option<Self::Handle>,
        valid: SetattrValid,
    ) -> io::Result<(bindings::stat64, Duration)> {
        self.do_setattr(inode, attr, handle, valid)
    }

    fn readlink(&self, _ctx: Context, inode: Self::Inode) -> io::Result<Vec<u8>> {
        self.do_readlink(inode)
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

    fn unlink(&self, _ctx: Context, parent: Self::Inode, name: &CStr) -> io::Result<()> {
        Self::validate_name(name)?;
        self.do_unlink(parent, name)
    }

    fn rmdir(&self, _ctx: Context, parent: Self::Inode, name: &CStr) -> io::Result<()> {
        Self::validate_name(name)?;
        self.do_rmdir(parent, name)
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

    fn rename(
        &self,
        _ctx: Context,
        old_parent: Self::Inode,
        old_name: &CStr,
        new_parent: Self::Inode,
        new_name: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        Self::validate_name(old_name)?;
        Self::validate_name(new_name)?;
        self.do_rename(old_parent, old_name, new_parent, new_name, flags)
    }

    fn link(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        new_parent: Self::Inode,
        new_name: &CStr,
    ) -> io::Result<Entry> {
        Self::validate_name(new_name)?;
        let entry = self.do_link(inode, new_parent, new_name)?;
        self.bump_refcount(entry.inode);
        Ok(entry)
    }

    fn open(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        if inode == self.init_inode {
            Ok((Some(self.init_handle), OpenOptions::empty()))
        } else {
            self.do_open(inode, flags)
        }
    }

    fn read<W: io::Write + ZeroCopyWriter>(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        mut w: W,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _flags: u32,
    ) -> io::Result<usize> {
        let data = self.get_inode_handle_data(inode, handle)?;

        #[cfg(not(feature = "efi"))]
        if inode == self.init_inode {
            println!("init inode");
            return w.write(&INIT_BINARY[offset as usize..(offset + (size as u64)) as usize]);
        }

        let f = data.file.read().unwrap();
        w.write_from(&f, size as usize, offset)
    }

    fn write<R: io::Read + ZeroCopyReader>(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        mut r: R,
        size: u32,
        offset: u64,
        _lock_owner: Option<u64>,
        _delayed_write: bool,
        _kill_priv: bool,
        _flags: u32,
    ) -> io::Result<usize> {
        let data = self.get_inode_handle_data(inode, handle)?;
        let f = data.file.read().unwrap();
        r.read_to(&f, size as usize, offset)
    }

    fn flush(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        _lock_owner: u64,
    ) -> io::Result<()> {
        let data = self.get_inode_handle_data(inode, handle)?;

        // Since this method is called whenever an fd is closed in the client, we can emulate that
        // behavior by doing the same thing (dup-ing the fd and then immediately closing it). Safe
        // because this doesn't modify any memory and we check the return values.
        unsafe {
            let newfd = libc::dup(data.file.write().unwrap().as_raw_fd());
            if newfd < 0 {
                return Err(linux_error(io::Error::last_os_error()));
            }

            if libc::close(newfd) < 0 {
                return Err(linux_error(io::Error::last_os_error()));
            }

            Ok(())
        }
    }

    fn release(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        _flags: u32,
        handle: Self::Handle,
        _flush: bool,
        _flock_release: bool,
        _lock_owner: Option<u64>,
    ) -> io::Result<()> {
        self.do_release(inode, handle)
    }

    fn fsync(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        _datasync: bool,
        handle: Self::Handle,
    ) -> io::Result<()> {
        let data = self.get_inode_handle_data(inode, handle)?;

        // Safe because this doesn't modify any memory and we check the return values.
        let res = unsafe { libc::fsync(data.file.write().unwrap().as_raw_fd()) };
        if res < 0 {
            return Err(linux_error(io::Error::last_os_error()));
        }

        Ok(())
    }

    fn opendir(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        self.do_open(inode, flags | libc::O_DIRECTORY as u32)
    }

    fn readdir<F>(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        size: u32,
        offset: u64,
        add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        // TODO: Read directory contents
        todo!("implement readdir")
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
        F: FnMut(DirEntry, Entry) -> io::Result<usize>,
    {
        todo!("implement readdirplus")
    }

    fn releasedir(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        _flags: u32,
        handle: Self::Handle,
    ) -> io::Result<()> {
        // TODO: Release an open directory
        todo!("implement releasedir")
    }

    fn fsyncdir(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        datasync: bool,
        handle: Self::Handle,
    ) -> io::Result<()> {
        // TODO: Synchronize directory contents
        todo!("implement fsyncdir")
    }

    fn setxattr(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        name: &CStr,
        value: &[u8],
        flags: u32,
    ) -> io::Result<()> {
        // TODO: Set an extended attribute
        todo!("implement setxattr")
    }

    fn getxattr(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        name: &CStr,
        size: u32,
    ) -> io::Result<GetxattrReply> {
        // TODO: Get an extended attribute
        todo!("implement getxattr")
    }

    fn listxattr(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        size: u32,
    ) -> io::Result<ListxattrReply> {
        // TODO: List extended attributes
        todo!("implement listxattr")
    }

    fn removexattr(&self, _ctx: Context, inode: Self::Inode, name: &CStr) -> io::Result<()> {
        // TODO: Remove an extended attribute
        todo!("implement removexattr")
    }

    fn access(&self, _ctx: Context, inode: Self::Inode, mask: u32) -> io::Result<()> {
        // TODO: Check file access permissions
        todo!("implement access")
    }

    fn create(
        &self,
        _ctx: Context,
        parent: Self::Inode,
        name: &CStr,
        mode: u32,
        flags: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<(Entry, Option<Self::Handle>, OpenOptions)> {
        // Validate the name to prevent path traversal
        Self::validate_name(name)?;

        // TODO: Create and open a file
        todo!("implement create")
    }

    fn mknod(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        mode: u32,
        _rdev: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        todo!("implement mknod")
    }

    fn fallocate(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        _mode: u32,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        todo!("implement fallocate")
    }

    fn lseek(
        &self,
        _ctx: Context,
        inode: Inode,
        handle: Handle,
        offset: u64,
        whence: u32,
    ) -> io::Result<u64> {
        todo!("implement lseek")
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
        guest_shm_base: u64,
        shm_size: u64,
        map_sender: &Option<Sender<MemoryMapping>>,
    ) -> io::Result<()> {
        todo!("implement setupmapping")
    }

    fn removemapping(
        &self,
        _ctx: Context,
        requests: Vec<fuse::RemovemappingOne>,
        guest_shm_base: u64,
        shm_size: u64,
        map_sender: &Option<Sender<MemoryMapping>>,
    ) -> io::Result<()> {
        todo!("implement removemapping")
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            entry_timeout: Duration::from_secs(5),
            attr_timeout: Duration::from_secs(5),
            cache_policy: CachePolicy::default(), // Use the default cache policy (Auto)
            writeback: false,
            xattr: false,
            proc_sfd_rawfd: None,
            export_fsid: 0,
            export_table: None,
        }
    }
}

// Add Default implementation for Context
impl Default for Context {
    fn default() -> Self {
        Context {
            uid: 0,
            gid: 0,
            pid: 0,
        }
    }
}

//--------------------------------------------------------------------------------------------------
// External Functions
//--------------------------------------------------------------------------------------------------

extern "C" {
    /// macOS system call for cloning a file with COW semantics
    ///
    /// Creates a copy-on-write clone of a file.
    ///
    /// ## Arguments
    ///
    /// * `src` - Path to the source file
    /// * `dst` - Path to the destination file
    /// * `flags` - Currently unused, must be 0
    ///
    /// ## Returns
    ///
    /// * `0` on success
    /// * `-1` on error with errno set
    fn clonefile(
        src: *const libc::c_char,
        dst: *const libc::c_char,
        flags: libc::c_int,
    ) -> libc::c_int;
}
