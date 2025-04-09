use std::{ffi::CString, fs, io, os::unix::fs::PermissionsExt, path::PathBuf};

use tempfile::TempDir;

use crate::virtio::{fs::filesystem::{Context, FileSystem}, fuse::FsOptions, overlayfs::{Config, OverlayFs}, };

use super::helper;

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[test]
fn test_copy_up_complex() -> io::Result<()> {
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
    //   (empty - will be populated by copy_up operations)
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

    // Perform copy_up
    fs.copy_up(&path_inodes)?;

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

    // Copy up the nested directory
    fs.copy_up(&nested_path_inodes)?;

    // Verify the directory structure was copied
    let top_nested_path = temp_dirs[2].path().join("dir3").join("nested");
    assert!(top_nested_path.exists());
    assert!(top_nested_path.is_dir());
    let metadata = fs::metadata(&top_nested_path)?;
    assert_eq!(metadata.permissions().mode() & 0o777, 0o755);

    // Test 3: Copy up a file from the middle layer
    let middle_file_name = CString::new("middle_file").unwrap();
    let (_, middle_file_path_inodes) = fs.do_lookup(dir3_entry.inode, &middle_file_name)?;

    // Perform copy_up
    fs.copy_up(&middle_file_path_inodes)?;

    // Verify the file was copied up correctly
    let top_middle_file_path = temp_dirs[2].path().join("dir3").join("middle_file");
    let metadata = fs::metadata(&top_middle_file_path)?;
    assert_eq!(metadata.permissions().mode() & 0o777, 0o666);
    assert!(top_middle_file_path.exists());

    // Test 4: Copy up a nested file
    let data_name = CString::new("data").unwrap();
    let (_, data_path_inodes) = fs.do_lookup(nested_entry.inode, &data_name)?;

    // Perform copy_up
    fs.copy_up(&data_path_inodes)?;

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

    // Perform copy_up
    fs.copy_up(&file2_path_inodes)?;

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

    // Perform copy_up
    fs.copy_up(&symlink_path_inodes)?;

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
fn test_copy_up_with_content() -> io::Result<()> {
    // Create test layers with files containing specific content:
    // Layer 0 (bottom):
    //   - file1 (contains "bottom layer content")
    //   - dir1/nested_file1 (contains "nested bottom content")
    // Layer 1 (middle):
    //   - file2 (contains "middle layer content")
    //   - dir1/nested_file2 (contains "nested middle content")
    // Layer 2 (top):
    //   - file3 (contains "top layer content")
    //   - dir1/nested_file3 (contains "nested top content")

    // Create temporary directories for each layer
    let temp_dirs: Vec<TempDir> = vec![
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
        TempDir::new().unwrap(),
    ];

    // Create directory structure in each layer
    for dir in &temp_dirs {
        fs::create_dir_all(dir.path().join("dir1"))?;
    }

    // Create files with content in bottom layer
    fs::write(temp_dirs[0].path().join("file1"), "bottom layer content")?;
    fs::write(
        temp_dirs[0].path().join("dir1").join("nested_file1"),
        "nested bottom content",
    )?;

    // Create files with content in middle layer
    fs::write(temp_dirs[1].path().join("file2"), "middle layer content")?;
    fs::write(
        temp_dirs[1].path().join("dir1").join("nested_file2"),
        "nested middle content",
    )?;

    // Create files with content in top layer
    fs::write(temp_dirs[2].path().join("file3"), "top layer content")?;
    fs::write(
        temp_dirs[2].path().join("dir1").join("nested_file3"),
        "nested top content",
    )?;

    // Set permissions
    for dir in &temp_dirs {
        fs::set_permissions(dir.path().join("dir1"), fs::Permissions::from_mode(0o755)).ok();
    }
    fs::set_permissions(
        temp_dirs[0].path().join("file1"),
        fs::Permissions::from_mode(0o644),
    )
    .ok();
    fs::set_permissions(
        temp_dirs[0].path().join("dir1").join("nested_file1"),
        fs::Permissions::from_mode(0o644),
    )
    .ok();
    fs::set_permissions(
        temp_dirs[1].path().join("file2"),
        fs::Permissions::from_mode(0o644),
    )
    .ok();
    fs::set_permissions(
        temp_dirs[1].path().join("dir1").join("nested_file2"),
        fs::Permissions::from_mode(0o644),
    )
    .ok();
    fs::set_permissions(
        temp_dirs[2].path().join("file3"),
        fs::Permissions::from_mode(0o644),
    )
    .ok();
    fs::set_permissions(
        temp_dirs[2].path().join("dir1").join("nested_file3"),
        fs::Permissions::from_mode(0o644),
    )
    .ok();

    // Create layer paths
    let layer_paths: Vec<PathBuf> = temp_dirs.iter().map(|d| d.path().to_path_buf()).collect();

    // Create the overlayfs
    let cfg = Config {
        layers: layer_paths,
        ..Default::default()
    };
    let fs = OverlayFs::new(cfg)?;
    let ctx = Context::default();

    // Test 1: Open file1 from bottom layer with write access (should trigger copy-up)
    let file1_name = CString::new("file1").unwrap();
    let (_, path_inodes) = fs.do_lookup(1, &file1_name)?;
    fs.copy_up(&path_inodes)?;

    // Verify file1 was copied up to the top layer with correct content
    let top_file1 = temp_dirs[2].path().join("file1");
    assert!(top_file1.exists());
    let content = fs::read_to_string(&top_file1)?;
    assert_eq!(content, "bottom layer content");

    // Test 2: Open nested_file1 from bottom layer with write access
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;
    let nested_file1_name = CString::new("nested_file1").unwrap();
    let (_, path_inodes) = fs.do_lookup(dir1_entry.inode, &nested_file1_name)?;
    fs.copy_up(&path_inodes)?;

    // Verify nested_file1 was copied up to the top layer with correct content
    let top_nested_file1 = temp_dirs[2].path().join("dir1").join("nested_file1");
    assert!(top_nested_file1.exists());
    let content = fs::read_to_string(&top_nested_file1)?;
    assert_eq!(content, "nested bottom content");

    // Test 3: Open file2 from middle layer with write access
    let file2_name = CString::new("file2").unwrap();
    let (_, path_inodes) = fs.do_lookup(1, &file2_name)?;
    fs.copy_up(&path_inodes)?;

    // Verify file2 was copied up to the top layer with correct content
    let top_file2 = temp_dirs[2].path().join("file2");
    assert!(top_file2.exists());
    let content = fs::read_to_string(&top_file2)?;
    assert_eq!(content, "middle layer content");

    // Test 4: Open file3 from top layer (no copy-up needed)
    let file3_name = CString::new("file3").unwrap();
    let (_, path_inodes) = fs.do_lookup(1, &file3_name)?;
    fs.copy_up(&path_inodes)?;

    // Verify file3 content is unchanged
    let content = fs::read_to_string(temp_dirs[2].path().join("file3"))?;
    assert_eq!(content, "top layer content");

    // Clean up
    fs.destroy();

    Ok(())
}

#[test]
fn test_link_basic() -> io::Result<()> {
    // Create test layers with simple structure:
    // Layer 0 (bottom):
    //   - file1
    // Layer 1 (top):
    //   - dir1/
    let layers = vec![vec![("file1", false, 0o644)], vec![("dir1", true, 0o755)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Create hard link from file1 to dir1/link1
    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(ctx, 1, &file1_name)?;

    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;

    let link1_name = CString::new("link1").unwrap();
    let link1_entry = fs.link(ctx, file1_entry.inode, dir1_entry.inode, &link1_name)?;

    // Verify the link was created
    let top_layer = temp_dirs.last().unwrap().path();
    assert!(top_layer.join("dir1/link1").exists());

    // Verify the link has the same inode number as the original file
    let updated_file1_entry = fs.lookup(ctx, 1, &file1_name)?;
    assert_eq!(link1_entry.attr.st_ino, updated_file1_entry.attr.st_ino);
    assert_eq!(link1_entry.attr.st_nlink, updated_file1_entry.attr.st_nlink);

    Ok(())
}

#[test]
fn test_link_multiple_layers() -> io::Result<()> {
    // Create test layers with multiple files:
    // Layer 0 (bottom):
    //   - file1
    //   - dir1/
    //   - dir1/file2
    // Layer 1 (middle):
    //   - file3
    // Layer 2 (top):
    //   - dir2/
    let layers = vec![
        vec![
            ("file1", false, 0o644),
            ("dir1", true, 0o755),
            ("dir1/file2", false, 0o644),
        ],
        vec![("file3", false, 0o644)],
        vec![("dir2", true, 0o755)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Create links to files from different layers
    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(ctx, 1, &file1_name)?;

    let file3_name = CString::new("file3").unwrap();
    let file3_entry = fs.lookup(ctx, 1, &file3_name)?;

    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(ctx, 1, &dir2_name)?;

    // Create links in top layer
    let link1_name = CString::new("link1").unwrap();
    let link2_name = CString::new("link2").unwrap();

    let link1_entry = fs.link(ctx, file1_entry.inode, dir2_entry.inode, &link1_name)?;
    let link2_entry = fs.link(ctx, file3_entry.inode, dir2_entry.inode, &link2_name)?;

    // Verify the links were created in the top layer
    let top_layer = temp_dirs.last().unwrap().path();
    assert!(top_layer.join("dir2/link1").exists());
    assert!(top_layer.join("dir2/link2").exists());

    // Verify source files were copied up
    assert!(top_layer.join("file1").exists());
    assert!(top_layer.join("file3").exists());

    // Verify link attributes
    let updated_file1_entry = fs.lookup(ctx, 1, &file1_name)?;
    let updated_file3_entry = fs.lookup(ctx, 1, &file3_name)?;
    assert_eq!(link1_entry.attr.st_ino, updated_file1_entry.attr.st_ino);
    assert_eq!(link2_entry.attr.st_ino, updated_file3_entry.attr.st_ino);

    Ok(())
}

#[test]
fn test_link_errors() -> io::Result<()> {
    // Create test layers:
    // Layer 0 (bottom):
    //   - file1
    //   - dir1/
    let layers = vec![vec![("file1", false, 0o644), ("dir1", true, 0o755)]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(ctx, 1, &file1_name)?;

    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;

    // Test linking to invalid parent
    let invalid_name = CString::new("link1").unwrap();
    assert!(fs
        .link(ctx, file1_entry.inode, 999999, &invalid_name)
        .is_err());

    // Test linking with invalid source inode
    assert!(fs
        .link(ctx, 999999, dir1_entry.inode, &invalid_name)
        .is_err());

    // Test linking with invalid name
    let invalid_name = CString::new("../link1").unwrap();
    assert!(fs
        .link(ctx, file1_entry.inode, dir1_entry.inode, &invalid_name)
        .is_err());

    Ok(())
}

#[test]
fn test_link_nested() -> io::Result<()> {
    // Create test layers with nested structure:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/file1
    //   - dir1/subdir/
    //   - dir1/subdir/file2
    // Layer 1 (top):
    //   - dir2/
    //   - dir2/subdir/
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/file2", false, 0o644),
        ],
        vec![("dir2", true, 0o755), ("dir2/subdir", true, 0o755)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    // Create links to nested files
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;

    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(ctx, dir1_entry.inode, &file1_name)?;

    let subdir_name = CString::new("subdir").unwrap();
    let subdir_entry = fs.lookup(ctx, dir1_entry.inode, &subdir_name)?;

    let file2_name = CString::new("file2").unwrap();
    let file2_entry = fs.lookup(ctx, subdir_entry.inode, &file2_name)?;

    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(ctx, 1, &dir2_name)?;

    let dir2_subdir_entry = fs.lookup(ctx, dir2_entry.inode, &subdir_name)?;

    // Create links in different locations
    let link1_name = CString::new("link1").unwrap();
    let link2_name = CString::new("link2").unwrap();

    let link1_entry = fs.link(ctx, file1_entry.inode, dir2_entry.inode, &link1_name)?;
    let link2_entry = fs.link(ctx, file2_entry.inode, dir2_subdir_entry.inode, &link2_name)?;

    // Verify the links were created
    let top_layer = temp_dirs.last().unwrap().path();
    assert!(top_layer.join("dir2/link1").exists());
    assert!(top_layer.join("dir2/subdir/link2").exists());

    // Verify source files were copied up
    assert!(top_layer.join("dir1/file1").exists());
    assert!(top_layer.join("dir1/subdir/file2").exists());

    // Verify link attributes
    let updated_file1_entry = fs.lookup(ctx, dir1_entry.inode, &file1_name)?;
    let updated_file2_entry = fs.lookup(ctx, subdir_entry.inode, &file2_name)?;
    assert_eq!(link1_entry.attr.st_ino, updated_file1_entry.attr.st_ino);
    assert_eq!(link2_entry.attr.st_ino, updated_file2_entry.attr.st_ino);

    Ok(())
}

#[test]
fn test_link_existing_name() -> io::Result<()> {
    // Create test layers:
    // Layer 0 (bottom):
    //   - file1
    //   - dir1/
    //   - dir1/existing
    let layers = vec![vec![
        ("file1", false, 0o644),
        ("dir1", true, 0o755),
        ("dir1/existing", false, 0o644),
    ]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;
    let ctx = Context::default();

    let file1_name = CString::new("file1").unwrap();
    let file1_entry = fs.lookup(ctx, 1, &file1_name)?;

    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;

    // Try to create a link with an existing name
    let existing_name = CString::new("existing").unwrap();
    assert!(fs
        .link(ctx, file1_entry.inode, dir1_entry.inode, &existing_name)
        .is_err());

    Ok(())
}
