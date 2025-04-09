use std::{ffi::CString, io};

use crate::virtio::fs::filesystem::{Context, FileSystem};

use super::helper;

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

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

    // Test: Try to remove a file using rmdir
    let file_name = CString::new("file1.txt").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir_name)?;
    match fs.rmdir(ctx, dir1_entry.inode, &file_name) {
        Ok(_) => panic!("rmdir succeeded on a file"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::ENOTDIR)),
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
