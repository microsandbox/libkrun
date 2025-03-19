use std::{ffi::CString, fs, io};

use crate::virtio::{
    fs::filesystem::{Context, FileSystem}, fuse::FsOptions, macos::overlayfs::tests::helper::TestContainer,
};

use super::helper;

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

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
    match result {
        Err(e) => {
            assert_eq!(
                e.raw_os_error(),
                Some(libc::EINVAL),
                "Reading link of regular file should return EINVAL"
            );
        }
        Ok(_) => panic!("Expected error for regular file"),
    }

    // Test readlink on directory (should fail)
    let dir_name = CString::new("directory").unwrap();
    let dir_entry = fs.lookup(Context::default(), 1, &dir_name)?;
    let result = fs.readlink(Context::default(), dir_entry.inode);
    match result {
        Err(e) => {
            assert_eq!(
                e.raw_os_error(),
                Some(libc::EINVAL),
                "Reading link of directory should return EINVAL"
            );
        }
        Ok(_) => panic!("Expected error for directory"),
    }

    // Test readlink with invalid inode
    let result = fs.readlink(Context::default(), 999999);
    match result {
        Err(e) => {
            assert_eq!(
                e.raw_os_error(),
                Some(libc::EBADF),
                "Reading link with invalid inode should return EBADF"
            );
        }
        Ok(_) => panic!("Expected error for invalid inode"),
    }

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
fn test_read_basic() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a file with content
    let layers = vec![vec![("file1", false, 0o644)]];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Write some content to the file
    std::fs::write(temp_dirs[0].path().join("file1"), b"Hello, World!")?;

    let ctx = Context::default();

    // Lookup and open the file
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;
    let (handle, _opts) = fs.open(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the entire content
    let mut writer = TestContainer(Vec::new());
    let bytes_read = fs.read(ctx, entry.inode, handle, &mut writer, 100, 0, None, 0)?;

    assert_eq!(bytes_read, 13); // Length of "Hello, World!"
    assert_eq!(&writer.0, b"Hello, World!");

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle, false, false, None)?;

    Ok(())
}

#[test]
fn test_read_with_offset() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a file with content
    let layers = vec![vec![("file1", false, 0o644)]];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Write some content to the file
    std::fs::write(temp_dirs[0].path().join("file1"), b"Hello, World!")?;

    let ctx = Context::default();

    // Lookup and open the file
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;
    let (handle, _opts) = fs.open(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read with offset
    let mut writer = TestContainer(Vec::new());
    let bytes_read = fs.read(
        ctx,
        entry.inode,
        handle,
        &mut writer,
        100,
        7, // Start at offset 7 (after "Hello, ")
        None,
        0,
    )?;

    assert_eq!(bytes_read, 6); // Length of "World!"
    assert_eq!(&writer.0, b"World!");

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle, false, false, None)?;

    Ok(())
}

#[test]
fn test_read_partial() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a file with content
    let layers = vec![vec![("file1", false, 0o644)]];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Write some content to the file
    std::fs::write(temp_dirs[0].path().join("file1"), b"Hello, World!")?;

    let ctx = Context::default();

    // Lookup and open the file
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;
    let (handle, _opts) = fs.open(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read only first 5 bytes
    let mut writer = TestContainer(Vec::new());
    let bytes_read = fs.read(
        ctx,
        entry.inode,
        handle,
        &mut writer,
        5, // Only read 5 bytes
        0,
        None,
        0,
    )?;

    assert_eq!(bytes_read, 5);
    assert_eq!(&writer.0, b"Hello");

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle, false, false, None)?;

    Ok(())
}

#[test]
fn test_read_whiteout() -> io::Result<()> {
    // Create test layers:
    // Layer 0 (bottom): file1 with content
    // Layer 1 (top): .wh.file1 (whiteout for file1)
    let layers = vec![
        vec![("file1", false, 0o644)],
        vec![(".wh.file1", false, 0o000)],
    ];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Write some content to the file in bottom layer
    std::fs::write(temp_dirs[0].path().join("file1"), b"Hello, World!")?;

    let ctx = Context::default();

    // Try to lookup the file (should fail because it's whited out)
    let file_name = CString::new("file1").unwrap();
    assert!(fs.lookup(ctx, 1, &file_name).is_err());

    Ok(())
}

