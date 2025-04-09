use std::{ffi::CString, io};

use crate::virtio::{fs::filesystem::{Context, FileSystem}, overlayfs::tests::helper::TestContainer};

use super::helper;

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[test]
fn test_write_basic() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing an empty file
    let layers = vec![vec![("file1", false, 0o644)]];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    let ctx = Context::default();

    // Lookup and open the file with write permissions
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;
    let (handle, _opts) = fs.open(ctx, entry.inode, (libc::O_WRONLY | libc::O_TRUNC) as u32)?;
    let handle = handle.unwrap();

    // Write content to the file
    let content = b"Hello, World!";
    let mut reader = TestContainer(content.to_vec());
    let bytes_written = fs.write(
        ctx,
        entry.inode,
        handle,
        &mut reader,
        content.len() as u32,
        0,
        None,
        false,
        false,
        0,
    )?;

    assert_eq!(bytes_written, content.len());

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle, false, false, None)?;

    // Verify the content was written correctly
    let file_content = std::fs::read(temp_dirs[0].path().join("file1"))?;
    assert_eq!(file_content, content);

    Ok(())
}

#[test]
fn test_write_with_offset() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a file with initial content
    let layers = vec![vec![("file1", false, 0o644)]];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Write some initial content to the file
    std::fs::write(temp_dirs[0].path().join("file1"), b"Hello, World!")?;

    let ctx = Context::default();

    // Lookup and open the file with write permissions
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;
    let (handle, _opts) = fs.open(ctx, entry.inode, libc::O_WRONLY as u32)?;
    let handle = handle.unwrap();

    // Write content at an offset
    let content = b"Rusty";
    let mut reader = TestContainer(content.to_vec());
    let bytes_written = fs.write(
        ctx,
        entry.inode,
        handle,
        &mut reader,
        content.len() as u32,
        7,
        None,
        false,
        false,
        0,
    )?;

    assert_eq!(bytes_written, content.len());

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle, false, false, None)?;

    // Verify the content was written correctly
    let file_content = std::fs::read(temp_dirs[0].path().join("file1"))?;
    assert_eq!(&file_content, b"Hello, Rusty!");

    Ok(())
}

#[test]
fn test_write_partial() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing an empty file
    let layers = vec![vec![("file1", false, 0o644)]];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    let ctx = Context::default();

    // Lookup and open the file with write permissions
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;
    let (handle, _opts) = fs.open(ctx, entry.inode, (libc::O_WRONLY | libc::O_TRUNC) as u32)?;
    let handle = handle.unwrap();

    // Write content to the file, but request to write more than we have
    let content = b"Hello, World!";
    let mut reader = TestContainer(content.to_vec());
    let bytes_written = fs.write(
        ctx,
        entry.inode,
        handle,
        &mut reader,
        100,
        0,
        None,
        false,
        false,
        0,
    )?;

    // Should only write what's available
    assert_eq!(bytes_written, content.len());

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle, false, false, None)?;

    // Verify the content was written correctly
    let file_content = std::fs::read(temp_dirs[0].path().join("file1"))?;
    assert_eq!(file_content, content);

    Ok(())
}

#[test]
fn test_write_whiteout() -> io::Result<()> {
    // Create an overlayfs with two layers, where the top layer has a whiteout for file1
    let layers = vec![
        vec![("file1", false, 0o644)],
        vec![(".wh.file1", false, 0o644)], // Whiteout for file1
    ];
    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;

    let ctx = Context::default();

    // Lookup and open the file (should fail because it's whited out)
    let file_name = CString::new("file1").unwrap();
    let lookup_result = fs.lookup(ctx, 1, &file_name);
    assert!(lookup_result.is_err());

    Ok(())
}

#[test]
fn test_write_after_copy_up() -> io::Result<()> {
    // Create an overlayfs with two layers, where file1 exists in the lower layer
    let layers = vec![vec![("file1", false, 0o644)], vec![]];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    // Write some initial content to the file in the lower layer
    std::fs::write(temp_dirs[0].path().join("file1"), b"Hello, World!")?;

    let ctx = Context::default();

    // Lookup and open the file with write permissions (should trigger copy-up)
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;
    let (handle, _opts) = fs.open(ctx, entry.inode, libc::O_WRONLY as u32)?;
    let handle = handle.unwrap();

    // Write new content to the file
    let content = b"Hello, Rusty!";
    let mut reader = TestContainer(content.to_vec());
    let bytes_written = fs.write(
        ctx,
        entry.inode,
        handle,
        &mut reader,
        content.len() as u32,
        0,
        None,
        false,
        false,
        0,
    )?;

    assert_eq!(bytes_written, content.len());

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle, false, false, None)?;

    // Verify the content was written correctly to the upper layer
    let file_content = std::fs::read(temp_dirs[1].path().join("file1"))?;
    assert_eq!(file_content, content);

    // The lower layer should remain unchanged
    let lower_content = std::fs::read(temp_dirs[0].path().join("file1"))?;
    assert_eq!(lower_content, b"Hello, World!");

    Ok(())
}

