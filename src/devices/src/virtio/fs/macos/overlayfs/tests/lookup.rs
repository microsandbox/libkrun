use std::{ffi::CString, io};

use crate::virtio::{fs::filesystem::{Context, FileSystem}, fuse::FsOptions};

use super::helper;

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