#[test]
fn test_read_after_copy_up() -> io::Result<()> {
    // Create test layers:
    // Layer 0 (bottom): file1 with content
    // Layer 1 (top): empty
    let layers = vec![vec![("file1", false, 0o644)], vec![]];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Write some content to the file in bottom layer
    std::fs::write(temp_dirs[0].path().join("file1"), b"Hello, World!")?;

    let ctx = Context::default();

    // Lookup the file
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;

    // Open with write flag to trigger copy-up
    let (handle, _opts) = fs.open(ctx, entry.inode, libc::O_RDWR as u32)?;
    let handle = handle.unwrap();

    // Verify the file was copied up
    assert!(temp_dirs[1].path().join("file1").exists());

    // Read the content after copy-up
    let mut writer = TestContainer(Vec::new());
    let bytes_read = fs.read(ctx, entry.inode, handle, &mut writer, 100, 0, None, 0)?;

    assert_eq!(bytes_read, 13);
    assert_eq!(&writer.0, b"Hello, World!");

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle, false, false, None)?;

    Ok(())
}

#[test]
fn test_read_invalid_handle() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a file
    let layers = vec![vec![("file1", false, 0o644)]];
    let (fs, _) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Try to read with an invalid handle
    let mut writer = TestContainer(Vec::new());
    let result = fs.read(
        ctx,
        1,
        999, // Invalid handle
        &mut writer,
        100,
        0,
        None,
        0,
    );

    assert!(result.is_err());
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EBADF));

    Ok(())
}

#[test]
fn test_read_multiple_times() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a file
    let layers = vec![vec![("file1", false, 0o644)]];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Write some content to the file
    std::fs::write(temp_dirs[0].path().join("file1"), b"Hello, World!")?;

    let ctx = Context::default();

    // Lookup and open the file
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;
    let (handle, _opts) = fs.open(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the file multiple times with different offsets
    let test_cases: Vec<(u64, u32, &[u8])> =
        vec![(0, 5, b"Hello"), (7, 5, b"World"), (12, 1, b"!")];

    for (offset, size, expected) in test_cases {
        let mut writer = TestContainer(Vec::new());
        let bytes_read = fs.read(ctx, entry.inode, handle, &mut writer, size, offset, None, 0)?;

        assert_eq!(bytes_read, expected.len());
        assert_eq!(&writer.0, expected);
    }

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle, false, false, None)?;

    Ok(())
}