#[test]
fn test_write_invalid_handle() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a file
    let layers = vec![vec![("file1", false, 0o644)]];
    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;

    let ctx = Context::default();

    // Lookup the file
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;

    // Try to write with an invalid handle
    let invalid_handle = 12345;
    let mut reader = TestContainer(b"Hello".to_vec());
    let result = fs.write(
        ctx,
        entry.inode,
        invalid_handle,
        &mut reader,
        5,
        0,
        None,
        false,
        false,
        0,
    );

    // Should fail with EBADF
    match result {
        Err(e) => {
            assert_eq!(e.raw_os_error(), Some(libc::EBADF));
        }
        Ok(_) => panic!("Expected error for invalid handle"),
    }

    Ok(())
}

#[test]
fn test_write_multiple_times() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing an empty file
    let layers = vec![vec![("file1", false, 0o644)]];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    let ctx = Context::default();

    // Lookup and open the file with write permissions
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;
    let (handle, _opts) = fs.open(ctx, entry.inode, (libc::O_WRONLY | libc::O_TRUNC) as u32)?;
    let handle = handle.unwrap();

    // Write content to the file in multiple operations
    let content1 = b"Hello, ";
    let mut reader1 = TestContainer(content1.to_vec());
    let bytes_written1 = fs.write(
        ctx,
        entry.inode,
        handle,
        &mut reader1,
        content1.len() as u32,
        0,
        None,
        false,
        false,
        0,
    )?;
    assert_eq!(bytes_written1, content1.len());

    let content2 = b"World!";
    let mut reader2 = TestContainer(content2.to_vec());
    let bytes_written2 = fs.write(
        ctx,
        entry.inode,
        handle,
        &mut reader2,
        content2.len() as u32,
        bytes_written1 as u64,
        None,
        false,
        false,
        0,
    )?;
    assert_eq!(bytes_written2, content2.len());

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle, false, false, None)?;

    // Verify the content was written correctly
    let file_content = std::fs::read(temp_dirs[0].path().join("file1"))?;
    assert_eq!(file_content, b"Hello, World!");

    Ok(())
}

#[test]
fn test_write_nested_directories() -> io::Result<()> {
    // Create an overlayfs with nested directories
    let layers = vec![vec![
        ("dir1", true, 0o755),
        ("dir1/dir2", true, 0o755),
        ("dir1/dir2/file1", false, 0o644),
    ]];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    let ctx = Context::default();

    // Lookup the nested directories and file
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;

    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(ctx, dir1_entry.inode, &dir2_name)?;

    let file_name = CString::new("file1").unwrap();
    let file_entry = fs.lookup(ctx, dir2_entry.inode, &file_name)?;

    // Open the file with write permissions
    let (handle, _opts) = fs.open(
        ctx,
        file_entry.inode,
        (libc::O_WRONLY | libc::O_TRUNC) as u32,
    )?;
    let handle = handle.unwrap();

    // Write content to the file
    let content = b"Nested file content";
    let mut reader = TestContainer(content.to_vec());
    let bytes_written = fs.write(
        ctx,
        file_entry.inode,
        handle,
        &mut reader,
        content.len() as u32,
        0,
        None,
        false,
        false,
        0,
    )?;
    assert_eq!(bytes_written, content.len());

    // Release the handle
    fs.release(ctx, file_entry.inode, 0, handle, false, false, None)?;

    // Verify the content was written correctly
    let file_path = temp_dirs[0].path().join("dir1").join("dir2").join("file1");
    let file_content = std::fs::read(file_path)?;
    assert_eq!(file_content, content);

    Ok(())
}

#[test]
fn test_write_with_whiteouts_and_opaque_dirs() -> io::Result<()> {
    // Create an overlayfs with multiple layers, whiteouts, and opaque directories
    let layers = vec![
        // Lower layer
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/file2", false, 0o644),
            ("file3", false, 0o644),
        ],
        // Upper layer with whiteout for file2 and opaque dir1
        vec![
            ("dir1", true, 0o755),
            ("dir1/.wh..wh..opq", false, 0o644), // Opaque dir marker
            ("dir1/file4", false, 0o644),        // New file in opaque dir
            (".wh.file3", false, 0o644),         // Whiteout for file3
        ],
    ];
    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;

    let ctx = Context::default();

    // Test 1: Write to file4 in opaque directory
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;

    let file4_name = CString::new("file4").unwrap();
    let file4_entry = fs.lookup(ctx, dir1_entry.inode, &file4_name)?;

    let (handle, _opts) = fs.open(ctx, file4_entry.inode, libc::O_WRONLY as u32)?;
    let handle = handle.unwrap();

    let content = b"File in opaque dir";
    let mut reader = TestContainer(content.to_vec());
    let bytes_written = fs.write(
        ctx,
        file4_entry.inode,
        handle,
        &mut reader,
        content.len() as u32,
        0,
        None,
        false,
        false,
        0,
    )?;
    assert_eq!(bytes_written, content.len());

    fs.release(ctx, file4_entry.inode, 0, handle, false, false, None)?;

    // Verify content
    let file_path = temp_dirs[1].path().join("dir1").join("file4");
    let file_content = std::fs::read(file_path)?;
    assert_eq!(file_content, content);

    // Test 2: Try to access file1 through opaque directory (should fail)
    let file1_name = CString::new("file1").unwrap();
    assert!(fs.lookup(ctx, dir1_entry.inode, &file1_name).is_err());

    // Test 3: Try to access file3 (should fail due to whiteout)
    let file3_name = CString::new("file3").unwrap();
    assert!(fs.lookup(ctx, 1, &file3_name).is_err());

    Ok(())
}
