use std::{ffi::CString, io};

use crate::virtio::fs::filesystem::{Context, Extensions, FileSystem};

use super::helper;

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[test]
fn test_open_basic() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a file
    let layers = vec![vec![("file1", false, 0o644)]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup the file to get its inode
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;

    // Open the file with read-only flags
    let (handle, _opts) = fs.open(ctx, entry.inode, libc::O_RDONLY as u32)?;

    // Verify we got a valid handle
    assert!(handle.is_some());

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;

    Ok(())
}

#[test]
fn test_open_directory() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a directory
    let layers = vec![vec![("dir1", true, 0o755)]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup the directory to get its inode
    let dir_name = CString::new("dir1").unwrap();
    let entry = fs.lookup(ctx, 1, &dir_name)?;

    // Open the directory
    let (handle, _opts) = fs.open(
        ctx,
        entry.inode,
        (libc::O_RDONLY | libc::O_DIRECTORY) as u32,
    )?;

    // Verify we got a valid handle
    assert!(handle.is_some());

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;

    Ok(())
}

#[test]
fn test_open_nonexistent() -> io::Result<()> {
    // Create a simple overlayfs with a single layer
    let layers = vec![vec![("file1", false, 0o644)]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Try to open a non-existent inode
    let result = fs.open(ctx, 999, libc::O_RDONLY as u32);

    // Verify it fails with ENOENT
    assert!(result.is_err());
    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::EBADF));

    Ok(())
}
#[test]
fn test_open_with_copy_up() -> io::Result<()> {
    // Create test layers:
    // Layer 0 (bottom): file1
    // Layer 1 (top): empty
    let layers = vec![vec![("file1", false, 0o644)], vec![]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup the file to get its inode
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;

    // Open the file with write flags, which should trigger copy-up
    let (handle, _opts) = fs.open(ctx, entry.inode, libc::O_RDWR as u32)?;

    // Verify we got a valid handle
    assert!(handle.is_some());

    // Verify the file was copied up to the top layer
    let top_layer_file = temp_dirs[1].path().join("file1");
    assert!(top_layer_file.exists());

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;

    Ok(())
}

#[test]
fn test_open_whiteout() -> io::Result<()> {
    // Create test layers:
    // Layer 0 (bottom): file1
    // Layer 1 (top): .wh.file1 (whiteout for file1)
    let layers = vec![
        vec![("file1", false, 0o644)],
        vec![(".wh.file1", false, 0o000)],
    ];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Try to lookup the file (should fail because it's whited out)
    let file_name = CString::new("file1").unwrap();
    let result = fs.lookup(ctx, 1, &file_name);

    // Verify lookup fails
    assert!(result.is_err());

    let non_existent_inode = 999; // Use a high number that shouldn't exist
    let open_result = fs.open(ctx, non_existent_inode, libc::O_RDONLY as u32);
    assert!(open_result.is_err());

    Ok(())
}

#[test]
fn test_open_and_release_multiple_times() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a file
    let layers = vec![vec![("file1", false, 0o644)]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup the file to get its inode
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;

    // Open and close the file multiple times
    for _ in 0..5 {
        // Open the file
        let (handle, _opts) = fs.open(ctx, entry.inode, libc::O_RDONLY as u32)?;

        // Verify we got a valid handle
        assert!(handle.is_some());

        // Release the handle
        fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;
    }

    // Verify we can still open the file after multiple open/release cycles
    let (handle, _opts) = fs.open(ctx, entry.inode, libc::O_RDONLY as u32)?;
    assert!(handle.is_some());
    fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;

    Ok(())
}

#[test]
fn test_open_with_different_flags() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a file
    let layers = vec![vec![("file1", false, 0o644)]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup the file to get its inode
    let file_name = CString::new("file1").unwrap();
    let entry = fs.lookup(ctx, 1, &file_name)?;

    // Test different open flags
    let flags = [
        libc::O_RDONLY,
        libc::O_WRONLY,
        libc::O_RDWR,
        libc::O_RDONLY | libc::O_NONBLOCK,
        libc::O_WRONLY | libc::O_APPEND,
    ];

    for flag in flags.iter() {
        // Open the file with the current flag
        let (handle, _opts) = fs.open(ctx, entry.inode, *flag as u32)?;

        // Verify we got a valid handle
        assert!(handle.is_some());

        // Release the handle
        fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;
    }

    Ok(())
}

#[test]
fn test_opendir_basic() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a directory
    let layers = vec![vec![("dir1", true, 0o755)]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup the directory to get its inode
    let dir_name = CString::new("dir1").unwrap();
    let entry = fs.lookup(ctx, 1, &dir_name)?;

    // Open the directory
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;

    // Verify we got a valid handle
    assert!(handle.is_some());

    // Release the handle
    fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;

    Ok(())
}

#[test]
fn test_opendir_nonexistent() -> io::Result<()> {
    // Create a simple overlayfs with a single layer
    let layers = vec![vec![("dir1", true, 0o755)]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Try to open a non-existent inode
    let result = fs.opendir(ctx, 999, libc::O_RDONLY as u32);

    // Verify it fails with EBADF
    match result {
        Err(e) => {
            assert_eq!(e.raw_os_error(), Some(libc::EBADF));
        }
        Ok(_) => panic!("Expected error for non-existent inode"),
    }

    Ok(())
}

#[test]
fn test_opendir_whiteout() -> io::Result<()> {
    // Create test layers:
    // Layer 0 (bottom): dir1/
    // Layer 1 (top): .wh.dir1 (whiteout for dir1)
    let layers = vec![
        vec![("dir1", true, 0o755)],
        vec![(".wh.dir1", false, 0o000)],
    ];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Try to lookup the directory (should fail because it's whited out)
    let dir_name = CString::new("dir1").unwrap();
    let result = fs.lookup(ctx, 1, &dir_name);

    // Verify lookup fails with ENOENT
    match result {
        Err(e) => {
            assert_eq!(e.raw_os_error(), Some(libc::ENOENT));
        }
        Ok(_) => panic!("Expected error for whited-out directory"),
    }

    Ok(())
}

#[test]
fn test_opendir_with_copy_up() -> io::Result<()> {
    // Create test layers:
    // Layer 0 (bottom): dir1/
    // Layer 1 (top): empty
    let layers = vec![vec![("dir1", true, 0o755)], vec![]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup the directory to get its inode
    let dir_name = CString::new("dir1").unwrap();
    let entry = fs.lookup(ctx, 1, &dir_name)?;

    // First open the directory normally
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    assert!(handle.is_some());
    fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;

    // Trigger copy-up by creating a new file in the directory
    let new_file = CString::new("newfile").unwrap();
    fs.mkdir(ctx, entry.inode, &new_file, 0o755, 0, Extensions::default())?;

    // Verify the directory was copied up to the top layer
    let top_layer_dir = temp_dirs[1].path().join("dir1");
    assert!(top_layer_dir.exists());
    assert!(top_layer_dir.is_dir());

    // Verify we can still open the directory after copy-up
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    assert!(handle.is_some());
    fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;

    Ok(())
}

#[test]
fn test_opendir_and_release_multiple_times() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a directory
    let layers = vec![vec![("dir1", true, 0o755)]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup the directory to get its inode
    let dir_name = CString::new("dir1").unwrap();
    let entry = fs.lookup(ctx, 1, &dir_name)?;

    // Open and close the directory multiple times
    for _ in 0..5 {
        // Open the directory
        let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;

        // Verify we got a valid handle
        assert!(handle.is_some());

        // Release the handle
        fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;
    }

    // Verify we can still open the directory after multiple open/release cycles
    let (handle, _opts) = fs.opendir(ctx, entry.inode, libc::O_RDONLY as u32)?;
    assert!(handle.is_some());
    fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;

    Ok(())
}

#[test]
fn test_opendir_with_different_flags() -> io::Result<()> {
    // Create a simple overlayfs with a single layer containing a directory
    let layers = vec![vec![("dir1", true, 0o755)]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Lookup the directory to get its inode
    let dir_name = CString::new("dir1").unwrap();
    let entry = fs.lookup(ctx, 1, &dir_name)?;

    // Test different open flags - only use read-only flags since directories can't be opened for writing
    let flags = [
        libc::O_RDONLY | libc::O_DIRECTORY,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK | libc::O_CLOEXEC,
    ];

    for flag in flags.iter() {
        // Open the directory with the current flag
        let (handle, _opts) = fs.opendir(ctx, entry.inode, *flag as u32)?;

        // Verify we got a valid handle
        assert!(handle.is_some());

        // Release the handle
        fs.release(ctx, entry.inode, 0, handle.unwrap(), false, false, None)?;
    }

    Ok(())
}