#[test]
fn test_read_nested_directories() -> io::Result<()> {
    // Create test layers with nested structure:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/file1 (content: "bottom file1")
    //   - dir1/subdir/
    //   - dir1/subdir/file2 (content: "bottom file2")
    // Layer 1 (middle):
    //   - dir1/file3 (content: "middle file3")
    //   - dir1/subdir/file4 (content: "middle file4")
    // Layer 2 (top):
    //   - dir1/file1 (content: "top file1")
    //   - dir1/subdir/file5 (content: "top file5")
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/file2", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/file3", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/file4", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/file5", false, 0o644),
        ],
    ];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Write content to files in different layers
    std::fs::write(temp_dirs[0].path().join("dir1/file1"), b"bottom file1")?;
    std::fs::write(
        temp_dirs[0].path().join("dir1/subdir/file2"),
        b"bottom file2",
    )?;
    std::fs::write(temp_dirs[1].path().join("dir1/file3"), b"middle file3")?;
    std::fs::write(
        temp_dirs[1].path().join("dir1/subdir/file4"),
        b"middle file4",
    )?;
    std::fs::write(temp_dirs[2].path().join("dir1/file1"), b"top file1")?;
    std::fs::write(temp_dirs[2].path().join("dir1/subdir/file5"), b"top file5")?;

    let ctx = Context::default();

    // First lookup dir1
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;

    // Test 1: Read file1 (should get content from top layer)
    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(ctx, dir1_entry.inode, &file1_name)?;
    let (handle, _) = fs.open(ctx, file1_entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    let mut writer = TestContainer(Vec::new());
    let bytes_read = fs.read(ctx, file1_entry.inode, handle, &mut writer, 100, 0, None, 0)?;
    assert_eq!(bytes_read, 9);
    assert_eq!(&writer.0, b"top file1");
    fs.release(ctx, file1_entry.inode, 0, handle, false, false, None)?;

    // Test 2: Read file3 (from middle layer)
    let file3_name = CString::new("file3").unwrap();
    let file3_entry = fs.lookup(ctx, dir1_entry.inode, &file3_name)?;
    let (handle, _) = fs.open(ctx, file3_entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    let mut writer = TestContainer(Vec::new());
    let bytes_read = fs.read(ctx, file3_entry.inode, handle, &mut writer, 100, 0, None, 0)?;
    assert_eq!(bytes_read, 12);
    assert_eq!(&writer.0, b"middle file3");
    fs.release(ctx, file3_entry.inode, 0, handle, false, false, None)?;

    // Lookup subdir
    let subdir_name = CString::new("subdir").unwrap();
    let subdir_entry = fs.lookup(ctx, dir1_entry.inode, &subdir_name)?;

    // Test 3: Read file2 (from bottom layer)
    let file2_name = CString::new("file2").unwrap();
    let file2_entry = fs.lookup(ctx, subdir_entry.inode, &file2_name)?;
    let (handle, _) = fs.open(ctx, file2_entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    let mut writer = TestContainer(Vec::new());
    let bytes_read = fs.read(ctx, file2_entry.inode, handle, &mut writer, 100, 0, None, 0)?;
    assert_eq!(bytes_read, 12);
    assert_eq!(&writer.0, b"bottom file2");
    fs.release(ctx, file2_entry.inode, 0, handle, false, false, None)?;

    Ok(())
}

#[test]
fn test_read_with_whiteouts_and_opaque_dirs() -> io::Result<()> {
    // Create test layers with whiteouts and opaque directories:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/file1 (content: "file1")
    //   - dir1/subdir/
    //   - dir1/subdir/file2 (content: "file2")
    // Layer 1 (middle):
    //   - dir1/
    //   - dir1/.wh.file1 (whiteout file1)
    //   - dir1/subdir/
    //   - dir1/subdir/.wh..wh..opq (opaque dir)
    //   - dir1/subdir/file3 (content: "file3")
    // Layer 2 (top):
    //   - dir1/
    //   - dir1/file4 (content: "file4")
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/file2", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/.wh.file1", false, 0o000),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/.wh..wh..opq", false, 0o000),
            ("dir1/subdir/file3", false, 0o644),
        ],
        vec![("dir1", true, 0o755), ("dir1/file4", false, 0o644)],
    ];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Write content to files
    std::fs::write(temp_dirs[0].path().join("dir1/file1"), b"file1")?;
    std::fs::write(temp_dirs[0].path().join("dir1/subdir/file2"), b"file2")?;
    std::fs::write(temp_dirs[1].path().join("dir1/subdir/file3"), b"file3")?;
    std::fs::write(temp_dirs[2].path().join("dir1/file4"), b"file4")?;

    let ctx = Context::default();

    // First lookup dir1
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;

    // Test 1: Try to read whited-out file1 (should fail)
    let file1_name = CString::new("file1").unwrap();
    assert!(fs.lookup(ctx, dir1_entry.inode, &file1_name).is_err());

    // Test 2: Read file4 from top layer
    let file4_name = CString::new("file4").unwrap();
    let file4_entry = fs.lookup(ctx, dir1_entry.inode, &file4_name)?;
    let (handle, _) = fs.open(ctx, file4_entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    let mut writer = TestContainer(Vec::new());
    let bytes_read = fs.read(ctx, file4_entry.inode, handle, &mut writer, 100, 0, None, 0)?;
    assert_eq!(bytes_read, 5);
    assert_eq!(&writer.0, b"file4");
    fs.release(ctx, file4_entry.inode, 0, handle, false, false, None)?;

    // Lookup subdir
    let subdir_name = CString::new("subdir").unwrap();
    let subdir_entry = fs.lookup(ctx, dir1_entry.inode, &subdir_name)?;

    // Test 3: Try to read file2 through opaque directory (should fail)
    let file2_name = CString::new("file2").unwrap();
    assert!(fs.lookup(ctx, subdir_entry.inode, &file2_name).is_err());

    // Test 4: Read file3 through opaque directory (should succeed)
    let file3_name = CString::new("file3").unwrap();
    let file3_entry = fs.lookup(ctx, subdir_entry.inode, &file3_name)?;
    let (handle, _) = fs.open(ctx, file3_entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    let mut writer = TestContainer(Vec::new());
    let bytes_read = fs.read(ctx, file3_entry.inode, handle, &mut writer, 100, 0, None, 0)?;
    assert_eq!(bytes_read, 5);
    assert_eq!(&writer.0, b"file3");
    fs.release(ctx, file3_entry.inode, 0, handle, false, false, None)?;

    Ok(())
}

#[test]
fn test_readdir_basic() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a directory with files
    let layers = vec![vec![
        ("dir1", true, 0o755),
        ("dir1/file1", false, 0o644),
        ("dir1/file2", false, 0o644),
    ]];
    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup and open the directory
    let dir_name = CString::new("dir1").unwrap();
    let entry = fs.lookup(ctx, 1, &dir_name)?;
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the directory entries
    let mut entries = Vec::new();
    fs.readdir(ctx, entry.inode, handle, 4096, 0, |dir_entry| {
        let name = String::from_utf8_lossy(dir_entry.name).to_string();
        entries.push(name);
        Ok(1)
    })?;

    // Verify the entries
    assert!(entries.contains(&"file1".to_string()));
    assert!(entries.contains(&"file2".to_string()));
    assert_eq!(entries.len(), 2);

    Ok(())
}

