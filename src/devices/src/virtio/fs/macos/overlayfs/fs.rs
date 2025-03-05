use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString};
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
    ListxattrReply, OpenOptions, SetattrValid, ZeroCopyReader, ZeroCopyWriter,
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
struct InodeData {
    /// The inode number in the overlay filesystem
    inode: Inode,

    /// The inode number from the host filesystem
    ino: u64,

    /// The device ID from the host filesystem
    dev: i32,

    /// Reference count for this inode
    refcount: AtomicU64,

    /// Path to inode
    path: Vec<Symbol>,

    /// The layer index this inode belongs to
    layer_idx: usize,
}

/// State for directory stream iteration
#[derive(Debug, Clone, Copy, PartialOrd, Ord, PartialEq, Eq)]
struct DirStream {
    /// Opaque handle for the directory stream
    stream: u64,

    /// Current position in the directory stream
    offset: i64,
}

/// Data associated with an open file handle
#[derive(Debug)]
struct HandleData {
    /// The inode this handle refers to
    inode: Inode,

    /// The underlying file object
    file: RwLock<std::fs::File>,

    /// Directory stream state (used for directory handles)
    dirstream: Mutex<DirStream>,
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
    cfg: Config,

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
    pub fn new(layers: Vec<PathBuf>, cfg: Config) -> io::Result<Self> {
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
            init_handle: 1,
            map_windows: Mutex::new(HashMap::new()),
            writeback: AtomicBool::new(false),
            announce_submounts: AtomicBool::new(false),
            cfg,
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
            let st = Self::lstat_path(&c_path)?;

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
        let inodes = self.inodes.read().unwrap();
        inodes.get(&inode).cloned().ok_or_else(ebadf)
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
    fn get_inode_data(&self, inode: Inode) -> io::Result<Arc<InodeData>> {
        self.inodes
            .read()
            .unwrap()
            .get(&inode)
            .cloned()
            .ok_or_else(ebadf)
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

    /// Converts an inode number to a volume path
    fn inode_number_to_vol_path(&self, inode: Inode) -> io::Result<CString> {
        let data = self.get_inode_data(inode)?;
        self.dev_ino_to_vol_path(data.dev, data.ino)
    }

    /// Creates an Entry from stat information and inode data
    fn create_entry(&self, inode: Inode, st: bindings::stat64) -> Entry {
        Entry {
            inode,
            generation: 0,
            attr: st,
            attr_flags: 0,
            attr_timeout: self.cfg.attr_timeout,
            entry_timeout: self.cfg.entry_timeout,
        }
    }

    /// Checks for whiteout file in top layer
    fn check_whiteout(&self, parent_path: &CStr, name: &CStr) -> io::Result<bool> {
        let parent_str = parent_path.to_str().map_err(|_| einval())?;
        let name_str = name.to_str().map_err(|_| einval())?;

        let whiteout_path = format!("{}/{}{}", parent_str, WHITEOUT_PREFIX, name_str);
        let whiteout_cpath = CString::new(whiteout_path).map_err(|_| einval())?;

        match Self::lstat_path(&whiteout_cpath) {
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
        match Self::lstat_path(&opaque_cpath) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Validates a name to prevent path traversal attacks
    ///
    /// This function checks if a name contains path traversal sequences like ".." or
    /// other potentially dangerous patterns.
    ///
    /// Returns:
    /// - Ok(()) if the name is safe
    /// - Err(io::Error) if the name contains path traversal sequences
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

        Ok(())
    }

    /// Looks up a path segment by segment in a given layer
    ///
    /// This function traverses a path one segment at a time within a specific layer,
    /// handling whiteouts and opaque markers along the way.
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
    /// # Arguments
    /// * `layer_root` - Root inode data for the layer being searched
    /// * `path_segments` - Path components to traverse, as interned symbols
    ///
    /// # Example Return Flow
    /// 1. If path exists: `Some(Ok(stat))`
    /// 2. If path has whiteout: `None`
    /// 3. If path not found: `Some(Err(NotFound))`
    /// 4. If directory has opaque marker: `None`
    /// 5. If IO error occurs: `Some(Err(io_error))`
    fn lookup_segment_by_segment(
        &self,
        layer_root: &Arc<InodeData>,
        path_segments: &[Symbol],
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

        current_stat = match Self::lstat_path(&root_vol_path) {
            Ok(stat) => stat,
            Err(e) => return Some(Err(e)),
        };

        // Traverse each path segment
        for segment in path_segments {
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

            match Self::lstat_path(&current_vol_path) {
                Ok(st) => {
                    // Update parent dev/ino for next iteration
                    parent_dev = st.st_dev as i32;
                    parent_ino = st.st_ino;
                    current_stat = st;
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

    /// Performs a lookup operation
    fn do_lookup(&self, parent: Inode, name: &CStr) -> io::Result<Entry> {
        // Get the parent inode data
        let parent_data = self.get_inode_data(parent)?;

        // Create path segments for lookup by appending the new name
        let mut path_segments = parent_data.path.clone();
        let symbol = self.intern_name(name)?;
        path_segments.push(symbol);

        // Start from the parent's layer and try each layer down to layer 0
        for layer_idx in (0..=parent_data.layer_idx).rev() {
            let layer_root = self.get_layer_root(layer_idx)?;

            match self.lookup_segment_by_segment(&layer_root, &path_segments) {
                Some(Ok(st)) => {
                    let alt_key = InodeAltKey::new(st.st_ino, st.st_dev as i32);

                    // Check if we already have this inode
                    let inodes = self.inodes.read().unwrap();
                    if let Some(data) = inodes.get_alt(&alt_key) {
                        data.refcount.fetch_add(1, Ordering::SeqCst);
                        return Ok(self.create_entry(data.inode, st));
                    }

                    drop(inodes);

                    // Create new inode
                    let (inode, _) = self.create_inode(
                        st.st_ino,
                        st.st_dev as i32,
                        path_segments.clone(),
                        layer_idx,
                    );
                    return Ok(self.create_entry(inode, st));
                }
                Some(Err(e)) if e.kind() == io::ErrorKind::NotFound => {
                    // Continue to check lower layers
                    continue;
                }
                Some(Err(e)) => return Err(e),
                None => {
                    // Hit a whiteout or opaque marker, stop searching lower layers
                    return Err(io::Error::from_raw_os_error(libc::ENOENT));
                }
            }
        }

        // Not found in any layer
        Err(io::Error::from_raw_os_error(libc::ENOENT))
    }

    /// Helper function to perform lstat on a path
    fn lstat_path(c_path: &CString) -> io::Result<bindings::stat64> {
        let mut st = MaybeUninit::<bindings::stat64>::zeroed();

        let ret = unsafe { libc::lstat(c_path.as_ptr(), st.as_mut_ptr() as *mut libc::stat) };
        if ret < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { st.assume_init() })
        }
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
        // TODO: Implement do_open
        todo!("implement do_open")
    }

    /// Performs a release operation
    fn do_release(&self, inode: Inode, handle: Handle) -> io::Result<()> {
        // TODO: Implement do_release
        todo!("implement do_release")
    }

    /// Performs a getattr operation
    fn do_getattr(&self, inode: Inode) -> io::Result<(bindings::stat64, Duration)> {
        // Get the path for this inode
        let path = self.inode_number_to_vol_path(inode)?;

        // Get file attributes
        let st = Self::lstat_path(&path)?;

        Ok((st, self.cfg.attr_timeout))
    }

    /// Performs an unlink operation
    fn do_unlink(
        &self,
        ctx: Context,
        parent: Inode,
        name: &CStr,
        flags: libc::c_int,
    ) -> io::Result<()> {
        // TODO: Implement do_unlink
        todo!("implement do_unlink")
    }

    /// Decrements the reference count for an inode and removes it if the count reaches zero
    fn forget_one(
        inodes: &mut MultikeyBTreeMap<Inode, InodeAltKey, Arc<InodeData>>,
        inode: Inode,
        count: u64,
    ) {
        if let Some(data) = inodes.get(&inode) {
            let previous = data.refcount.fetch_sub(count, Ordering::SeqCst);

            // If the reference count drops to zero or below, remove the inode
            if previous <= count {
                // Remove the inode from the map
                inodes.remove(&inode);
            }
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
        let mut opts = FsOptions::empty();

        // Enable writeback caching if requested and supported
        if self.cfg.writeback && capable.contains(FsOptions::WRITEBACK_CACHE) {
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
        self.do_lookup(parent, name)
    }

    fn forget(&self, _ctx: Context, inode: Self::Inode, count: u64) {
        // Skip forgetting the root inode
        if inode == self.init_inode {
            return;
        }

        let mut inodes = self.inodes.write().unwrap();
        Self::forget_one(&mut inodes, inode, count);
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
        // TODO: Set file attributes
        todo!("implement setattr")
    }

    fn readlink(&self, _ctx: Context, inode: Self::Inode) -> io::Result<Vec<u8>> {
        // TODO: Read the target of a symbolic link
        todo!("implement readlink")
    }

    fn mkdir(
        &self,
        _ctx: Context,
        parent: Self::Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        // Validate the name to prevent path traversal
        Self::validate_name(name)?;

        // Get the parent inode data
        let parent_data = self
            .inodes
            .read()
            .unwrap()
            .get(&parent)
            .ok_or_else(ebadf)?
            .clone();

        // Intern the name
        let symbol = self.intern_name(name)?;

        // Create the path for the new directory
        let mut dir_path = parent_data.path.clone();
        dir_path.push(symbol);

        // TODO: Create a directory
        todo!("implement mkdir")
    }

    fn unlink(&self, _ctx: Context, parent: Self::Inode, name: &CStr) -> io::Result<()> {
        // Validate the name to prevent path traversal
        Self::validate_name(name)?;

        // TODO: Remove a file
        todo!("implement unlink")
    }

    fn rmdir(&self, _ctx: Context, parent: Self::Inode, name: &CStr) -> io::Result<()> {
        // Validate the name to prevent path traversal
        Self::validate_name(name)?;

        // TODO: Remove a directory
        todo!("implement rmdir")
    }

    fn symlink(
        &self,
        _ctx: Context,
        linkname: &CStr,
        parent: Self::Inode,
        name: &CStr,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        // Validate the name to prevent path traversal
        Self::validate_name(name)?;

        // Get the parent inode data
        let parent_data = self
            .inodes
            .read()
            .unwrap()
            .get(&parent)
            .ok_or_else(ebadf)?
            .clone();

        // Intern the name
        let symbol = self.intern_name(name)?;

        // Create the path for the new symlink
        let mut link_path = parent_data.path.clone();
        link_path.push(symbol);

        // TODO: Create a symbolic link
        todo!("implement symlink")
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
        // Validate both names to prevent path traversal
        Self::validate_name(old_name)?;
        Self::validate_name(new_name)?;

        // Get the old parent inode data
        let old_parent_data = self
            .inodes
            .read()
            .unwrap()
            .get(&old_parent)
            .ok_or_else(ebadf)?
            .clone();

        // Get the new parent inode data
        let new_parent_data = self
            .inodes
            .read()
            .unwrap()
            .get(&new_parent)
            .ok_or_else(ebadf)?
            .clone();

        // Intern the old and new names
        let old_symbol = self.intern_name(old_name)?;
        let new_symbol = self.intern_name(new_name)?;

        // Create the old path
        let mut old_path = old_parent_data.path.clone();
        old_path.push(old_symbol);

        // Create the new path
        let mut new_path = new_parent_data.path.clone();
        new_path.push(new_symbol);

        // TODO: Rename a file
        todo!("implement rename")
    }

    fn link(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        new_parent: Self::Inode,
        new_name: &CStr,
    ) -> io::Result<Entry> {
        // Validate the name to prevent path traversal
        Self::validate_name(new_name)?;

        // Get the parent inode data
        let parent_data = self
            .inodes
            .read()
            .unwrap()
            .get(&new_parent)
            .ok_or_else(ebadf)?
            .clone();

        // Intern the name
        let symbol = self.intern_name(new_name)?;

        // Create the path for the new hard link
        let mut link_path = parent_data.path.clone();
        link_path.push(symbol);

        // TODO: Create a hard link
        todo!("implement link")
    }

    fn open(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        // TODO: Open a file
        todo!("implement open")
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
        // TODO: Read data from a file
        todo!("implement read")
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
        // TODO: Write data to a file
        todo!("implement write")
    }

    fn flush(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        _lock_owner: u64,
    ) -> io::Result<()> {
        // TODO: Flush file contents
        todo!("implement flush")
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
        // TODO: Release an open file
        todo!("implement release")
    }

    fn fsync(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        _datasync: bool,
        handle: Self::Handle,
    ) -> io::Result<()> {
        // TODO: Synchronize file contents
        todo!("implement fsync")
    }

    fn opendir(
        &self,
        _ctx: Context,
        inode: Self::Inode,
        flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        // TODO: Open a directory
        todo!("implement opendir")
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

        // Get the parent inode data
        let parent_data = self
            .inodes
            .read()
            .unwrap()
            .get(&parent)
            .ok_or_else(ebadf)?
            .clone();

        // Intern the name
        let symbol = self.intern_name(name)?;

        // Create the path for the new file
        let mut file_path = parent_data.path.clone();
        file_path.push(symbol);

        // TODO: Create and open a file
        todo!("implement create")
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            entry_timeout: Duration::from_secs(5),
            attr_timeout: Duration::from_secs(5),
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
