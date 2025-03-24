use std::{collections::HashSet, ffi::CString, fs, io};

use crate::virtio::{
    bindings::{self, LINUX_ENODATA, LINUX_ENOSYS},
    fs::filesystem::{Context, FileSystem, GetxattrReply, ListxattrReply},
    fuse::{FsOptions, SetattrValid},
    linux_errno::LINUX_ERANGE,
    macos::overlayfs::{Config, OverlayFs},
};

use super::helper;

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[test]
fn test_getattr_basic() -> io::Result<()> {
    // Create test layers:
    // Lower layer: file1 (mode 0644), dir1 (mode 0755), shadowed (mode 0644)
    // Upper layer: file2 (mode 0600), shadowed (mode 0600) - shadows lower layer's shadowed
    let layers = vec![
        vec![
            ("file1", false, 0o644),
            ("dir1", true, 0o755),
            ("shadowed", false, 0o644),
        ],
        vec![
            ("file2", false, 0o600),
            ("shadowed", false, 0o600), // This shadows the lower layer's shadowed file
        ],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test getattr on file in lower layer
    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(Context::default(), 1, &file1_name)?;
    let (file1_attr, _) = fs.getattr(Context::default(), file1_entry.inode, None)?;
    assert_eq!(file1_attr.st_mode & 0o777, 0o644);
    assert_eq!(file1_attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Test getattr on directory
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(Context::default(), 1, &dir1_name)?;
    let (dir1_attr, _) = fs.getattr(Context::default(), dir1_entry.inode, None)?;
    assert_eq!(dir1_attr.st_mode & 0o777, 0o755);
    assert_eq!(dir1_attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Test getattr on file in upper layer
    let file2_name = CString::new("file2").unwrap();
    let file2_entry = fs.lookup(Context::default(), 1, &file2_name)?;
    let (file2_attr, _) = fs.getattr(Context::default(), file2_entry.inode, None)?;
    assert_eq!(file2_attr.st_mode & 0o777, 0o600);
    assert_eq!(file2_attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Test getattr on shadowed file - should get attributes from upper layer
    let shadowed_name = CString::new("shadowed").unwrap();
    let shadowed_entry = fs.lookup(Context::default(), 1, &shadowed_name)?;
    let (shadowed_attr, _) = fs.getattr(Context::default(), shadowed_entry.inode, None)?;
    assert_eq!(
        shadowed_attr.st_mode & 0o777,
        0o600,
        "Should get mode from upper layer's shadowed file"
    );
    assert_eq!(shadowed_attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    Ok(())
}

#[test]
fn test_getattr_invalid_inode() -> io::Result<()> {
    // Create a simple test layer
    let layers = vec![vec![("file1", false, 0o644)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test getattr with invalid inode
    let invalid_inode = 999999;
    let result = fs.getattr(Context::default(), invalid_inode, None);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EBADF));

    Ok(())
}

#[test]
fn test_getattr_whiteout() -> io::Result<()> {
    // Create test layers:
    // Lower layer: file1
    // Upper layer: .wh.file1 (whiteout for file1)
    let layers = vec![
        vec![("file1", false, 0o644)],
        vec![(".wh.file1", false, 0o644)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Try to lookup and getattr whited-out file
    let file1_name = CString::new("file1").unwrap();
    assert!(fs.lookup(Context::default(), 1, &file1_name).is_err());

    Ok(())
}

#[test]
fn test_getattr_timestamps() -> io::Result<()> {
    // Create test layers with a single file
    let layers = vec![vec![("file1", false, 0o644)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Get the file's attributes
    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(Context::default(), 1, &file1_name)?;
    let (file1_attr, timeout) = fs.getattr(Context::default(), file1_entry.inode, None)?;

    // Verify that timestamps are present
    assert!(file1_attr.st_atime > 0);
    assert!(file1_attr.st_mtime > 0);
    assert!(file1_attr.st_ctime > 0);

    // Verify that the timeout matches the configuration
    assert_eq!(timeout, fs.get_config().attr_timeout);

    Ok(())
}

#[test]
fn test_getattr_complex() -> io::Result<()> {
    // Create test layers with complex directory structure and various shadowing/opaque scenarios:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/file1 (mode 0644)
    //   - dir1/subdir/
    //   - dir1/subdir/bottom_file (mode 0644)
    //   - dir2/
    //   - dir2/file2 (mode 0644)
    // Layer 1 (middle):
    //   - dir1/ (with opaque marker)
    //   - dir1/file1 (mode 0600) - shadows bottom but visible due to opaque
    //   - dir1/middle_file (mode 0600)
    //   - dir2/file2 (mode 0600) - shadows bottom
    // Layer 2 (top):
    //   - dir1/
    //   - dir1/top_file (mode 0666)
    //   - dir2/ (with opaque marker)
    //   - dir2/new_file (mode 0666)
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/bottom_file", false, 0o644),
            ("dir2", true, 0o755),
            ("dir2/file2", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/.wh..wh..opq", false, 0o644), // Makes dir1 opaque
            ("dir1/file1", false, 0o600),        // Shadows but visible due to opaque
            ("dir1/middle_file", false, 0o600),
            ("dir2", true, 0o755),
            ("dir2/file2", false, 0o600), // Shadows bottom layer
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/top_file", false, 0o666),
            ("dir2", true, 0o755),
            ("dir2/.wh..wh..opq", false, 0o644), // Makes dir2 opaque
            ("dir2/new_file", false, 0o666),
        ],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test 1: Files in dir1 (with opaque marker in middle layer)
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(Context::default(), 1, &dir1_name)?;

    // 1a. file1 should have mode 0600 from middle layer (due to opaque marker), not 0644 from bottom
    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(Context::default(), dir1_entry.inode, &file1_name)?;
    let (file1_attr, _) = fs.getattr(Context::default(), file1_entry.inode, None)?;
    assert_eq!(
        file1_attr.st_mode & 0o777,
        0o600,
        "file1 should have mode from middle layer due to opaque marker"
    );

    // 1b. bottom_file should not be visible due to opaque marker in middle layer
    let bottom_file_name = CString::new("bottom_file").unwrap();
    assert!(
        fs.lookup(Context::default(), dir1_entry.inode, &bottom_file_name)
            .is_err(),
        "bottom_file should be hidden by opaque marker"
    );

    // 1c. middle_file should be visible with mode 0600
    let middle_file_name = CString::new("middle_file").unwrap();
    let middle_file_entry = fs.lookup(Context::default(), dir1_entry.inode, &middle_file_name)?;
    let (middle_file_attr, _) = fs.getattr(Context::default(), middle_file_entry.inode, None)?;
    assert_eq!(middle_file_attr.st_mode & 0o777, 0o600);

    // 1d. top_file should be visible with mode 0666
    let top_file_name = CString::new("top_file").unwrap();
    let top_file_entry = fs.lookup(Context::default(), dir1_entry.inode, &top_file_name)?;
    let (top_file_attr, _) = fs.getattr(Context::default(), top_file_entry.inode, None)?;
    assert_eq!(top_file_attr.st_mode & 0o777, 0o666);

    // Test 2: Files in dir2 (with opaque marker in top layer)
    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(Context::default(), 1, &dir2_name)?;

    // 2a. file2 from bottom and middle layers should not be visible due to opaque marker in top
    let file2_name = CString::new("file2").unwrap();
    assert!(
        fs.lookup(Context::default(), dir2_entry.inode, &file2_name)
            .is_err(),
        "file2 should be hidden by opaque marker in top layer"
    );

    // 2b. new_file should be visible with mode 0666
    let new_file_name = CString::new("new_file").unwrap();
    let new_file_entry = fs.lookup(Context::default(), dir2_entry.inode, &new_file_name)?;
    let (new_file_attr, _) = fs.getattr(Context::default(), new_file_entry.inode, None)?;
    assert_eq!(new_file_attr.st_mode & 0o777, 0o666);

    // Test 3: Directory attributes
    // 3a. dir1 should exist and be a directory
    let (dir1_attr, _) = fs.getattr(Context::default(), dir1_entry.inode, None)?;
    assert_eq!(dir1_attr.st_mode & libc::S_IFMT, libc::S_IFDIR);
    assert_eq!(dir1_attr.st_mode & 0o777, 0o755);

    // 3b. dir2 should exist and be a directory
    let (dir2_attr, _) = fs.getattr(Context::default(), dir2_entry.inode, None)?;
    assert_eq!(dir2_attr.st_mode & libc::S_IFMT, libc::S_IFDIR);
    assert_eq!(dir2_attr.st_mode & 0o777, 0o755);

    Ok(())
}

#[test]
fn test_setattr_basic() -> io::Result<()> {
    // Create test layers:
    // Lower layer: file1 (mode 0644)
    // Upper layer: file2 (mode 0600)
    let layers = vec![vec![("file1", false, 0o644)], vec![("file2", false, 0o600)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, true)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test setattr on file in upper layer
    let file2_name = CString::new("file2").unwrap();
    let file2_entry = fs.lookup(Context::default(), 1, &file2_name)?;

    // Change mode to 0640
    let mut attr = file2_entry.attr;
    attr.st_mode = (attr.st_mode & !0o777) | 0o640;
    let valid = SetattrValid::MODE;
    let (new_attr, _) = fs.setattr(Context::default(), file2_entry.inode, attr, None, valid)?;
    assert_eq!(new_attr.st_mode & 0o777, 0o640);

    // Verify the change was applied to the filesystem
    let (verify_attr, _) = fs.getattr(Context::default(), file2_entry.inode, None)?;
    assert_eq!(verify_attr.st_mode & 0o777, 0o640);

    Ok(())
}

#[test]
fn test_setattr_copy_up() -> io::Result<()> {
    // Create test layers:
    // Lower layer: file1 (mode 0644)
    // Upper layer: empty (file1 will be copied up)
    let layers = vec![vec![("file1", false, 0o644)], vec![]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, true)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test setattr on file in lower layer (should trigger copy_up)
    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(Context::default(), 1, &file1_name)?;

    // Change mode to 0640
    let mut attr = file1_entry.attr;
    attr.st_mode = (attr.st_mode & !0o777) | 0o640;
    let valid = SetattrValid::MODE;
    let (new_attr, _) = fs.setattr(Context::default(), file1_entry.inode, attr, None, valid)?;
    assert_eq!(new_attr.st_mode & 0o777, 0o640);

    Ok(())
}

#[test]
fn test_setattr_timestamps() -> io::Result<()> {
    // Create test layers with a single file
    let layers = vec![vec![("file1", false, 0o644)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Get the file's entry
    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(Context::default(), 1, &file1_name)?;

    // Set specific timestamps
    let mut attr = file1_entry.attr;
    attr.st_atime = 12345;
    attr.st_atime_nsec = 67890;
    attr.st_mtime = 98765;
    attr.st_mtime_nsec = 43210;

    let valid = SetattrValid::ATIME | SetattrValid::MTIME;
    let (new_attr, _) = fs.setattr(Context::default(), file1_entry.inode, attr, None, valid)?;

    // Verify timestamps were set
    assert_eq!(new_attr.st_atime, 12345);
    assert_eq!(new_attr.st_atime_nsec, 67890);
    assert_eq!(new_attr.st_mtime, 98765);
    assert_eq!(new_attr.st_mtime_nsec, 43210);

    Ok(())
}

#[test]
fn test_setattr_size() -> io::Result<()> {
    // Create test layers with a single file
    let layers = vec![vec![("file1", false, 0o644)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Get the file's entry
    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(Context::default(), 1, &file1_name)?;

    // Set file size to 1000 bytes
    let mut attr = file1_entry.attr;
    attr.st_size = 1000;
    let valid = SetattrValid::SIZE;
    let (new_attr, _) = fs.setattr(Context::default(), file1_entry.inode, attr, None, valid)?;

    // Verify size was set
    assert_eq!(new_attr.st_size, 1000);

    // Verify the actual file size on disk
    let file_path = temp_dirs[0].path().join("file1");
    let metadata = fs::metadata(file_path)?;
    assert_eq!(metadata.len(), 1000);

    Ok(())
}

#[test]
fn test_setattr_complex() -> io::Result<()> {
    // Create test layers with complex structure:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/file1 (mode 0644)
    //   - dir1/subdir/
    //   - dir1/subdir/bottom_file (mode 0644)
    // Layer 1 (middle):
    //   - dir2/
    //   - dir2/file2 (mode 0600)
    // Layer 2 (top):
    //   - dir3/
    //   - dir3/file3 (mode 0666)
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/bottom_file", false, 0o644),
        ],
        vec![("dir2", true, 0o755), ("dir2/file2", false, 0o600)],
        vec![("dir3", true, 0o755), ("dir3/file3", false, 0o666)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test 1: Modify file in bottom layer (should trigger copy_up)
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(Context::default(), 1, &dir1_name)?;
    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(Context::default(), dir1_entry.inode, &file1_name)?;

    // Change mode and size
    let mut attr = file1_entry.attr;
    attr.st_mode = (attr.st_mode & !0o777) | 0o640;
    attr.st_size = 2000;
    let valid = SetattrValid::MODE | SetattrValid::SIZE;
    let (new_attr, _) = fs.setattr(Context::default(), file1_entry.inode, attr, None, valid)?;

    // Verify changes
    assert_eq!(new_attr.st_mode & 0o777, 0o640);
    assert_eq!(new_attr.st_size, 2000);

    // Test 2: Modify file in middle layer (should trigger copy_up)
    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(Context::default(), 1, &dir2_name)?;
    let file2_name = CString::new("file2").unwrap();
    let file2_entry = fs.lookup(Context::default(), dir2_entry.inode, &file2_name)?;

    // Change timestamps
    let mut attr = file2_entry.attr;
    attr.st_atime = 12345;
    attr.st_mtime = 67890;
    let valid = SetattrValid::ATIME | SetattrValid::MTIME;
    let (new_attr, _) = fs.setattr(Context::default(), file2_entry.inode, attr, None, valid)?;

    // Verify changes
    assert_eq!(new_attr.st_atime, 12345);
    assert_eq!(new_attr.st_mtime, 67890);

    // Verify file was copied up
    let top_file2_path = temp_dirs[2].path().join("dir2").join("file2");
    assert!(top_file2_path.exists());

    // Test 3: Modify file in top layer (no copy_up needed)
    let dir3_name = CString::new("dir3").unwrap();
    let dir3_entry = fs.lookup(Context::default(), 1, &dir3_name)?;
    let file3_name = CString::new("file3").unwrap();
    let file3_entry = fs.lookup(Context::default(), dir3_entry.inode, &file3_name)?;

    // Change mode
    let mut attr = file3_entry.attr;
    attr.st_mode = (attr.st_mode & !0o777) | 0o644;
    let valid = SetattrValid::MODE;
    let (new_attr, _) = fs.setattr(Context::default(), file3_entry.inode, attr, None, valid)?;

    // Verify changes
    assert_eq!(new_attr.st_mode & 0o777, 0o644);

    Ok(())
}

#[test]
fn test_xattrs() -> io::Result<()> {
    // Create test layers with nested structure:
    // Layer 0 (bottom): dir1/file1.txt, dir2/file2.txt
    // Layer 1 (middle): dir1/file3.txt, dir3/file4.txt
    // Layer 2 (top): dir1/file5.txt, dir2/dir4/file6.txt
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1.txt", false, 0o644),
            ("dir2", true, 0o755),
            ("dir2/file2.txt", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/file3.txt", false, 0o644),
            ("dir3", true, 0o755),
            ("dir3/file4.txt", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/file5.txt", false, 0o644),
            ("dir2", true, 0o755),
            ("dir2/dir4", true, 0o755),
            ("dir2/dir4/file6.txt", false, 0o644),
        ],
    ];

    // Enable xattr in config
    let mut cfg = Config::default();
    cfg.xattr = true;

    // Create overlay filesystem with the specified layers
    let temp_dirs = layers
        .iter()
        .map(|layer| helper::setup_test_layer(layer).unwrap())
        .collect::<Vec<_>>();

    let layer_paths = temp_dirs
        .iter()
        .map(|dir| dir.path().to_path_buf())
        .collect::<Vec<_>>();

    cfg.layers = layer_paths;

    let overlayfs = OverlayFs::new(cfg)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    overlayfs.init(FsOptions::empty())?;
    let ctx = Context::default();

    // ---------- Test setting, getting, listing, and removing xattrs on files in different layers ----------

    // Look up dir1
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = overlayfs.lookup(ctx, 1, &dir1_name)?;

    // Test file in top layer (dir1/file5.txt)
    let file5_name = CString::new("file5.txt").unwrap();
    let file5_entry = overlayfs.lookup(ctx, dir1_entry.inode, &file5_name)?;

    // Test setxattr on top layer file
    let xattr_name = CString::new("user.test_attr").unwrap();
    let xattr_value = b"test_value_123";
    overlayfs.setxattr(ctx, file5_entry.inode, &xattr_name, xattr_value, 0)?;

    // Test getxattr
    let result = overlayfs.getxattr(ctx, file5_entry.inode, &xattr_name, 100);
    match result {
        Ok(GetxattrReply::Value(value)) => {
            assert_eq!(value, xattr_value);
        }
        Err(e) => panic!("Expected GetxattrReply::Value, got error: {:?}", e),
        _ => panic!("Unexpected result from getxattr"),
    }

    // Test listxattr
    let result = overlayfs.listxattr(ctx, file5_entry.inode, 100);
    match result {
        Ok(ListxattrReply::Names(names)) => {
            let mut found = false;
            let mut start = 0;
            while start < names.len() {
                let end = names[start..]
                    .iter()
                    .position(|&b| b == 0)
                    .map(|pos| start + pos)
                    .unwrap_or(names.len());

                let attr_name = &names[start..end];
                if attr_name == xattr_name.to_bytes() {
                    found = true;
                    break;
                }
                start = end + 1;
            }
            assert!(found, "Attribute name not found in listxattr result");
        }
        Err(e) => panic!("Expected ListxattrReply::Names, got error: {:?}", e),
        _ => panic!("Unexpected result from listxattr"),
    }

    // Test setting another attribute
    let xattr_name2 = CString::new("user.another_attr").unwrap();
    let xattr_value2 = b"another_value_456";
    overlayfs.setxattr(ctx, file5_entry.inode, &xattr_name2, xattr_value2, 0)?;

    // Verify both attributes are listed
    let result = overlayfs.listxattr(ctx, file5_entry.inode, 200);
    match result {
        Ok(ListxattrReply::Names(names)) => {
            let mut attrs = HashSet::new();
            let mut start = 0;
            while start < names.len() {
                let end = names[start..]
                    .iter()
                    .position(|&b| b == 0)
                    .map(|pos| start + pos)
                    .unwrap_or(names.len());

                let attr_name = &names[start..end];
                attrs.insert(attr_name.to_vec());
                start = end + 1;
            }
            assert!(
                attrs.contains(&xattr_name.to_bytes().to_vec()),
                "First attribute not found"
            );
            assert!(
                attrs.contains(&xattr_name2.to_bytes().to_vec()),
                "Second attribute not found"
            );
        }
        Err(e) => panic!("Expected ListxattrReply::Names, got error: {:?}", e),
        _ => panic!("Unexpected result from listxattr"),
    }

    // Test removexattr
    overlayfs.removexattr(ctx, file5_entry.inode, &xattr_name)?;

    // Verify the attribute was removed
    let result = overlayfs.listxattr(ctx, file5_entry.inode, 100);
    match result {
        Ok(ListxattrReply::Names(names)) => {
            let mut found = false;
            let mut start = 0;
            while start < names.len() {
                let end = names[start..]
                    .iter()
                    .position(|&b| b == 0)
                    .map(|pos| start + pos)
                    .unwrap_or(names.len());

                let attr_name = &names[start..end];
                if attr_name == xattr_name.to_bytes() {
                    found = true;
                    break;
                }
                start = end + 1;
            }
            assert!(!found, "Attribute should have been removed");
        }
        Err(e) => panic!("Expected ListxattrReply::Names, got error: {:?}", e),
        _ => panic!("Unexpected result from listxattr"),
    }

    // ---------- Test xattrs on files in middle layer (should trigger copy-up) ----------

    // Look up dir3
    let dir3_name = CString::new("dir3").unwrap();
    let dir3_entry = overlayfs.lookup(ctx, 1, &dir3_name)?;

    // Test file in middle layer (dir3/file4.txt)
    let file4_name = CString::new("file4.txt").unwrap();
    let file4_entry = overlayfs.lookup(ctx, dir3_entry.inode, &file4_name)?;

    // Verify file exists in middle layer before copy-up
    let middle_layer_file = temp_dirs[1].path().join("dir3").join("file4.txt");
    assert!(
        middle_layer_file.exists(),
        "File should exist in middle layer before copy-up"
    );
    assert!(
        !temp_dirs[2].path().join("dir3").join("file4.txt").exists(),
        "File should not exist in top layer before copy-up"
    );

    // This should cause a copy-up operation since the file is in a lower layer
    let middle_xattr_name = CString::new("user.middle_attr").unwrap();
    let middle_xattr_value = b"middle_layer_value";
    overlayfs.setxattr(
        ctx,
        file4_entry.inode,
        &middle_xattr_name,
        middle_xattr_value,
        0,
    )?;

    // Verify file was copied up to top layer
    let top_layer_file = temp_dirs[2].path().join("dir3").join("file4.txt");
    assert!(
        top_layer_file.exists(),
        "File should be copied up to top layer"
    );

    // Verify the attribute was set on the top layer file
    let result = overlayfs.getxattr(ctx, file4_entry.inode, &middle_xattr_name, 100);
    match result {
        Ok(GetxattrReply::Value(value)) => {
            assert_eq!(value, middle_xattr_value);
        }
        Err(e) => panic!("Expected GetxattrReply::Value, got error: {:?}", e),
        _ => panic!("Unexpected result from getxattr"),
    }

    // Verify the middle layer file still exists and is unchanged (no xattr)
    assert!(
        middle_layer_file.exists(),
        "Original file should still exist in middle layer"
    );
    let result = overlayfs.getxattr(ctx, file4_entry.inode, &middle_xattr_name, 100);
    match result {
        Ok(GetxattrReply::Value(value)) => {
            assert_eq!(
                value, middle_xattr_value,
                "Xattr should be accessible through overlay"
            );
        }
        Err(e) => panic!("Expected GetxattrReply::Value, got error: {:?}", e),
        _ => panic!("Unexpected result from getxattr"),
    }

    // Try to read the xattr directly from the middle layer file (should not exist)
    let middle_layer_path = CString::new(middle_layer_file.to_str().unwrap()).unwrap();
    let mut buf = vec![0; 100];
    let res = unsafe {
        libc::getxattr(
            middle_layer_path.as_ptr(),
            middle_xattr_name.as_ptr(),
            buf.as_mut_ptr() as *mut libc::c_void,
            buf.len(),
            0,
            0,
        )
    };
    assert!(res < 0, "Xattr should not exist on middle layer file");
    let err = io::Error::last_os_error();
    assert!(
        err.raw_os_error().unwrap() == libc::ENOATTR
            || err.raw_os_error().unwrap() == libc::ENODATA,
        "Expected ENOATTR or ENODATA when reading xattr from middle layer file"
    );

    // ---------- Test xattrs on nested directories ----------

    // Look up dir2/dir4
    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = overlayfs.lookup(ctx, 1, &dir2_name)?;

    let dir4_name = CString::new("dir4").unwrap();
    let dir4_entry = overlayfs.lookup(ctx, dir2_entry.inode, &dir4_name)?;

    // Set xattr on a nested directory
    let dir_xattr_name = CString::new("user.dir_attr").unwrap();
    let dir_xattr_value = b"directory_attribute";
    overlayfs.setxattr(ctx, dir4_entry.inode, &dir_xattr_name, dir_xattr_value, 0)?;

    // Verify the attribute was set
    let result = overlayfs.getxattr(ctx, dir4_entry.inode, &dir_xattr_name, 100);
    match result {
        Ok(GetxattrReply::Value(value)) => {
            assert_eq!(value, dir_xattr_value);
        }
        Err(e) => panic!("Expected GetxattrReply::Value, got error: {:?}", e),
        _ => panic!("Unexpected result from getxattr"),
    }

    // ---------- Test xattrs on file in deeply nested directory ----------

    // Get file in nested directory (dir2/dir4/file6.txt)
    let file6_name = CString::new("file6.txt").unwrap();
    let file6_entry = overlayfs.lookup(ctx, dir4_entry.inode, &file6_name)?;

    // Set xattr on the nested file
    let nested_xattr_name = CString::new("user.nested_attr").unwrap();
    let nested_xattr_value = b"nested_file_value";
    overlayfs.setxattr(
        ctx,
        file6_entry.inode,
        &nested_xattr_name,
        nested_xattr_value,
        0,
    )?;

    // Verify the attribute was set
    let result = overlayfs.getxattr(ctx, file6_entry.inode, &nested_xattr_name, 100);
    match result {
        Ok(GetxattrReply::Value(value)) => {
            assert_eq!(value, nested_xattr_value);
        }
        Err(e) => panic!("Expected GetxattrReply::Value, got error: {:?}", e),
        _ => panic!("Unexpected result from getxattr"),
    }

    // ---------- Test error cases ----------

    // Test getxattr on non-existent attribute
    let nonexistent_attr = CString::new("user.nonexistent").unwrap();
    let result = overlayfs.getxattr(ctx, file6_entry.inode, &nonexistent_attr, 100);
    match result {
        Err(e) => {
            let err_code = e.raw_os_error().unwrap();
            assert!(
                err_code == LINUX_ENODATA,
                "Expected ENODATA, got: {}",
                err_code
            );
        }
        Ok(_) => panic!("Expected error for non-existent attribute"),
    }

    // Test getxattr with buffer too small
    let result = overlayfs.getxattr(ctx, file6_entry.inode, &nested_xattr_name, 5);
    match result {
        Err(e) => {
            assert_eq!(
                e.raw_os_error().unwrap(),
                LINUX_ERANGE,
                "Expected ERANGE error"
            );
        }
        Ok(_) => panic!("Expected ERANGE error for small buffer"),
    }

    // Test removexattr on non-existent attribute
    let result = overlayfs.removexattr(ctx, file6_entry.inode, &nonexistent_attr);
    match result {
        Err(e) => {
            let err_code = e.raw_os_error().unwrap();
            assert!(
                err_code == LINUX_ENODATA,
                "Expected ENODATA, got: {}",
                err_code
            );
        }
        Ok(_) => panic!("Expected error for non-existent attribute"),
    }

    // Test setting xattr with invalid flags (flag value 2 is XATTR_CREATE, which should fail if attr exists)
    let result = overlayfs.setxattr(
        ctx,
        file6_entry.inode,
        &nested_xattr_name,
        nested_xattr_value,
        bindings::LINUX_XATTR_CREATE as u32, // XATTR_CREATE - should fail on existing attr
    );
    match result {
        Err(e) => {
            assert_eq!(
                e.raw_os_error().unwrap(),
                libc::EEXIST,
                "Expected EEXIST error"
            );
        }
        Ok(_) => panic!("Expected EEXIST error for XATTR_CREATE on existing attribute"),
    }

    // ---------- Test disabling xattr functionality ----------

    // Create a new overlayfs with xattr disabled
    let mut cfg_no_xattr = Config::default();
    cfg_no_xattr.xattr = false;
    cfg_no_xattr.layers = temp_dirs
        .iter()
        .map(|dir| dir.path().to_path_buf())
        .collect();

    let overlayfs_no_xattr = OverlayFs::new(cfg_no_xattr)?;

    overlayfs_no_xattr.init(FsOptions::empty())?;

    // Look up a file again
    let dir1_entry = overlayfs_no_xattr.lookup(ctx, 1, &dir1_name)?;
    let file5_entry = overlayfs_no_xattr.lookup(ctx, dir1_entry.inode, &file5_name)?;

    // All xattr operations should return ENOSYS
    let result = overlayfs_no_xattr.setxattr(ctx, file5_entry.inode, &xattr_name, b"test", 0);
    match result {
        Err(e) => {
            assert_eq!(
                e.raw_os_error().unwrap(),
                LINUX_ENOSYS,
                "Expected ENOSYS error"
            );
        }
        Ok(_) => panic!("Expected ENOSYS error when xattr is disabled"),
    }

    let result = overlayfs_no_xattr.getxattr(ctx, file5_entry.inode, &xattr_name, 100);
    match result {
        Err(e) => {
            assert_eq!(
                e.raw_os_error().unwrap(),
                LINUX_ENOSYS,
                "Expected ENOSYS error"
            );
        }
        Ok(_) => panic!("Expected ENOSYS error when xattr is disabled"),
    }

    let result = overlayfs_no_xattr.listxattr(ctx, file5_entry.inode, 100);
    match result {
        Err(e) => {
            assert_eq!(
                e.raw_os_error().unwrap(),
                LINUX_ENOSYS,
                "Expected ENOSYS error"
            );
        }
        Ok(_) => panic!("Expected ENOSYS error when xattr is disabled"),
    }

    let result = overlayfs_no_xattr.removexattr(ctx, file5_entry.inode, &xattr_name);
    match result {
        Err(e) => {
            assert_eq!(
                e.raw_os_error().unwrap(),
                LINUX_ENOSYS,
                "Expected ENOSYS error"
            );
        }
        Ok(_) => panic!("Expected ENOSYS error when xattr is disabled"),
    }

    Ok(())
}