#[test]
fn test_readdir_with_offset() -> io::Result<()> {
    // Create an overlayfs with multiple layers containing overlapping directories and files
    // Layer 0 (lowest): Some initial files
    // Layer 1 (middle): Some additional files and modifications
    // Layer 2 (top): More files and potential whiteouts
    let layers = vec![
        // Layer 0 (lowest)
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/file2", false, 0o644),
            ("dir1/common", false, 0o644),
        ],
        // Layer 1 (middle)
        vec![
            ("dir1", true, 0o755),
            ("dir1/file3", false, 0o644),
            ("dir1/file4", false, 0o644),
            ("dir1/common", false, 0o644), // This overlays the one in layer 0
        ],
        // Layer 2 (top)
        vec![
            ("dir1", true, 0o755),
            ("dir1/file5", false, 0o644),
            ("dir1/file6", false, 0o644),
            ("dir1/file7", false, 0o644),
        ],
    ];
    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup and open the directory
    let dir_name = CString::new("dir1").unwrap();
    let entry = fs.lookup(ctx, 1, &dir_name)?;
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the first batch of directory entries and save the offset
    let mut entries = Vec::new();
    let mut last_offset = 0;
    fs.readdir(
        ctx,
        entry.inode,
        handle,
        1024, // Small buffer to force multiple reads
        0,
        |dir_entry| {
            let name = String::from_utf8_lossy(dir_entry.name).to_string();
            entries.push(name);
            last_offset = dir_entry.offset;
            Ok(0)
        },
    )?;

    println!("entries: {:?}", entries);

    // Read the second batch of directory entries starting from the last offset
    let mut more_entries = Vec::new();
    fs.readdir(ctx, entry.inode, handle, 4096, last_offset, |dir_entry| {
        let name = String::from_utf8_lossy(dir_entry.name).to_string();
        more_entries.push(name);
        Ok(1)
    })?;

    println!("more_entries: {:?}", more_entries);

    // Verify that we got all entries between the two reads
    let all_entries: Vec<_> = entries
        .into_iter()
        .chain(more_entries.into_iter())
        .collect();

    println!("all_entries: {:?}", all_entries);
    assert!(all_entries.contains(&"file1".to_string()));
    assert!(all_entries.contains(&"file2".to_string()));
    assert!(all_entries.contains(&"file3".to_string()));
    assert!(all_entries.contains(&"file4".to_string()));
    assert!(all_entries.contains(&"file5".to_string()));
    assert!(all_entries.contains(&"file6".to_string()));
    assert!(all_entries.contains(&"file7".to_string()));
    assert!(all_entries.contains(&"common".to_string()));

    // Verify we have the right number of entries
    assert_eq!(all_entries.len(), 8);

    Ok(())
}

#[test]
fn test_readdir_empty_directory() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing an empty directory
    let layers = vec![vec![("empty_dir", true, 0o755)]];
    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup and open the directory
    let dir_name = CString::new("empty_dir").unwrap();
    let entry = fs.lookup(ctx, 1, &dir_name)?;
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the directory entries
    let mut entries = Vec::new();
    fs.readdir(ctx, entry.inode, handle, 4096, 0, |dir_entry| {
        let name = String::from_utf8_lossy(dir_entry.name).to_string();
        entries.push(name);
        Ok(0)
    })?;

    // Verify the entries (should be empty since "." and ".." are handled by the kernel)
    assert_eq!(entries.len(), 0);

    Ok(())
}

