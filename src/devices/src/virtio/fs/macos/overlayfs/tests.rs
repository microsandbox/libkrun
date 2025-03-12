use std::os::unix::fs::PermissionsExt;
use std::{ffi::CString, fs, io};

use crate::virtio::{
    fs::filesystem::{Context, Extensions, FileSystem, SetattrValid},
    fuse::FsOptions,
};

use super::*;

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[test]
fn test_lookup_basic() -> io::Result<()> {
    // Create test layers:
    // Lower layer: file1, dir1/file2
    // Upper layer: file3
    let layers = vec![
        vec![
            ("file1", false, 0o644),
            ("dir1", true, 0o755),
            ("dir1/file2", false, 0o644),
        ],
        vec![("file3", false, 0o644)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test lookup in top layer
    let file3_name = CString::new("file3").unwrap();
    let entry = fs.lookup(Context::default(), 1, &file3_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Test lookup in lower layer
    let file1_name = CString::new("file1").unwrap();
    let entry = fs.lookup(Context::default(), 1, &file1_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Test lookup of directory
    let dir1_name = CString::new("dir1").unwrap();
    let entry = fs.lookup(Context::default(), 1, &dir1_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    Ok(())
}

#[test]
fn test_lookup_whiteout() -> io::Result<()> {
    // Create test layers:
    // Lower layer: file1, file2
    // Upper layer: .wh.file1 (whiteout for file1)
    let layers = vec![
        vec![("file1", false, 0o644), ("file2", false, 0o644)],
        vec![(".wh.file1", false, 0o644)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test lookup of whited-out file
    let file1_name = CString::new("file1").unwrap();
    assert!(fs.lookup(Context::default(), 1, &file1_name).is_err());

    // Test lookup of non-whited-out file
    let file2_name = CString::new("file2").unwrap();
    let entry = fs.lookup(Context::default(), 1, &file2_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    Ok(())
}

#[test]
fn test_lookup_opaque_dir() -> io::Result<()> {
    // Create test layers:
    // Lower layer: dir1/file1, dir1/file2
    // Upper layer: dir1/.wh..wh..opq, dir1/file3
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/file2", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/.wh..wh..opq", false, 0o644),
            ("dir1/file3", false, 0o644),
        ],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Lookup dir1 first
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(Context::default(), 1, &dir1_name)?;

    // Test lookup of file in opaque directory
    // file1 and file2 should not be visible
    let file1_name = CString::new("file1").unwrap();
    assert!(fs
        .lookup(Context::default(), dir1_entry.inode, &file1_name)
        .is_err());

    let file2_name = CString::new("file2").unwrap();
    assert!(fs
        .lookup(Context::default(), dir1_entry.inode, &file2_name)
        .is_err());

    // file3 should be visible
    let file3_name = CString::new("file3").unwrap();
    let entry = fs.lookup(Context::default(), dir1_entry.inode, &file3_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    Ok(())
}

#[test]
fn test_lookup_multiple_layers() -> io::Result<()> {
    // Create test layers:
    // Lower layer 1: file1
    // Lower layer 2: file2
    // Upper layer: file3
    let layers = vec![
        vec![("file1", false, 0o644)],
        vec![("file2", false, 0o644)],
        vec![("file3", false, 0o644)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test lookup in each layer
    let file1_name = CString::new("file1").unwrap();
    let entry = fs.lookup(Context::default(), 1, &file1_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    let file2_name = CString::new("file2").unwrap();
    let entry = fs.lookup(Context::default(), 1, &file2_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    let file3_name = CString::new("file3").unwrap();
    let entry = fs.lookup(Context::default(), 1, &file3_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    Ok(())
}

#[test]
fn test_lookup_nested_whiteouts() -> io::Result<()> {
    // Create test layers:
    // Lower layer: dir1/file1, dir2/file2
    // Middle layer: dir1/.wh.file1, .wh.dir2
    // Upper layer: dir1/file3
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir2", true, 0o755),
            ("dir2/file2", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/.wh.file1", false, 0o644),
            (".wh.dir2", false, 0o644),
        ],
        vec![("dir1", true, 0o755), ("dir1/file3", false, 0o644)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Lookup dir1
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(Context::default(), 1, &dir1_name)?;

    // file1 should be whited out
    let file1_name = CString::new("file1").unwrap();
    assert!(fs
        .lookup(Context::default(), dir1_entry.inode, &file1_name)
        .is_err());

    // file3 should be visible
    let file3_name = CString::new("file3").unwrap();
    let entry = fs.lookup(Context::default(), dir1_entry.inode, &file3_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // dir2 should be whited out
    let dir2_name = CString::new("dir2").unwrap();
    assert!(fs.lookup(Context::default(), 1, &dir2_name).is_err());

    Ok(())
}

#[test]
fn test_lookup_complex_layers() -> io::Result<()> {
    // Create test layers with complex directory structure:
    // Layer 0 (bottom): bar, bar/hi, bar/hi/txt
    // Layer 1: foo, foo/hello, bar
    // Layer 2: bar, bar/hi, bar/hi/xml
    // Layer 3 (top): bar, bar/hello, bar/hi, bar/hi/json
    let layers = vec![
        vec![
            ("bar", true, 0o755),
            ("bar/hi", true, 0o755),
            ("bar/hi/txt", false, 0o644),
        ],
        vec![
            ("foo", true, 0o755),
            ("foo/hello", false, 0o644),
            ("bar", true, 0o755),
        ],
        vec![
            ("bar", true, 0o755),
            ("bar/hi", true, 0o755),
            ("bar/hi/xml", false, 0o644),
        ],
        vec![
            ("bar", true, 0o755),
            ("bar/hello", false, 0o644),
            ("bar/hi", true, 0o755),
            ("bar/hi/json", false, 0o644),
        ],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // First lookup 'bar' directory
    let bar_name = CString::new("bar").unwrap();
    let bar_entry = fs.lookup(Context::default(), 1, &bar_name)?;
    assert_eq!(bar_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Then lookup 'hi' in bar directory
    let hi_name = CString::new("hi").unwrap();
    let hi_entry = fs.lookup(Context::default(), bar_entry.inode, &hi_name)?;
    assert_eq!(hi_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Finally lookup 'txt' in bar/hi directory - should find it in layer 0
    let txt_name = CString::new("txt").unwrap();
    let txt_entry = fs.lookup(Context::default(), hi_entry.inode, &txt_name)?;
    assert_eq!(txt_entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Verify we can also find files from other layers
    // Lookup 'json' in bar/hi - should find it in layer 3 (top)
    let json_name = CString::new("json").unwrap();
    let json_entry = fs.lookup(Context::default(), hi_entry.inode, &json_name)?;
    assert_eq!(json_entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Lookup 'xml' in bar/hi - should find it in layer 2
    let xml_name = CString::new("xml").unwrap();
    let xml_entry = fs.lookup(Context::default(), hi_entry.inode, &xml_name)?;
    assert_eq!(xml_entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Lookup 'hello' in bar - should find it in layer 3
    let hello_name = CString::new("hello").unwrap();
    let hello_entry = fs.lookup(Context::default(), bar_entry.inode, &hello_name)?;
    assert_eq!(hello_entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Lookup 'foo' in root - should find it in layer 1
    let foo_name = CString::new("foo").unwrap();
    let foo_entry = fs.lookup(Context::default(), 1, &foo_name)?;
    assert_eq!(foo_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Lookup 'hello' in foo - should find it in layer 1
    let foo_hello_name = CString::new("hello").unwrap();
    let foo_hello_entry = fs.lookup(Context::default(), foo_entry.inode, &foo_hello_name)?;
    assert_eq!(foo_hello_entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    Ok(())
}

#[test]
fn test_lookup_complex_opaque_dirs() -> io::Result<()> {
    // Create test layers with complex directory structure and opaque directories:
    // Layer 0 (bottom):
    //   - bar/
    //   - bar/file1
    //   - bar/subdir/
    //   - bar/subdir/bottom_file
    //   - other/
    //   - other/file
    // Layer 1:
    //   - bar/ (with opaque marker)
    //   - bar/file2
    //   - extra/
    //   - extra/data
    // Layer 2 (top):
    //   - bar/
    //   - bar/file3
    //   - bar/subdir/
    //   - bar/subdir/top_file
    //   - other/
    //   - other/new_file

    let layers = vec![
        vec![
            ("bar", true, 0o755),
            ("bar/file1", false, 0o644),
            ("bar/subdir", true, 0o755),
            ("bar/subdir/bottom_file", false, 0o644),
            ("other", true, 0o755),
            ("other/file", false, 0o644),
        ],
        vec![
            ("bar", true, 0o755),
            ("bar/.wh..wh..opq", false, 0o644),
            ("bar/file2", false, 0o644),
            ("extra", true, 0o755),
            ("extra/data", false, 0o644),
        ],
        vec![
            ("bar", true, 0o755),
            ("bar/file3", false, 0o644),
            ("bar/subdir", true, 0o755),
            ("bar/subdir/top_file", false, 0o644),
            ("other", true, 0o755),
            ("other/new_file", false, 0o644),
        ],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // First lookup 'bar' directory
    let bar_name = CString::new("bar").unwrap();
    let bar_entry = fs.lookup(Context::default(), 1, &bar_name)?;
    assert_eq!(bar_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Lookup 'file1' in bar - should NOT be found due to opaque marker in layer 1
    let file1_name = CString::new("file1").unwrap();
    let file1_result = fs.lookup(Context::default(), bar_entry.inode, &file1_name);
    assert!(
        file1_result.is_err(),
        "file1 should be hidden by opaque directory"
    );

    // Lookup 'file2' in bar - should be found in layer 1
    let file2_name = CString::new("file2").unwrap();
    let file2_entry = fs.lookup(Context::default(), bar_entry.inode, &file2_name)?;
    assert_eq!(file2_entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Lookup 'file3' in bar - should be found in layer 2
    let file3_name = CString::new("file3").unwrap();
    let file3_entry = fs.lookup(Context::default(), bar_entry.inode, &file3_name)?;
    assert_eq!(file3_entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Lookup 'subdir' in bar - should be found in layer 2, not layer 0
    // because of the opaque marker in layer 1
    let subdir_name = CString::new("subdir").unwrap();
    let subdir_entry = fs.lookup(Context::default(), bar_entry.inode, &subdir_name)?;
    assert_eq!(subdir_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Lookup 'bottom_file' in bar/subdir - should NOT be found due to opaque marker
    let bottom_file_name = CString::new("bottom_file").unwrap();
    let bottom_file_result = fs.lookup(Context::default(), subdir_entry.inode, &bottom_file_name);
    assert!(
        bottom_file_result.is_err(),
        "bottom_file should be hidden by opaque directory"
    );

    // Lookup 'top_file' in bar/subdir - should be found in layer 2
    let top_file_name = CString::new("top_file").unwrap();
    let top_file_entry = fs.lookup(Context::default(), subdir_entry.inode, &top_file_name)?;
    assert_eq!(top_file_entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Lookup 'other' in root - should be found
    let other_name = CString::new("other").unwrap();
    let other_entry = fs.lookup(Context::default(), 1, &other_name)?;
    assert_eq!(other_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Lookup 'file' in other - should be found in layer 0
    // (other directory is not affected by the opaque marker in bar)
    let other_file_name = CString::new("file").unwrap();
    let other_file_entry = fs.lookup(Context::default(), other_entry.inode, &other_file_name)?;
    assert_eq!(other_file_entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Lookup 'extra' in root - should be found in layer 1
    let extra_name = CString::new("extra").unwrap();
    let extra_entry = fs.lookup(Context::default(), 1, &extra_name)?;
    assert_eq!(extra_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    Ok(())
}

#[test]
fn test_lookup_opaque_with_empty_subdir() -> io::Result<()> {
    // Create test layers:
    // Lower layer:
    //   - bar/
    //   - bar/hello/
    //   - bar/hello/txt
    // Upper layer:
    //   - bar/
    //   - bar/.wh..wh..opq
    //   - bar/hello/  (empty directory)
    let layers = vec![
        vec![
            ("bar", true, 0o755),
            ("bar/hello", true, 0o755),
            ("bar/hello/txt", false, 0o644),
        ],
        vec![
            ("bar", true, 0o755),
            ("bar/.wh..wh..opq", false, 0o644),
            ("bar/hello", true, 0o755),
        ],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // First lookup 'bar' directory
    let bar_name = CString::new("bar").unwrap();
    let bar_entry = fs.lookup(Context::default(), 1, &bar_name)?;
    assert_eq!(bar_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Then lookup 'hello' in bar directory
    let hello_name = CString::new("hello").unwrap();
    let hello_entry = fs.lookup(Context::default(), bar_entry.inode, &hello_name)?;
    assert_eq!(hello_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Finally lookup 'txt' in bar/hello directory
    // This should fail because the opaque marker in bar/ hides everything from lower layers
    let txt_name = CString::new("txt").unwrap();
    let txt_result = fs.lookup(Context::default(), hello_entry.inode, &txt_name);
    assert!(
        txt_result.is_err(),
        "txt should be hidden by opaque directory marker in bar/"
    );

    Ok(())
}

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
fn test_copyup_complex() -> io::Result<()> {
    // Create test layers with complex structure:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/file1 (mode 0644)
    //   - dir1/subdir/
    //   - dir1/subdir/bottom_file (mode 0644)
    //   - dir1/symlink -> file1
    //   - dir2/
    //   - dir2/file2 (mode 0600)
    // Layer 1 (middle):
    //   - dir3/
    //   - dir3/middle_file (mode 0666)
    //   - dir3/nested/
    //   - dir3/nested/data (mode 0644)
    // Layer 2 (top - initially empty):
    //   (empty - will be populated by copyup operations)
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/bottom_file", false, 0o644),
            ("dir2", true, 0o755),
            ("dir2/file2", false, 0o600),
        ],
        vec![
            ("dir3", true, 0o755),
            ("dir3/middle_file", false, 0o666),
            ("dir3/nested", true, 0o755),
            ("dir3/nested/data", false, 0o644),
        ],
        vec![], // Empty top layer
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Create symlink in bottom layer
    let symlink_path = temp_dirs[0].path().join("dir1").join("symlink");
    std::os::unix::fs::symlink("file1", &symlink_path)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test 1: Copy up a regular file from bottom layer
    // First lookup dir1/file1 to get its path_inodes
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(Context::default(), 1, &dir1_name)?;
    let file1_name = CString::new("file1").unwrap();
    let (_, path_inodes) = fs.do_lookup(dir1_entry.inode, &file1_name)?;

    // Perform copyup
    fs.do_copyup(&path_inodes)?;

    // Verify the file was copied up correctly
    let top_file1_path = temp_dirs[2].path().join("dir1").join("file1");
    let metadata = fs::metadata(&top_file1_path)?;
    assert_eq!(metadata.permissions().mode() & 0o777, 0o644);
    assert!(top_file1_path.exists());

    // Test 2: Copy up a directory with nested content
    let dir3_name = CString::new("dir3").unwrap();
    let dir3_entry = fs.lookup(Context::default(), 1, &dir3_name)?;
    let nested_name = CString::new("nested").unwrap();
    let (nested_entry, nested_path_inodes) = fs.do_lookup(dir3_entry.inode, &nested_name)?;
    println!("nested_path_inodes: {:?}", nested_path_inodes);

    // Copy up the nested directory
    fs.do_copyup(&nested_path_inodes)?;
    println!(
        "nested_path_inodes: {:?}",
        fs.get_inode_data(nested_entry.inode)
    );

    // Verify the directory structure was copied
    let top_nested_path = temp_dirs[2].path().join("dir3").join("nested");
    assert!(top_nested_path.exists());
    assert!(top_nested_path.is_dir());
    let metadata = fs::metadata(&top_nested_path)?;
    assert_eq!(metadata.permissions().mode() & 0o777, 0o755);

    // Test 3: Copy up a file from the middle layer
    let middle_file_name = CString::new("middle_file").unwrap();
    let (_, middle_file_path_inodes) = fs.do_lookup(dir3_entry.inode, &middle_file_name)?;

    // Perform copyup
    fs.do_copyup(&middle_file_path_inodes)?;

    // Verify the file was copied up correctly
    let top_middle_file_path = temp_dirs[2].path().join("dir3").join("middle_file");
    let metadata = fs::metadata(&top_middle_file_path)?;
    assert_eq!(metadata.permissions().mode() & 0o777, 0o666);
    assert!(top_middle_file_path.exists());

    // Test 4: Copy up a nested file
    let data_name = CString::new("data").unwrap();
    let (_, data_path_inodes) = fs.do_lookup(nested_entry.inode, &data_name)?;

    // Perform copyup
    fs.do_copyup(&data_path_inodes)?;

    // Verify the nested file was copied up correctly
    let top_data_path = temp_dirs[2].path().join("dir3").join("nested").join("data");
    let metadata = fs::metadata(&top_data_path)?;
    assert_eq!(metadata.permissions().mode() & 0o777, 0o644);
    assert!(top_data_path.exists());

    // Test 5: Verify parent directories are created as needed
    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(Context::default(), 1, &dir2_name)?;
    let file2_name = CString::new("file2").unwrap();
    let (_, file2_path_inodes) = fs.do_lookup(dir2_entry.inode, &file2_name)?;

    // Perform copyup
    fs.do_copyup(&file2_path_inodes)?;

    // Verify the directory structure
    let top_dir2_path = temp_dirs[2].path().join("dir2");
    assert!(top_dir2_path.exists());
    assert!(top_dir2_path.is_dir());
    let top_file2_path = top_dir2_path.join("file2");
    let metadata = fs::metadata(&top_file2_path)?;
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert!(top_file2_path.exists());

    // Test 6: Copy up a symbolic link
    let symlink_name = CString::new("symlink").unwrap();
    let (_, symlink_path_inodes) = fs.do_lookup(dir1_entry.inode, &symlink_name)?;

    // Perform copyup
    fs.do_copyup(&symlink_path_inodes)?;

    // Verify the symlink was copied up correctly
    let top_symlink_path = temp_dirs[2].path().join("dir1").join("symlink");
    assert!(top_symlink_path.exists());
    assert!(fs::symlink_metadata(&top_symlink_path)?
        .file_type()
        .is_symlink());

    // Read the symlink target
    let target = fs::read_link(&top_symlink_path)?;
    assert_eq!(target.to_str().unwrap(), "file1");

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
fn test_setattr_copyup() -> io::Result<()> {
    // Create test layers:
    // Lower layer: file1 (mode 0644)
    // Upper layer: empty (file1 will be copied up)
    let layers = vec![vec![("file1", false, 0o644)], vec![]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, true)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test setattr on file in lower layer (should trigger copyup)
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

    // Test 1: Modify file in bottom layer (should trigger copyup)
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

    // Test 2: Modify file in middle layer (should trigger copyup)
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

    // Test 3: Modify file in top layer (no copyup needed)
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
fn test_readlink_basic() -> io::Result<()> {
    // Create test layers:
    // Lower layer: target_file, link -> target_file
    let layers = vec![vec![
        ("target_file", false, 0o644),
        // Note: symlinks will be created separately below
    ]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Create symlink in bottom layer
    let symlink_path = temp_dirs[0].path().join("link");
    std::os::unix::fs::symlink("target_file", &symlink_path)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test readlink
    let link_name = CString::new("link").unwrap();
    let link_entry = fs.lookup(Context::default(), 1, &link_name)?;
    let target = fs.readlink(Context::default(), link_entry.inode)?;

    assert_eq!(target, b"target_file");

    Ok(())
}

#[test]
fn test_readlink_multiple_layers() -> io::Result<()> {
    // Create test layers:
    // Lower layer: target1, link1 -> target1
    // Middle layer: target2, link2 -> target2
    // Upper layer: target3, link3 -> target3
    let layers = vec![
        vec![("target1", false, 0o644)],
        vec![("target2", false, 0o644)],
        vec![("target3", false, 0o644)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;
    // Create symlinks in each layer
    std::os::unix::fs::symlink("target1", temp_dirs[0].path().join("link1"))?;
    std::os::unix::fs::symlink("target2", temp_dirs[1].path().join("link2"))?;
    std::os::unix::fs::symlink("target3", temp_dirs[2].path().join("link3"))?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test readlink for symlink in bottom layer
    let link1_name = CString::new("link1").unwrap();
    let link1_entry = fs.lookup(Context::default(), 1, &link1_name)?;
    let target1 = fs.readlink(Context::default(), link1_entry.inode)?;
    assert_eq!(target1, b"target1");

    // Test readlink for symlink in middle layer
    let link2_name = CString::new("link2").unwrap();
    let link2_entry = fs.lookup(Context::default(), 1, &link2_name)?;
    let target2 = fs.readlink(Context::default(), link2_entry.inode)?;
    assert_eq!(target2, b"target2");

    // Test readlink for symlink in top layer
    let link3_name = CString::new("link3").unwrap();
    let link3_entry = fs.lookup(Context::default(), 1, &link3_name)?;
    let target3 = fs.readlink(Context::default(), link3_entry.inode)?;
    assert_eq!(target3, b"target3");

    Ok(())
}

#[test]
fn test_readlink_shadowed() -> io::Result<()> {
    // Create test layers:
    // Lower layer: target1, link -> target1
    // Upper layer: link -> target2 (shadows lower layer's link)
    let layers = vec![
        vec![("target1", false, 0o644)],
        vec![("target2", false, 0o644)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Create symlinks
    std::os::unix::fs::symlink("target1", temp_dirs[0].path().join("link"))?;
    std::os::unix::fs::symlink("target2", temp_dirs[1].path().join("link"))?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test readlink - should get the symlink from upper layer
    let link_name = CString::new("link").unwrap();
    let link_entry = fs.lookup(Context::default(), 1, &link_name)?;
    let target = fs.readlink(Context::default(), link_entry.inode)?;

    assert_eq!(target, b"target2", "Should read symlink from upper layer");

    Ok(())
}

#[test]
fn test_readlink_nested() -> io::Result<()> {
    // Create test layers with nested directory structure:
    // Lower layer:
    //   - dir1/target1
    //   - dir1/link1 -> target1
    //   - dir2/target2
    //   - dir2/subdir/link2 -> ../target2
    let layers = vec![vec![
        ("dir1", true, 0o755),
        ("dir1/target1", false, 0o644),
        ("dir2", true, 0o755),
        ("dir2/target2", false, 0o644),
        ("dir2/subdir", true, 0o755),
    ]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;
    // Create symlinks
    std::os::unix::fs::symlink("target1", temp_dirs[0].path().join("dir1/link1"))?;
    std::os::unix::fs::symlink("../target2", temp_dirs[0].path().join("dir2/subdir/link2"))?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test readlink for simple symlink in directory
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(Context::default(), 1, &dir1_name)?;
    let link1_name = CString::new("link1").unwrap();
    let link1_entry = fs.lookup(Context::default(), dir1_entry.inode, &link1_name)?;
    let target1 = fs.readlink(Context::default(), link1_entry.inode)?;
    assert_eq!(target1, b"target1");

    // Test readlink for symlink with relative path
    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(Context::default(), 1, &dir2_name)?;
    let subdir_name = CString::new("subdir").unwrap();
    let subdir_entry = fs.lookup(Context::default(), dir2_entry.inode, &subdir_name)?;
    let link2_name = CString::new("link2").unwrap();
    let link2_entry = fs.lookup(Context::default(), subdir_entry.inode, &link2_name)?;
    let target2 = fs.readlink(Context::default(), link2_entry.inode)?;
    assert_eq!(target2, b"../target2");

    Ok(())
}

#[test]
fn test_readlink_errors() -> io::Result<()> {
    // Create test layers:
    // Lower layer: regular_file, directory
    let layers = vec![vec![
        ("regular_file", false, 0o644),
        ("directory", true, 0o755),
    ]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Test readlink on regular file (should fail)
    let file_name = CString::new("regular_file").unwrap();
    let file_entry = fs.lookup(Context::default(), 1, &file_name)?;
    let result = fs.readlink(Context::default(), file_entry.inode);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().raw_os_error(),
        Some(libc::EINVAL),
        "Reading link of regular file should return EINVAL"
    );

    // Test readlink on directory (should fail)
    let dir_name = CString::new("directory").unwrap();
    let dir_entry = fs.lookup(Context::default(), 1, &dir_name)?;
    let result = fs.readlink(Context::default(), dir_entry.inode);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().raw_os_error(),
        Some(libc::EINVAL),
        "Reading link of directory should return EINVAL"
    );

    // Test readlink with invalid inode
    let result = fs.readlink(Context::default(), 999999);
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().raw_os_error(),
        Some(libc::EBADF),
        "Reading link with invalid inode should return EBADF"
    );

    Ok(())
}

#[test]
fn test_readlink_whiteout() -> io::Result<()> {
    // Create test layers:
    // Lower layer: target1, link1 -> target1
    // Upper layer: .wh.link1 (whiteout for link1)
    let layers = vec![
        vec![("target1", false, 0o644)],
        vec![(".wh.link1", false, 0o644)], // Whiteout file
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Create symlink in bottom layer
    std::os::unix::fs::symlink("target1", temp_dirs[0].path().join("link1"))?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Try to lookup whited-out symlink (should fail)
    let link_name = CString::new("link1").unwrap();
    match fs.lookup(Context::default(), 1, &link_name) {
        Ok(_) => panic!("Expected lookup of whited-out symlink to fail"),
        Err(e) => {
            assert_eq!(
                e.raw_os_error(),
                Some(libc::ENOENT),
                "Looking up whited-out symlink should return ENOENT"
            );
        }
    }

    Ok(())
}

#[test]
fn test_mkdir_basic() -> io::Result<()> {
    // Create test layers:
    // Single layer with a file
    let layers = vec![vec![("file1", false, 0o644)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Create a new directory
    let dir_name = CString::new("new_dir").unwrap();
    let ctx = Context::default();
    let entry = fs.mkdir(ctx, 1, &dir_name, 0o755, 0, Extensions::default())?;

    // Verify the directory was created with correct mode
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);
    assert_eq!(entry.attr.st_mode & 0o777, 0o755);

    // Verify we can look it up
    let lookup_entry = fs.lookup(ctx, 1, &dir_name)?;
    assert_eq!(lookup_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Verify the directory exists on disk in the top layer
    let dir_path = temp_dirs.last().unwrap().path().join("new_dir");
    assert!(dir_path.exists());
    assert!(dir_path.is_dir());

    Ok(())
}

#[test]
fn test_mkdir_nested() -> io::Result<()> {
    // Create test layers with complex structure:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/file1
    //   - dir1/subdir/
    //   - dir1/subdir/bottom_file
    // Layer 1 (middle):
    //   - dir2/
    //   - dir2/file2
    // Layer 2 (top):
    //   - dir3/
    //   - dir3/top_file
    //   - dir1/.wh.subdir (whiteout)
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/bottom_file", false, 0o644),
        ],
        vec![("dir2", true, 0o755), ("dir2/file2", false, 0o644)],
        vec![
            ("dir3", true, 0o755),
            ("dir3/top_file", false, 0o644),
            ("dir1/.wh.subdir", false, 0o644),
        ],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Test 1: Create nested directory in dir1 (should trigger copy-up)
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;
    let nested_name = CString::new("new_nested").unwrap();
    let nested_entry = fs.mkdir(
        ctx,
        dir1_entry.inode,
        &nested_name,
        0o700,
        0,
        Extensions::default(),
    )?;
    assert_eq!(nested_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Test 2: Create directory inside the newly created nested directory
    let deep_name = CString::new("deep_dir").unwrap();
    let deep_entry = fs.mkdir(
        ctx,
        nested_entry.inode,
        &deep_name,
        0o755,
        0,
        Extensions::default(),
    )?;
    assert_eq!(deep_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    // Test 3: Create directory in dir2 (middle layer, should trigger copy-up)
    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(ctx, 1, &dir2_name)?;
    let middle_nested_name = CString::new("middle_nested").unwrap();
    let middle_nested_entry = fs.mkdir(
        ctx,
        dir2_entry.inode,
        &middle_nested_name,
        0o755,
        0,
        Extensions::default(),
    )?;
    assert_eq!(
        middle_nested_entry.attr.st_mode & libc::S_IFMT,
        libc::S_IFDIR
    );

    // Test 4: Create directory in dir3 (top layer, no copy-up needed)
    let dir3_name = CString::new("dir3").unwrap();
    let dir3_entry = fs.lookup(ctx, 1, &dir3_name)?;
    let top_nested_name = CString::new("top_nested").unwrap();
    let top_nested_entry = fs.mkdir(
        ctx,
        dir3_entry.inode,
        &top_nested_name,
        0o755,
        0,
        Extensions::default(),
    )?;
    assert_eq!(top_nested_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    helper::debug_print_layers(&temp_dirs, false)?;

    // Verify all directories exist in appropriate layers
    let top_layer = temp_dirs.last().unwrap().path();
    assert!(top_layer.join("dir1/new_nested").exists());
    assert!(top_layer.join("dir1/new_nested/deep_dir").exists());
    assert!(top_layer.join("dir2/middle_nested").exists());
    assert!(top_layer.join("dir3/top_nested").exists());

    // Verify the original files are still accessible
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;
    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(ctx, dir1_entry.inode, &file1_name)?;
    assert_eq!(file1_entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    Ok(())
}

#[test]
fn test_mkdir_with_umask() -> io::Result<()> {
    // Create test layers with complex structure:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/subdir/ (0o755)
    //   - dir1/subdir/file1
    // Layer 1 (middle):
    //   - dir2/
    //   - dir2/file2
    // Layer 2 (top):
    //   - dir3/ (0o777)
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/file1", false, 0o644),
        ],
        vec![("dir2", true, 0o755), ("dir2/file2", false, 0o644)],
        vec![("dir3", true, 0o777)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Test 1: Create directory with different umasks in root
    let dir_names = vec![
        ("dir_umask_022", 0o777, 0o022, 0o755), // Common umask
        ("dir_umask_077", 0o777, 0o077, 0o700), // Strict umask
        ("dir_umask_002", 0o777, 0o002, 0o775), // Group writable
        ("dir_umask_000", 0o777, 0o000, 0o777), // No umask
    ];

    let test_cases = dir_names.clone();
    for (name, mode, umask, expected) in test_cases {
        let dir_name = CString::new(name).unwrap();
        let entry = fs.mkdir(ctx, 1, &dir_name, mode, umask, Extensions::default())?;
        assert_eq!(
            entry.attr.st_mode & 0o777,
            expected,
            "Directory {} has wrong permissions",
            name
        );
    }

    // Test 2: Create nested directories with umask in different layers
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;
    let nested_name = CString::new("nested_umask").unwrap();
    let nested_entry = fs.mkdir(
        ctx,
        dir1_entry.inode,
        &nested_name,
        0o777,
        0o027,
        Extensions::default(),
    )?;
    assert_eq!(nested_entry.attr.st_mode & 0o777, 0o750);

    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(ctx, 1, &dir2_name)?;
    let middle_name = CString::new("middle_umask").unwrap();
    let middle_entry = fs.mkdir(
        ctx,
        dir2_entry.inode,
        &middle_name,
        0o777,
        0o077,
        Extensions::default(),
    )?;
    assert_eq!(middle_entry.attr.st_mode & 0o777, 0o700);

    Ok(())
}

#[test]
fn test_mkdir_existing_name() -> io::Result<()> {
    // Create test layers with complex structure:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/file1
    //   - dir1/subdir/
    //   - dir1/subdir/file2
    // Layer 1 (middle):
    //   - dir2/
    //   - dir2/file3
    //   - dir1/another_file
    // Layer 2 (top):
    //   - dir3/
    //   - dir3/file4
    //   - .wh.dir1/subdir (whiteout)
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/file2", false, 0o644),
        ],
        vec![
            ("dir2", true, 0o755),
            ("dir2/file3", false, 0o644),
            ("dir1/another_file", false, 0o644),
        ],
        vec![
            ("dir3", true, 0o755),
            ("dir3/file4", false, 0o644),
            ("dir1/.wh.subdir", false, 0o644),
        ],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Test 1: Try to create directory with name of existing file in bottom layer
    let file1_name = CString::new("file1").unwrap();
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;
    match fs.mkdir(
        ctx,
        dir1_entry.inode,
        &file1_name,
        0o755,
        0,
        Extensions::default(),
    ) {
        Ok(_) => {
            helper::debug_print_layers(&temp_dirs, false)?;
            panic!("Expected mkdir with existing file name to fail");
        }
        Err(e) => assert_eq!(e.kind(), io::ErrorKind::AlreadyExists),
    }

    // Test 2: Try to create directory with name of existing file in middle layer
    let file3_name = CString::new("file3").unwrap();
    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(ctx, 1, &dir2_name)?;
    match fs.mkdir(
        ctx,
        dir2_entry.inode,
        &file3_name,
        0o755,
        0,
        Extensions::default(),
    ) {
        Ok(_) => panic!("Expected mkdir with existing file name to fail"),
        Err(e) => assert_eq!(e.kind(), io::ErrorKind::AlreadyExists),
    }

    // Test 3: Try to create directory with name of existing directory
    let dir3_name = CString::new("dir3").unwrap();
    match fs.mkdir(ctx, 1, &dir3_name, 0o755, 0, Extensions::default()) {
        Ok(_) => panic!("Expected mkdir with existing directory name to fail"),
        Err(e) => assert_eq!(e.kind(), io::ErrorKind::AlreadyExists),
    }

    // Test 4: Try to create directory with name that exists in lower layer but is whited out
    let subdir_name = CString::new("subdir").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;

    // This should succeed because the original subdir is whited out
    let new_subdir = fs.mkdir(
        ctx,
        dir1_entry.inode,
        &subdir_name,
        0o755,
        0,
        Extensions::default(),
    )?;
    assert_eq!(new_subdir.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

    Ok(())
}

#[test]
fn test_mkdir_invalid_parent() -> io::Result<()> {
    // Create test layers with complex structure:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/file1
    //   - dir1/subdir/
    // Layer 1 (middle):
    //   - dir2/
    //   - dir2/file2
    //   - .wh.dir1 (whiteout entire dir1)
    // Layer 2 (top):
    //   - dir3/
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
        ],
        vec![
            ("dir2", true, 0o755),
            ("dir2/file2", false, 0o644),
            (".wh.dir1", false, 0o644), // Whiteout entire dir1
        ],
        vec![("dir3", true, 0o755)],
    ];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&_temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Test 1: Try to create directory with non-existent parent inode
    let dir_name = CString::new("new_dir").unwrap();
    let invalid_inode = 999999;
    match fs.mkdir(
        ctx,
        invalid_inode,
        &dir_name,
        0o755,
        0,
        Extensions::default(),
    ) {
        Ok(_) => panic!("Expected mkdir with invalid parent to fail"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::EBADF)),
    }

    // Test 2: Try to create directory in whited-out directory
    let dir1_name = CString::new("dir1").unwrap();
    match fs.lookup(ctx, 1, &dir1_name) {
        Ok(_) => panic!("Expected lookup of whited-out directory to fail"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
    }

    // Test 3: Try to create directory with file as parent
    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(ctx, 1, &dir2_name)?;
    let file2_name = CString::new("file2").unwrap();
    let file2_entry = fs.lookup(ctx, dir2_entry.inode, &file2_name)?;

    let nested_name = CString::new("nested").unwrap();
    match fs.mkdir(
        ctx,
        file2_entry.inode,
        &nested_name,
        0o755,
        0,
        Extensions::default(),
    ) {
        Ok(_) => panic!("Expected mkdir with file as parent to fail"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOTDIR)),
    }

    Ok(())
}

#[test]
fn test_mkdir_invalid_name() -> io::Result<()> {
    // Create test layers with complex structure:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/.hidden_file
    //   - dir1/subdir/
    // Layer 1 (middle):
    //   - dir2/
    //   - dir2/.wh..wh..opq (opaque directory)
    // Layer 2 (top):
    //   - dir3/
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/.hidden_file", false, 0o644),
            ("dir1/subdir", true, 0o755),
        ],
        vec![
            ("dir2", true, 0o755),
            ("dir2/.wh..wh..opq", false, 0o644), // Opaque directory marker
        ],
        vec![("dir3", true, 0o755)],
    ];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Test various invalid names
    let test_cases = vec![
        ("", io::ErrorKind::InvalidInput, "empty name"),
        (
            "..",
            io::ErrorKind::PermissionDenied,
            "parent dir traversal",
        ),
        ("foo/bar", io::ErrorKind::PermissionDenied, "contains slash"),
        (
            "foo\\bar",
            io::ErrorKind::PermissionDenied,
            "contains backslash",
        ),
        (
            "foo\0bar",
            io::ErrorKind::InvalidInput,
            "contains null byte",
        ),
        (".wh.foo", io::ErrorKind::InvalidInput, "whiteout prefix"),
        (".wh..wh..opq", io::ErrorKind::InvalidInput, "opaque marker"),
    ];

    for (name, expected_kind, desc) in test_cases {
        let name = CString::new(name.as_bytes().to_vec()).unwrap_or_default();
        match fs.mkdir(ctx, 1, &name, 0o755, 0, Extensions::default()) {
            Ok(_) => panic!("Expected mkdir with {} to fail", desc),
            Err(e) => assert_eq!(
                e.kind(),
                expected_kind,
                "Wrong error kind for {}: expected {:?}, got {:?}",
                desc,
                expected_kind,
                e.kind()
            ),
        }
    }

    // Test invalid UTF-8 separately since it can't be represented as a string literal
    let invalid_utf8 = vec![0x66, 0x6f, 0x6f, 0x80, 0x62, 0x61, 0x72]; // "foo<invalid>bar"
    let name = CString::new(invalid_utf8).unwrap();
    match fs.mkdir(ctx, 1, &name, 0o755, 0, Extensions::default()) {
        Ok(_) => panic!("Expected mkdir with invalid UTF-8 to fail"),
        Err(e) => assert_eq!(
            e.kind(),
            io::ErrorKind::InvalidInput,
            "Wrong error kind for invalid UTF-8: expected {:?}, got {:?}",
            io::ErrorKind::InvalidInput,
            e.kind()
        ),
    }

    // Test with valid but unusual names
    let valid_cases = vec![
        "very_long_name_that_is_valid_but_unusual_and_tests_length_limits",
        " leading_space",
        "trailing_space ",
        "!@#$%^&*()_+-=",
    ];

    for name in valid_cases {
        let name = CString::new(name).unwrap();
        // These should succeed
        let entry = fs.mkdir(ctx, 1, &name, 0o755, 0, Extensions::default())?;
        assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);
    }

    Ok(())
}

#[test]
fn test_mkdir_multiple_layers() -> io::Result<()> {
    // Create test layers with complex structure:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/file1
    //   - dir1/subdir/
    //   - dir1/subdir/bottom_file
    // Layer 1 (middle):
    //   - dir2/
    //   - dir2/file2
    // Layer 2 (top):
    //   - dir3/
    //   - dir3/top_file
    //   - .wh.dir1 (whiteout)
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/bottom_file", false, 0o644),
        ],
        vec![("dir2", true, 0o755), ("dir2/file2", false, 0o644)],
        vec![
            ("dir3", true, 0o755),
            ("dir3/top_file", false, 0o644),
            (".wh.dir1", false, 0o644),
        ],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Test 1: Create directory in each layer and verify copy-up behavior
    let dir_names = vec![("dir2", "new_dir2"), ("dir3", "new_dir3")];

    for (parent, new_dir) in dir_names {
        let parent_name = CString::new(parent).unwrap();
        let parent_entry = fs.lookup(ctx, 1, &parent_name)?;

        let new_name = CString::new(new_dir).unwrap();
        let entry = fs.mkdir(
            ctx,
            parent_entry.inode,
            &new_name,
            0o755,
            0,
            Extensions::default(),
        )?;
        assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);

        // Create a nested directory inside
        let nested_name = CString::new(format!("nested_in_{}", new_dir)).unwrap();
        let nested_entry = fs.mkdir(
            ctx,
            entry.inode,
            &nested_name,
            0o700,
            0,
            Extensions::default(),
        )?;
        assert_eq!(nested_entry.attr.st_mode & libc::S_IFMT, libc::S_IFDIR);
    }

    // Test 2: Verify all directories exist in the top layer
    let top_layer = temp_dirs.last().unwrap().path();
    assert!(top_layer.join("dir2/new_dir2").exists());
    assert!(top_layer.join("dir2/new_dir2/nested_in_new_dir2").exists());
    assert!(top_layer.join("dir3/new_dir3").exists());
    assert!(top_layer.join("dir3/new_dir3/nested_in_new_dir3").exists());

    // Test 3: Try to create directory in whited-out dir1 (should fail)
    let dir1_name = CString::new("dir1").unwrap();
    match fs.lookup(ctx, 1, &dir1_name) {
        Ok(_) => panic!("Expected lookup of whited-out directory to fail"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
    }

    Ok(())
}

#[test]
fn test_unlink_basic() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a file
    let (fs, temp_dirs) = helper::create_overlayfs(vec![vec![("file1.txt", false, 0o644)]])?;
    let ctx = Context::default();

    // Lookup the file to get its parent inode (root) and verify it exists
    let file_name = CString::new("file1.txt").unwrap();
    let _ = fs.lookup(ctx, 1, &file_name)?;

    // Unlink the file
    fs.unlink(ctx, 1, &file_name)?;

    // Verify the file is gone
    match fs.lookup(ctx, 1, &file_name) {
        Ok(_) => panic!("File still exists after unlink"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
    }

    // Verify the file is physically removed from the filesystem
    assert!(!temp_dirs[0].path().join("file1.txt").exists());

    Ok(())
}

#[test]
fn test_unlink_whiteout() -> io::Result<()> {
    // Create an overlayfs with two layers:
    // - Lower layer: contains file1.txt
    // - Upper layer: empty
    let (fs, temp_dirs) = helper::create_overlayfs(vec![
        vec![("file1.txt", false, 0o644)], // lower layer
        vec![],                            // upper layer
    ])?;
    let ctx = Context::default();

    // Lookup the file to verify it exists
    let file_name = CString::new("file1.txt").unwrap();
    let _ = fs.lookup(ctx, 1, &file_name)?;

    // Unlink the file - this should create a whiteout in the upper layer
    fs.unlink(ctx, 1, &file_name)?;

    // Verify the file appears to be gone through the overlayfs
    match fs.lookup(ctx, 1, &file_name) {
        Ok(_) => panic!("File still exists after unlink"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
    }

    // Verify the original file still exists in the lower layer
    assert!(temp_dirs[0].path().join("file1.txt").exists());

    // Verify a whiteout was created in the upper layer
    assert!(temp_dirs[1].path().join(".wh.file1.txt").exists());

    Ok(())
}

#[test]
fn test_unlink_multiple_layers() -> io::Result<()> {
    // Create an overlayfs with three layers, each containing different files
    let (fs, temp_dirs) = helper::create_overlayfs(vec![
        vec![("lower.txt", false, 0o644)],  // lowest layer
        vec![("middle.txt", false, 0o644)], // middle layer
        vec![("upper.txt", false, 0o644)],  // upper layer
    ])?;
    let ctx = Context::default();

    // Test unlinking a file from each layer
    for file in &["lower.txt", "middle.txt", "upper.txt"] {
        let file_name = CString::new(*file).unwrap();

        // Verify file exists before unlink
        fs.lookup(ctx, 1, &file_name)?;

        // Unlink the file
        fs.unlink(ctx, 1, &file_name)?;

        // Verify file appears gone through overlayfs
        match fs.lookup(ctx, 1, &file_name) {
            Ok(_) => panic!("File {} still exists after unlink", file),
            Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
        }
    }

    // Verify physical state of layers:
    // - Files in lower layers should still exist
    // - File in top layer should be gone
    // - Whiteouts should exist in top layer for lower files
    assert!(temp_dirs[0].path().join("lower.txt").exists());
    assert!(temp_dirs[1].path().join("middle.txt").exists());
    assert!(!temp_dirs[2].path().join("upper.txt").exists());
    assert!(temp_dirs[2].path().join(".wh.lower.txt").exists());
    assert!(temp_dirs[2].path().join(".wh.middle.txt").exists());

    Ok(())
}

#[test]
fn test_unlink_nested_files() -> io::Result<()> {
    // Create an overlayfs with nested directory structure
    let (fs, temp_dirs) = helper::create_overlayfs(vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1.txt", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/file2.txt", false, 0o644),
        ],
        vec![], // empty upper layer
    ])?;
    helper::debug_print_layers(&temp_dirs, false)?;
    let ctx = Context::default();

    // Lookup and unlink nested files
    let dir1_name = CString::new("dir1").unwrap();
    let subdir_name = CString::new("subdir").unwrap();
    let file1_name = CString::new("file1.txt").unwrap();
    let file2_name = CString::new("file2.txt").unwrap();

    // Get directory inodes
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;
    let subdir_entry = fs.lookup(ctx, dir1_entry.inode, &subdir_name)?;

    // Unlink file2.txt from subdir
    fs.unlink(ctx, subdir_entry.inode, &file2_name)?;

    // Verify file2.txt is gone but file1.txt still exists
    match fs.lookup(ctx, subdir_entry.inode, &file2_name) {
        Ok(_) => panic!("file2.txt still exists after unlink"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
    }
    fs.lookup(ctx, dir1_entry.inode, &file1_name)?; // should succeed

    helper::debug_print_layers(&temp_dirs, false)?;

    // Verify whiteout was created in correct location
    assert!(temp_dirs[1]
        .path()
        .join("dir1/subdir/.wh.file2.txt")
        .exists());

    Ok(())
}

#[test]
fn test_unlink_errors() -> io::Result<()> {
    // Create a basic overlayfs
    let (fs, _) = helper::create_overlayfs(vec![vec![("file1.txt", false, 0o644)]])?;
    let ctx = Context::default();

    // Test: Try to unlink non-existent file
    let nonexistent = CString::new("nonexistent.txt").unwrap();
    match fs.unlink(ctx, 1, &nonexistent) {
        Ok(_) => panic!("Unlink succeeded on non-existent file"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
    }

    // Test: Try to unlink with invalid parent inode
    let file_name = CString::new("file1.txt").unwrap();
    match fs.unlink(ctx, 999999, &file_name) {
        Ok(_) => panic!("Unlink succeeded with invalid parent inode"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::EBADF)),
    }

    // Test: Try to unlink with invalid name (containing path traversal)
    let invalid_name = CString::new("../file1.txt").unwrap();
    match fs.unlink(ctx, 1, &invalid_name) {
        Ok(_) => panic!("Unlink succeeded with invalid name"),
        Err(e) => {
            assert_eq!(
                e.kind(),
                io::ErrorKind::PermissionDenied,
                "Expected PermissionDenied error, got {:?}",
                e.kind()
            );
        }
    }

    Ok(())
}

#[test]
fn test_unlink_complex_layers() -> io::Result<()> {
    // Create an overlayfs with complex layer structure:
    // - Lower layer: base files
    // - Middle layer: some files deleted, some added
    // - Upper layer: more modifications
    let (fs, temp_dirs) = helper::create_overlayfs(vec![
        vec![
            // lower layer
            ("dir1", true, 0o755),
            ("dir1/file1.txt", false, 0o644),
            ("dir1/file2.txt", false, 0o644),
            ("dir2", true, 0o755),
            ("dir2/file3.txt", false, 0o644),
        ],
        vec![
            // middle layer
            ("dir1/new_file.txt", false, 0o644),
            ("dir2/file4.txt", false, 0o644),
            // Whiteout in middle layer for file3.txt in dir2 - placed in dir2 directory
            ("dir2/.wh.file3.txt", false, 0o000),
        ],
        vec![
            // upper layer
            ("dir3", true, 0o755),
            ("dir3/file5.txt", false, 0o644),
        ],
    ])?;
    helper::debug_print_layers(&temp_dirs, false)?;
    let ctx = Context::default();

    // Test 1: Unlink a file that exists in the top layer
    let dir3_name = CString::new("dir3").unwrap();
    let file5_name = CString::new("file5.txt").unwrap();
    let dir3_entry = fs.lookup(ctx, 1, &dir3_name)?;
    fs.unlink(ctx, dir3_entry.inode, &file5_name)?;
    assert!(!temp_dirs[2].path().join("dir3/file5.txt").exists());

    // Test 2: Unlink a file from middle layer
    let dir1_name = CString::new("dir1").unwrap();
    let new_file_name = CString::new("new_file.txt").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;
    fs.unlink(ctx, dir1_entry.inode, &new_file_name)?;
    // Expect a whiteout created in the top layer for new_file.txt
    assert!(temp_dirs[2].path().join("dir1/.wh.new_file.txt").exists());

    // Test 3: Unlink a file from lowest layer
    let file1_name = CString::new("file1.txt").unwrap();
    fs.unlink(ctx, dir1_entry.inode, &file1_name)?;
    // // Expect a whiteout in the top layer but the original file remains in lower layer
    // assert!(temp_dirs[2].path().join("dir1/.wh.file1.txt").exists());
    // assert!(temp_dirs[0].path().join("dir1/file1.txt").exists());

    // // Test 4: Unlink a file from lowest layer that is already whiteouted
    // let file2_name = CString::new("file2.txt").unwrap();
    // // First unlink to create the whiteout
    // fs.unlink(ctx, dir1_entry.inode, &file2_name)?;
    // assert!(temp_dirs[2].path().join("dir1/.wh.file2.txt").exists());
    // // Second attempt should fail with ENOENT
    // match fs.unlink(ctx, dir1_entry.inode, &file2_name) {
    //     Ok(_) => panic!("Unlink succeeded on already whiteouted file"),
    //     Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
    // }

    Ok(())
}

#[test]
fn test_rmdir_basic() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing an empty directory
    let (fs, temp_dirs) = helper::create_overlayfs(vec![vec![("empty_dir", true, 0o755)]])?;
    let ctx = Context::default();

    // Lookup the directory to verify it exists
    let dir_name = CString::new("empty_dir").unwrap();
    let _ = fs.lookup(ctx, 1, &dir_name)?;

    // Remove the directory
    fs.rmdir(ctx, 1, &dir_name)?;

    // Verify the directory is gone
    match fs.lookup(ctx, 1, &dir_name) {
        Ok(_) => panic!("Directory still exists after rmdir"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
    }

    // Verify the directory is physically removed from the filesystem
    assert!(!temp_dirs[0].path().join("empty_dir").exists());

    Ok(())
}

#[test]
fn test_rmdir_whiteout() -> io::Result<()> {
    // Create an overlayfs with two layers:
    // - Lower layer: contains empty_dir
    // - Upper layer: empty
    let (fs, temp_dirs) = helper::create_overlayfs(vec![
        vec![("empty_dir", true, 0o755)], // lower layer
        vec![],                           // upper layer
    ])?;
    let ctx = Context::default();

    // Lookup the directory to verify it exists
    let dir_name = CString::new("empty_dir").unwrap();
    let _ = fs.lookup(ctx, 1, &dir_name)?;

    // Remove the directory - this should create a whiteout in the upper layer
    fs.rmdir(ctx, 1, &dir_name)?;

    // Verify the directory appears to be gone through the overlayfs
    match fs.lookup(ctx, 1, &dir_name) {
        Ok(_) => panic!("Directory still exists after rmdir"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
    }

    // Verify the original directory still exists in the lower layer
    assert!(temp_dirs[0].path().join("empty_dir").exists());

    // Verify a whiteout was created in the upper layer
    assert!(temp_dirs[1].path().join(".wh.empty_dir").exists());

    Ok(())
}

#[test]
fn test_rmdir_multiple_layers() -> io::Result<()> {
    // Create an overlayfs with three layers, each containing different directories
    let (fs, temp_dirs) = helper::create_overlayfs(vec![
        vec![("lower_dir", true, 0o755)],  // lowest layer
        vec![("middle_dir", true, 0o755)], // middle layer
        vec![("upper_dir", true, 0o755)],  // upper layer
    ])?;
    let ctx = Context::default();

    // Test removing a directory from each layer
    for dir in &["lower_dir", "middle_dir", "upper_dir"] {
        let dir_name = CString::new(*dir).unwrap();

        // Verify directory exists before removal
        fs.lookup(ctx, 1, &dir_name)?;

        // Remove the directory
        fs.rmdir(ctx, 1, &dir_name)?;

        // Verify directory appears gone through overlayfs
        match fs.lookup(ctx, 1, &dir_name) {
            Ok(_) => panic!("Directory {} still exists after rmdir", dir),
            Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
        }
    }

    // Verify physical state of layers:
    // - Directories in lower layers should still exist
    // - Directory in top layer should be gone
    // - Whiteouts should exist in top layer for lower directories
    assert!(temp_dirs[0].path().join("lower_dir").exists());
    assert!(temp_dirs[1].path().join("middle_dir").exists());
    assert!(!temp_dirs[2].path().join("upper_dir").exists());
    assert!(temp_dirs[2].path().join(".wh.lower_dir").exists());
    assert!(temp_dirs[2].path().join(".wh.middle_dir").exists());

    Ok(())
}

#[test]
fn test_rmdir_nested_dirs() -> io::Result<()> {
    // Create an overlayfs with nested directory structure
    let (fs, temp_dirs) = helper::create_overlayfs(vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/subdir1", true, 0o755),
            ("dir1/subdir2", true, 0o755),
            ("dir1/subdir2/nested", true, 0o755),
        ],
        vec![], // empty upper layer
    ])?;
    helper::debug_print_layers(&temp_dirs, false)?;
    let ctx = Context::default();

    // Lookup and remove nested directories
    let dir1_name = CString::new("dir1").unwrap();
    let subdir2_name = CString::new("subdir2").unwrap();
    let nested_name = CString::new("nested").unwrap();

    // Get directory inodes
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;
    let subdir2_entry = fs.lookup(ctx, dir1_entry.inode, &subdir2_name)?;

    // Remove nested directory
    fs.rmdir(ctx, subdir2_entry.inode, &nested_name)?;

    // Verify nested is gone but subdir1 still exists
    match fs.lookup(ctx, subdir2_entry.inode, &nested_name) {
        Ok(_) => panic!("nested directory still exists after rmdir"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
    }

    let subdir1_name = CString::new("subdir1").unwrap();
    fs.lookup(ctx, dir1_entry.inode, &subdir1_name)?; // should succeed

    // Verify whiteout was created in correct location
    assert!(temp_dirs[1].path().join("dir1/subdir2/.wh.nested").exists());

    Ok(())
}

#[test]
fn test_rmdir_errors() -> io::Result<()> {
    // Create an overlayfs with a directory containing a file
    let (fs, _temp_dirs) = helper::create_overlayfs(vec![vec![
        ("dir1", true, 0o755),
        ("dir1/file1.txt", false, 0o644),
    ]])?;
    let ctx = Context::default();

    // Test: Try to remove non-existent directory
    let nonexistent = CString::new("nonexistent").unwrap();
    match fs.rmdir(ctx, 1, &nonexistent) {
        Ok(_) => panic!("rmdir succeeded on non-existent directory"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOENT)),
    }

    // Test: Try to remove with invalid parent inode
    let dir_name = CString::new("dir1").unwrap();
    match fs.rmdir(ctx, 999999, &dir_name) {
        Ok(_) => panic!("rmdir succeeded with invalid parent inode"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::EBADF)),
    }

    // Test: Try to remove non-empty directory
    match fs.rmdir(ctx, 1, &dir_name) {
        Ok(_) => panic!("rmdir succeeded on non-empty directory"),
        Err(e) => {
            assert_eq!(e.raw_os_error(), Some(libc::ENOTEMPTY));
        }
    }

    // Test: Try to remove with invalid name (containing path traversal)
    let invalid_name = CString::new("../dir1").unwrap();
    match fs.rmdir(ctx, 1, &invalid_name) {
        Ok(_) => panic!("rmdir succeeded with invalid name"),
        Err(e) => {
            assert_eq!(
                e.kind(),
                io::ErrorKind::PermissionDenied,
                "Expected PermissionDenied error, got {:?}",
                e.kind()
            );
        }
    }

    Ok(())
}

#[test]
fn test_rmdir_complex_layers() -> io::Result<()> {
    // Create an overlayfs with complex layer structure:
    // - Lower layer: base directories
    // - Middle layer: some directories deleted, some added
    // - Upper layer: more modifications
    let (fs, temp_dirs) = helper::create_overlayfs(vec![
        vec![
            // lower layer
            ("dir1", true, 0o755),
            ("dir1/subdir1", true, 0o755),
            ("dir2", true, 0o755),
            ("dir2/subdir2", true, 0o755),
        ],
        vec![
            // middle layer
            ("dir1/new_dir", true, 0o755),
            ("dir2/subdir3", true, 0o755),
            // Whiteout in middle layer for subdir2 in dir2
            ("dir2/.wh.subdir2", false, 0o000),
        ],
        vec![
            // upper layer
            ("dir3", true, 0o755),
            ("dir3/subdir4", true, 0o755),
        ],
    ])?;
    helper::debug_print_layers(&temp_dirs, false)?;
    let ctx = Context::default();

    // Test 1: Remove a directory that exists in the top layer
    let dir3_name = CString::new("dir3").unwrap();
    let subdir4_name = CString::new("subdir4").unwrap();
    let dir3_entry = fs.lookup(ctx, 1, &dir3_name)?;
    fs.rmdir(ctx, dir3_entry.inode, &subdir4_name)?;
    assert!(!temp_dirs[2].path().join("dir3/subdir4").exists());

    // Test 2: Remove a directory from middle layer
    let dir1_name = CString::new("dir1").unwrap();
    let new_dir_name = CString::new("new_dir").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;
    fs.rmdir(ctx, dir1_entry.inode, &new_dir_name)?;
    // Expect a whiteout created in the top layer for new_dir
    assert!(temp_dirs[2].path().join("dir1/.wh.new_dir").exists());

    // Test 3: Remove a directory from lowest layer
    let subdir1_name = CString::new("subdir1").unwrap();
    fs.rmdir(ctx, dir1_entry.inode, &subdir1_name)?;
    // Expect a whiteout in the top layer but the original directory remains in lower layer
    assert!(temp_dirs[2].path().join("dir1/.wh.subdir1").exists());
    assert!(temp_dirs[0].path().join("dir1/subdir1").exists());

    Ok(())
}

mod helper {
    use std::{
        fs::{self, File},
        os::unix::fs::PermissionsExt,
        process::Command,
    };

    use super::*;
    use tempfile::TempDir;

    // Helper function to create a temporary directory with specified files
    pub(super) fn setup_test_layer(files: &[(&str, bool, u32)]) -> io::Result<TempDir> {
        let dir = TempDir::new().unwrap();

        for (path, is_dir, mode) in files {
            let full_path = dir.path().join(path);
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent)?;
            }

            if *is_dir {
                fs::create_dir(&full_path)?;
            } else {
                File::create(&full_path)?;
            }

            fs::set_permissions(&full_path, fs::Permissions::from_mode(*mode))?;
        }

        Ok(dir)
    }

    // Helper function to create an overlayfs with specified layers
    pub(super) fn create_overlayfs(
        layers: Vec<Vec<(&str, bool, u32)>>,
    ) -> io::Result<(OverlayFs, Vec<TempDir>)> {
        let mut temp_dirs = Vec::new();
        let mut layer_paths = Vec::new();

        for layer in layers {
            let temp_dir = setup_test_layer(&layer)?;
            layer_paths.push(temp_dir.path().to_path_buf());
            temp_dirs.push(temp_dir);
        }

        let cfg = Config::default();
        let overlayfs = OverlayFs::new(layer_paths, cfg)?;
        Ok((overlayfs, temp_dirs))
    }

    // Debug utility to print the directory structure of each layer using tree command
    pub(super) fn debug_print_layers(temp_dirs: &[TempDir], show_perms: bool) -> io::Result<()> {
        println!("\n=== Layer Directory Structures ===");

        for (i, dir) in temp_dirs.iter().enumerate() {
            println!("\nLayer {}: {}", i, dir.path().display());

            let path = dir.path();
            let mut tree_cmd = Command::new("tree");
            tree_cmd.arg("-a"); // show hidden files
            if show_perms {
                tree_cmd.arg("-p");
            }
            let output = tree_cmd.arg(path).output()?;

            if output.status.success() {
                println!("{}", String::from_utf8_lossy(&output.stdout));
            } else {
                println!(
                    "Error running tree command: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }

        println!("================================\n");

        Ok(())
    }
}