#[test]
fn test_readdir_whiteout() -> io::Result<()> {
    // Create an overlayfs with two layers:
    // Layer 0 (bottom): dir1 with file1, file2, file3
    // Layer 1 (top): dir1 with file2 whited out
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/file2", false, 0o644),
            ("dir1/file3", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/.wh.file2", false, 0o644), // Whiteout for file2
        ],
    ];
    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup and open the directory
    let dir_name = CString::new("dir1").unwrap();
    let entry = fs.lookup(ctx, 1, &dir_name)?;
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the directory entries
    let mut entries = Vec::new();
    fs.readdir(ctx, entry.inode, handle, 4096, 0, |dir_entry| {
        let name = String::from_utf8_lossy(dir_entry.name).to_string();
        entries.push(name);
        Ok(1)
    })?;

    // Verify the entries (should include "file1" and "file3", but not "file2")
    assert!(entries.contains(&"file1".to_string()));
    assert!(entries.contains(&"file3".to_string()));
    assert!(!entries.contains(&"file2".to_string())); // Should be whited out
    assert_eq!(entries.len(), 2);

    Ok(())
}

#[test]
fn test_readdir_multiple_layers() -> io::Result<()> {
    let layers = vec![
        vec![("dir1", true, 0o755), ("dir1/file1", false, 0o644)],
        vec![
            ("dir1", true, 0o755),
            ("dir1/file2", false, 0o644),
            ("dir2", true, 0o755),
            ("dir2/file1", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/file3", false, 0o644),
            ("dir2/file2", false, 0o644),
            ("dir3", true, 0o755),
            ("dir3/file1", false, 0o644),
        ],
    ];
    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup and open the dir1
    let entry = fs.lookup(ctx, 1, &CString::new("dir1").unwrap())?;
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the directory entries
    let mut entries = Vec::new();
    fs.readdir(ctx, entry.inode, handle, 4096, 0, |dir_entry| {
        let name = String::from_utf8_lossy(dir_entry.name).to_string();
        entries.push(name);
        Ok(1)
    })?;

    // Verify the entries (should include "file1", "file2", and "file3")
    assert!(entries.contains(&"file1".to_string()));
    assert!(entries.contains(&"file2".to_string()));
    assert!(entries.contains(&"file3".to_string()));
    assert_eq!(entries.len(), 3);

    // Lookup and open the dir2
    let entry = fs.lookup(ctx, 1, &CString::new("dir2").unwrap())?;
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the directory entries
    let mut entries = Vec::new();
    fs.readdir(ctx, entry.inode, handle, 4096, 0, |dir_entry| {
        let name = String::from_utf8_lossy(dir_entry.name).to_string();
        entries.push(name);
        Ok(1)
    })?;

    // Verify the entries (should include "file1", and "file2")
    assert!(entries.contains(&"file1".to_string()));
    assert!(entries.contains(&"file2".to_string()));
    assert_eq!(entries.len(), 2);

    // Lookup and open the dir3
    let entry = fs.lookup(ctx, 1, &CString::new("dir3").unwrap())?;
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the directory entries
    let mut entries = Vec::new();
    fs.readdir(ctx, entry.inode, handle, 4096, 0, |dir_entry| {
        let name = String::from_utf8_lossy(dir_entry.name).to_string();
        entries.push(name);
        Ok(1)
    })?;

    // Verify the entries (should include "file1")
    assert!(entries.contains(&"file1".to_string()));
    assert_eq!(entries.len(), 1);

    Ok(())
}

#[test]
fn test_readdir_opaque_marker() -> io::Result<()> {
    // Create an overlayfs with three layers:
    // Layer 0 (bottom): dir1 with file1, file2, file3
    // Layer 1 (middle): dir1 with opaque marker, file4, file5
    // Layer 2 (top): dir1 with file5 (shadows middle), file6, file7
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/file2", false, 0o644),
            ("dir1/file3", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/.wh..wh..opq", false, 0o644), // Opaque marker for dir1
            ("dir1/file4", false, 0o644),
            ("dir1/file5", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/file5", false, 0o644), // Shadows file5 from layer 1
            ("dir1/file6", false, 0o644),
            ("dir1/file7", false, 0o644),
        ],
    ];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup and open the directory
    let dir_name = CString::new("dir1").unwrap();
    let entry = fs.lookup(ctx, 1, &dir_name)?;
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the directory entries
    let mut entries = Vec::new();
    fs.readdir(ctx, entry.inode, handle, 4096, 0, |dir_entry| {
        let name = String::from_utf8_lossy(dir_entry.name).to_string();
        entries.push(name);
        Ok(1)
    })?;

    // Sort entries for consistent comparison
    entries.sort();

    // Due to the opaque marker in the middle layer, we should only see:
    // - files from the top layer (file5, file6, file7)
    // - files from the middle layer that aren't shadowed by the top (file4)
    // - NO files from the bottom layer (file1, file2, file3 should be hidden)
    let expected_entries = vec![
        "file4".to_string(),
        "file5".to_string(),
        "file6".to_string(),
        "file7".to_string(),
    ];

    assert_eq!(entries, expected_entries, "Unexpected directory entries");

    // Release the directory handle
    fs.releasedir(ctx, entry.inode, 0, handle)?;

    // Additional test: Create a second directory with opaque marker in top layer
    let layers2 = vec![
        vec![
            ("dir2", true, 0o755),
            ("dir2/bottom1", false, 0o644),
            ("dir2/bottom2", false, 0o644),
        ],
        vec![
            ("dir2", true, 0o755),
            ("dir2/middle1", false, 0o644),
            ("dir2/middle2", false, 0o644),
        ],
        vec![
            ("dir2", true, 0o755),
            ("dir2/.wh..wh..opq", false, 0o644), // Opaque marker in top layer
            ("dir2/top1", false, 0o644),
        ],
    ];

    let (fs2, _temp_dirs2) = helper::create_overlayfs(layers2)?;
    let ctx = Context::default();

    // Lookup and open the directory
    let dir_name = CString::new("dir2").unwrap();
    let entry = fs2.lookup(ctx, 1, &dir_name)?;
    let (handle, _opts) = fs2.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the directory entries
    let mut entries = Vec::new();
    fs2.readdir(ctx, entry.inode, handle, 4096, 0, |dir_entry| {
        let name = String::from_utf8_lossy(dir_entry.name).to_string();
        entries.push(name);
        Ok(1)
    })?;

    // Sort entries for consistent comparison
    entries.sort();

    // With opaque marker in the top layer, we should only see:
    // - files from the top layer (top1)
    // - NO files from middle or bottom layers
    assert_eq!(
        entries,
        vec!["top1".to_string()],
        "Unexpected entries in dir2"
    );

    // Release the directory handle
    fs2.releasedir(ctx, entry.inode, 0, handle)?;

    Ok(())
}

#[test]
fn test_readdir_shadow() -> io::Result<()> {
    // Create an overlayfs with three layers with shadowing:
    // Layer 0 (bottom): dir1 with common, only_bottom, shadowed1, shadowed2
    // Layer 1 (middle): dir1 with common, only_middle, shadowed1
    // Layer 2 (top): dir1 with common, only_top, shadowed2
    //
    // Each file has different content to verify proper shadowing
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/common", false, 0o644),
            ("dir1/only_bottom", false, 0o644),
            ("dir1/shadowed1", false, 0o644),
            ("dir1/shadowed2", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/common", false, 0o644),
            ("dir1/only_middle", false, 0o644),
            ("dir1/shadowed1", false, 0o644),
        ],
        vec![
            ("dir1", true, 0o755),
            ("dir1/common", false, 0o644),
            ("dir1/only_top", false, 0o644),
            ("dir1/shadowed2", false, 0o644),
        ],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Write different content to each layer's files
    // Bottom layer
    fs::write(
        temp_dirs[0].path().join("dir1/common"),
        "bottom layer common content",
    )?;
    fs::write(
        temp_dirs[0].path().join("dir1/only_bottom"),
        "only in bottom layer",
    )?;
    fs::write(
        temp_dirs[0].path().join("dir1/shadowed1"),
        "shadowed1 bottom content",
    )?;
    fs::write(
        temp_dirs[0].path().join("dir1/shadowed2"),
        "shadowed2 bottom content",
    )?;

    // Middle layer
    fs::write(
        temp_dirs[1].path().join("dir1/common"),
        "middle layer common content",
    )?;
    fs::write(
        temp_dirs[1].path().join("dir1/only_middle"),
        "only in middle layer",
    )?;
    fs::write(
        temp_dirs[1].path().join("dir1/shadowed1"),
        "shadowed1 middle content",
    )?;

    // Top layer
    fs::write(
        temp_dirs[2].path().join("dir1/common"),
        "top layer common content",
    )?;
    fs::write(
        temp_dirs[2].path().join("dir1/only_top"),
        "only in top layer",
    )?;
    fs::write(
        temp_dirs[2].path().join("dir1/shadowed2"),
        "shadowed2 top content",
    )?;

    let ctx = Context::default();

    // Lookup and open the directory
    let dir_name = CString::new("dir1").unwrap();
    let entry = fs.lookup(ctx, 1, &dir_name)?;
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    // Read the directory entries
    let mut entries = Vec::new();
    fs.readdir(ctx, entry.inode, handle, 4096, 0, |dir_entry| {
        let name = String::from_utf8_lossy(dir_entry.name).to_string();
        entries.push(name);
        Ok(1)
    })?;

    // Sort entries for consistent comparison
    entries.sort();

    // Release the directory handle
    fs.releasedir(ctx, entry.inode, 0, handle)?;

    // We should see all unique filenames across layers
    // Each file should appear exactly once
    let expected_entries = vec![
        "common".to_string(),
        "only_bottom".to_string(),
        "only_middle".to_string(),
        "only_top".to_string(),
        "shadowed1".to_string(),
        "shadowed2".to_string(),
    ];

    assert_eq!(entries, expected_entries, "Unexpected directory entries");

    // Now verify the content of each file to check shadowing

    // 1. common file - should have top layer content
    let common_entry = fs.lookup(ctx, entry.inode, &CString::new("common").unwrap())?;
    let (handle, _) = fs.open(ctx, common_entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    let mut container = TestContainer(Vec::new());
    fs.read(
        ctx,
        common_entry.inode,
        handle,
        &mut container,
        1024,
        0,
        None,
        0,
    )?;
    assert_eq!(
        String::from_utf8_lossy(&container.0),
        "top layer common content",
        "common file should have top layer content"
    );
    fs.release(ctx, common_entry.inode, 0, handle, false, false, None)?;

    // 2. shadowed1 file - should have middle layer content (shadowed by middle over bottom)
    let shadowed1_entry = fs.lookup(ctx, entry.inode, &CString::new("shadowed1").unwrap())?;
    let (handle, _) = fs.open(ctx, shadowed1_entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    let mut container = TestContainer(Vec::new());
    fs.read(
        ctx,
        shadowed1_entry.inode,
        handle,
        &mut container,
        1024,
        0,
        None,
        0,
    )?;
    assert_eq!(
        String::from_utf8_lossy(&container.0),
        "shadowed1 middle content",
        "shadowed1 file should have middle layer content"
    );
    fs.release(ctx, shadowed1_entry.inode, 0, handle, false, false, None)?;

    // 3. shadowed2 file - should have top layer content (shadowed by top over bottom)
    let shadowed2_entry = fs.lookup(ctx, entry.inode, &CString::new("shadowed2").unwrap())?;
    let (handle, _) = fs.open(ctx, shadowed2_entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    let mut container = TestContainer(Vec::new());
    fs.read(
        ctx,
        shadowed2_entry.inode,
        handle,
        &mut container,
        1024,
        0,
        None,
        0,
    )?;
    assert_eq!(
        String::from_utf8_lossy(&container.0),
        "shadowed2 top content",
        "shadowed2 file should have top layer content"
    );
    fs.release(ctx, shadowed2_entry.inode, 0, handle, false, false, None)?;

    // 4. only_bottom file - should exist and have bottom layer content
    let only_bottom_entry = fs.lookup(ctx, entry.inode, &CString::new("only_bottom").unwrap())?;
    let (handle, _) = fs.open(ctx, only_bottom_entry.inode, libc::O_RDONLY as u32)?;
    let handle = handle.unwrap();

    let mut container = TestContainer(Vec::new());
    fs.read(
        ctx,
        only_bottom_entry.inode,
        handle,
        &mut container,
        1024,
        0,
        None,
        0,
    )?;
    assert_eq!(
        String::from_utf8_lossy(&container.0),
        "only in bottom layer",
        "only_bottom file should have bottom layer content"
    );
    fs.release(ctx, only_bottom_entry.inode, 0, handle, false, false, None)?;

    Ok(())
}
