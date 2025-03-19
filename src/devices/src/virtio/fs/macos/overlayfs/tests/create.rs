use std::{
    ffi::CString,
    fs::{self, FileType},
    io,
    os::unix::fs::FileTypeExt,
    path::Path,
};

use crate::virtio::{
    bindings,
    fs::filesystem::{Context, Extensions, FileSystem},
    fuse::FsOptions,
};

use super::helper;

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

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
fn test_symlink_basic() -> io::Result<()> {
    // Create test layers:
    // Single layer with a file
    let layers = vec![vec![("target_file", false, 0o644)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Create a new symlink
    let link_name = CString::new("link").unwrap();
    let target_name = CString::new("target_file").unwrap();
    let ctx = Context::default();
    let entry = fs.symlink(ctx, &target_name, 1, &link_name, Extensions::default())?;

    // Verify the symlink was created with correct mode
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFLNK);
    assert_eq!(entry.attr.st_mode & 0o777, 0o777); // Symlinks are typically 0777

    // Verify we can look it up
    let lookup_entry = fs.lookup(ctx, 1, &link_name)?;
    assert_eq!(lookup_entry.attr.st_mode & libc::S_IFMT, libc::S_IFLNK);

    // Verify the symlink exists on disk in the top layer
    let link_path = temp_dirs.last().unwrap().path().join("link");
    assert!(link_path.exists());
    assert!(link_path.is_symlink());

    // Verify the symlink points to the correct target
    let target = fs.readlink(ctx, lookup_entry.inode)?;
    assert_eq!(target, target_name.to_bytes());

    Ok(())
}

#[test]
fn test_symlink_nested() -> io::Result<()> {
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
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
            ("dir1/subdir/bottom_file", false, 0o644),
        ],
        vec![("dir2", true, 0o755), ("dir2/file2", false, 0o644)],
        vec![("dir3", true, 0o755), ("dir3/top_file", false, 0o644)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Test 1: Create symlink in dir1 (should trigger copy-up)
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;
    let link_name = CString::new("link_to_file1").unwrap();
    let target_name = CString::new("file1").unwrap();
    let link_entry = fs.symlink(
        ctx,
        &target_name,
        dir1_entry.inode,
        &link_name,
        Extensions::default(),
    )?;
    assert_eq!(link_entry.attr.st_mode & libc::S_IFMT, libc::S_IFLNK);

    // Test 2: Create symlink in dir2 (middle layer, should trigger copy-up)
    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(ctx, 1, &dir2_name)?;
    let middle_link_name = CString::new("link_to_file2").unwrap();
    let middle_target = CString::new("file2").unwrap();
    let middle_link_entry = fs.symlink(
        ctx,
        &middle_target,
        dir2_entry.inode,
        &middle_link_name,
        Extensions::default(),
    )?;
    assert_eq!(middle_link_entry.attr.st_mode & libc::S_IFMT, libc::S_IFLNK);

    // Test 3: Create symlink in dir3 (top layer, no copy-up needed)
    let dir3_name = CString::new("dir3").unwrap();
    let dir3_entry = fs.lookup(ctx, 1, &dir3_name)?;
    let top_link_name = CString::new("link_to_top_file").unwrap();
    let top_target = CString::new("top_file").unwrap();
    let top_link_entry = fs.symlink(
        ctx,
        &top_target,
        dir3_entry.inode,
        &top_link_name,
        Extensions::default(),
    )?;
    assert_eq!(top_link_entry.attr.st_mode & libc::S_IFMT, libc::S_IFLNK);

    // Verify all symlinks exist in appropriate layers
    let top_layer = temp_dirs.last().unwrap().path();
    assert!(fs::symlink_metadata(top_layer.join("dir1/link_to_file1")).is_ok());
    assert!(fs::symlink_metadata(top_layer.join("dir2/link_to_file2")).is_ok());
    assert!(fs::symlink_metadata(top_layer.join("dir3/link_to_top_file")).is_ok());

    // Verify symlink targets
    let link1_target = fs.readlink(ctx, link_entry.inode)?;
    assert_eq!(link1_target, target_name.to_bytes());

    let link2_target = fs.readlink(ctx, middle_link_entry.inode)?;
    assert_eq!(link2_target, middle_target.to_bytes());

    let link3_target = fs.readlink(ctx, top_link_entry.inode)?;
    assert_eq!(link3_target, top_target.to_bytes());

    Ok(())
}

#[test]
fn test_symlink_existing_name() -> io::Result<()> {
    // Create test layers with a file and directory
    let layers = vec![vec![
        ("target_file", false, 0o644),
        ("existing_name", false, 0o644),
    ]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();
    let link_name = CString::new("existing_name").unwrap();
    let target_name = CString::new("target_file").unwrap();

    // Try to create a symlink with an existing name
    match fs.symlink(ctx, &target_name, 1, &link_name, Extensions::default()) {
        Ok(_) => panic!("Expected error when creating symlink with existing name"),
        Err(e) => assert_eq!(e.kind(), io::ErrorKind::AlreadyExists),
    }

    Ok(())
}

#[test]
fn test_symlink_multiple_layers() -> io::Result<()> {
    // Create test layers:
    // Layer 0 (bottom): base files
    // Layer 1 (middle): some files
    // Layer 2 (top): more files
    let layers = vec![
        vec![
            ("bottom_dir", true, 0o755),
            ("bottom_dir/target1", false, 0o644),
        ],
        vec![
            ("middle_dir", true, 0o755),
            ("middle_dir/target2", false, 0o644),
        ],
        vec![("top_dir", true, 0o755), ("top_dir/target3", false, 0o644)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Create symlinks to files in different layers
    let test_cases = vec![
        ("link_to_bottom", "bottom_dir/target1"),
        ("link_to_middle", "middle_dir/target2"),
        ("link_to_top", "top_dir/target3"),
    ];

    for (link, target) in test_cases.clone() {
        let link_name = CString::new(link).unwrap();
        let target_name = CString::new(target).unwrap();

        let entry = fs.symlink(ctx, &target_name, 1, &link_name, Extensions::default())?;
        assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFLNK);

        // Verify symlink target
        let target_bytes = fs.readlink(ctx, entry.inode)?;
        assert_eq!(target_bytes, target_name.to_bytes());
    }

    // Verify all symlinks exist in the top layer
    let top_layer = temp_dirs.last().unwrap().path();
    for (link, _) in test_cases {
        assert!(fs::symlink_metadata(top_layer.join(link)).is_ok());
    }

    Ok(())
}

#[test]
fn test_symlink_invalid_name() -> io::Result<()> {
    // Create a simple test layer
    let layers = vec![vec![("target_file", false, 0o644)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();
    let target_name = CString::new("target_file").unwrap();

    // Test cases with invalid names
    let invalid_names = vec![
        "..",           // Path traversal attempt
        "invalid/name", // Contains slash
        ".wh.name",     // Contains whiteout prefix
        ".wh..wh..opq", // Opaque directory marker
    ];

    for name in invalid_names {
        let link_name = CString::new(name).unwrap();
        match fs.symlink(ctx, &target_name, 1, &link_name, Extensions::default()) {
            Ok(_) => panic!("Expected error for invalid name: {}", name),
            Err(e) => {
                assert!(
                    e.kind() == io::ErrorKind::InvalidInput
                        || e.kind() == io::ErrorKind::PermissionDenied,
                    "Unexpected error kind for name {}: {:?}",
                    name,
                    e.kind()
                );
            }
        }
    }

    Ok(())
}

#[test]
fn test_rename_basic() -> io::Result<()> {
    // Create test layers
    let files = vec![("file1.txt", false, 0o644), ("file2.txt", false, 0o644)];
    let layers = vec![files];
    let (overlayfs, _temp_dirs) = helper::create_overlayfs(layers)?;

    // Lookup source and destination parents (root in this case)
    let root = 1;
    let old_name = CString::new("file1.txt")?;
    let new_name = CString::new("renamed.txt")?;

    // Perform rename
    overlayfs.rename(Context::default(), root, &old_name, root, &new_name, 0)?;

    // Verify old name doesn't exist
    assert!(overlayfs
        .lookup(Context::default(), root, &old_name)
        .is_err());

    // Verify new name exists
    let entry = overlayfs.lookup(Context::default(), root, &new_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    Ok(())
}

#[test]
fn test_rename_whiteout() -> io::Result<()> {
    // Create test layers with file in lower layer
    let lower_files = vec![("file1.txt", false, 0o644)];
    let upper_files = vec![];
    let layers = vec![lower_files, upper_files];
    let (overlayfs, _temp_dirs) = helper::create_overlayfs(layers)?;

    let root = 1;
    let old_name = CString::new("file1.txt")?;
    let new_name = CString::new("renamed.txt")?;

    // Rename file from lower layer
    overlayfs.rename(Context::default(), root, &old_name, root, &new_name, 0)?;

    // Verify old name is whited out
    assert!(overlayfs
        .lookup(Context::default(), root, &old_name)
        .is_err());

    // Verify new name exists in upper layer
    let entry = overlayfs.lookup(Context::default(), root, &new_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    Ok(())
}

#[test]
fn test_rename_multiple_layers() -> io::Result<()> {
    // Create test layers
    let lower_files = vec![("file1.txt", false, 0o644), ("file2.txt", false, 0o644)];
    let middle_files = vec![("file3.txt", false, 0o644)];
    let upper_files = vec![("file4.txt", false, 0o644)];
    let layers = vec![lower_files, middle_files, upper_files];
    let (overlayfs, _temp_dirs) = helper::create_overlayfs(layers)?;

    let root = 1;
    let old_name = CString::new("file1.txt")?;
    let new_name = CString::new("renamed.txt")?;

    // Rename file from lowest layer
    overlayfs.rename(Context::default(), root, &old_name, root, &new_name, 0)?;

    // Verify old name is whited out
    assert!(overlayfs
        .lookup(Context::default(), root, &old_name)
        .is_err());

    // Verify new name exists in upper layer
    let entry = overlayfs.lookup(Context::default(), root, &new_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    Ok(())
}

#[test]
fn test_rename_errors() -> io::Result<()> {
    // Create test layers
    let files = vec![
        ("dir1", true, 0o755),
        ("dir1/file1.txt", false, 0o644),
        ("file2.txt", false, 0o644),
    ];
    let layers = vec![files];
    let (overlayfs, _temp_dirs) = helper::create_overlayfs(layers)?;

    let root = 1;
    let dir1_name = CString::new("dir1")?;
    let _ = overlayfs.lookup(Context::default(), root, &dir1_name)?;

    // Test renaming non-existent file
    let nonexistent = CString::new("nonexistent.txt")?;
    let new_name = CString::new("renamed.txt")?;
    assert!(overlayfs
        .rename(Context::default(), root, &nonexistent, root, &new_name, 0,)
        .is_err());

    // Test renaming to invalid parent
    let file2_name = CString::new("file2.txt")?;
    let invalid_parent = 99999;
    assert!(overlayfs
        .rename(
            Context::default(),
            root,
            &file2_name,
            invalid_parent,
            &new_name,
            0,
        )
        .is_err());

    // Test renaming directory to non-empty directory
    let _ = CString::new("dir1_new")?;
    assert!(overlayfs
        .rename(Context::default(), root, &dir1_name, root, &file2_name, 0,)
        .is_err());

    Ok(())
}

#[test]
fn test_rename_whiteout_flag() -> io::Result<()> {
    // Create test layers with file in lower layer
    let lower_files = vec![("file1.txt", false, 0o644)];
    let upper_files = vec![];
    let layers = vec![lower_files, upper_files];
    let (overlayfs, temp_dirs) = helper::create_overlayfs(layers)?;

    let root = 1;
    let old_name = CString::new("file1.txt")?;
    let new_name = CString::new("renamed.txt")?;

    // Use the whiteout flag
    let flags = bindings::LINUX_RENAME_WHITEOUT;
    overlayfs.rename(
        Context::default(),
        root,
        &old_name,
        root,
        &new_name,
        flags as u32,
    )?;

    // Verify that lookup for the old name fails
    assert!(overlayfs
        .lookup(Context::default(), root, &old_name)
        .is_err());

    // Verify new name exists
    let entry = overlayfs.lookup(Context::default(), root, &new_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Check that a whiteout file is created in the top layer
    let top_layer = temp_dirs.last().unwrap().path();
    // For root parent, the whiteout should be at the top layer root with prefix '.wh.'
    let whiteout_path = top_layer.join(".wh.file1.txt");
    let meta = fs::metadata(&whiteout_path)?;
    // Updated check: expect a regular file with mode 0o600
    assert!(
        meta.file_type().is_file(),
        "Expected whiteout to be a regular file"
    );

    Ok(())
}

#[test]
fn test_rename_nested_files() -> io::Result<()> {
    // Create test layers with nested structure
    let files = vec![
        ("dir1", true, 0o755),
        ("dir1/file1.txt", false, 0o644),
        ("dir2", true, 0o755),
    ];
    let (overlayfs, _temp_dirs) = helper::create_overlayfs(vec![files])?;

    let root = 1;
    let dir1_name = CString::new("dir1")?;
    let dir2_name = CString::new("dir2")?;

    // Lookup directory inodes
    let dir1_entry = overlayfs.lookup(Context::default(), root, &dir1_name)?;
    let dir2_entry = overlayfs.lookup(Context::default(), root, &dir2_name)?;

    let old_name = CString::new("file1.txt")?;
    let new_name = CString::new("renamed.txt")?;

    // Rename file between directories
    overlayfs.rename(
        Context::default(),
        dir1_entry.inode,
        &old_name,
        dir2_entry.inode,
        &new_name,
        0,
    )?;

    // Verify old location is empty
    assert!(overlayfs
        .lookup(Context::default(), dir1_entry.inode, &old_name)
        .is_err());

    // Verify new location has the file
    let entry = overlayfs.lookup(Context::default(), dir2_entry.inode, &new_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    Ok(())
}

#[test]
fn test_rename_complex_layers() -> io::Result<()> {
    // Create test layers with complex structure
    let lower_files = vec![
        ("dir1", true, 0o755),
        ("dir1/file1.txt", false, 0o644),
        ("dir2", true, 0o755),
        ("dir2/file2.txt", false, 0o644),
    ];
    let middle_files = vec![("dir3", true, 0o755), ("dir3/file3.txt", false, 0o644)];
    let upper_files = vec![("dir4", true, 0o755), ("dir4/file4.txt", false, 0o644)];
    let layers = vec![lower_files, middle_files, upper_files];
    let (overlayfs, temp_dirs) = helper::create_overlayfs(layers)?;

    let root = 1;

    // Test renaming between different layer directories
    let dir1_name = CString::new("dir1")?;
    let dir4_name = CString::new("dir4")?;
    let dir1_entry = overlayfs.lookup(Context::default(), root, &dir1_name)?;
    let dir4_entry = overlayfs.lookup(Context::default(), root, &dir4_name)?;

    let old_name = CString::new("file1.txt")?;
    let new_name = CString::new("renamed.txt")?;

    // Rename from lower to upper layer directory
    overlayfs.rename(
        Context::default(),
        dir1_entry.inode,
        &old_name,
        dir4_entry.inode,
        &new_name,
        0,
    )?;

    // Verify file moved correctly
    assert!(overlayfs
        .lookup(Context::default(), dir1_entry.inode, &old_name)
        .is_err());
    let entry = overlayfs.lookup(Context::default(), dir4_entry.inode, &new_name)?;
    assert_eq!(entry.attr.st_mode & libc::S_IFMT, libc::S_IFREG);

    // Check whiteout file in the old parent's directory (dir1) in the top layer
    let top_layer = temp_dirs.last().unwrap().path();
    let whiteout_path = top_layer.join("dir1").join(".wh.file1.txt");
    assert!(
        fs::metadata(&whiteout_path).is_ok(),
        "Expected whiteout file at {:?}",
        whiteout_path
    );

    Ok(())
}

#[test]
fn test_create_basic() -> io::Result<()> {
    // Create test layers:
    // Single layer with a directory
    let layers = vec![vec![("dir1", true, 0o755)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    // Create a new file in root
    let file_name = CString::new("new_file.txt").unwrap();
    let ctx = Context::default();
    let (entry, handle, _) =
        fs.create(ctx, 1, &file_name, 0o644, 0, 0o022, Extensions::default())?;

    // Verify the file was created with correct mode
    let entry_mode = entry.attr.st_mode as u32;
    assert_eq!(entry_mode & libc::S_IFMT as u32, libc::S_IFREG as u32);
    assert_eq!(entry_mode & 0o777, 0o644 & !0o022);

    // Verify we can look it up
    let lookup_entry = fs.lookup(ctx, 1, &file_name)?;
    let lookup_mode = lookup_entry.attr.st_mode as u32;
    assert_eq!(lookup_mode & libc::S_IFMT as u32, libc::S_IFREG as u32);

    // Verify the file exists on disk in the top layer
    let file_path = temp_dirs.last().unwrap().path().join("new_file.txt");
    assert!(file_path.exists());
    assert!(file_path.is_file());

    // If we got a handle, release it
    if let Some(h) = handle {
        fs.release(ctx, entry.inode, 0, h, false, false, None)?;
    }

    Ok(())
}

#[test]
fn test_create_nested() -> io::Result<()> {
    // Create test layers with complex structure:
    // Layer 0 (bottom):
    //   - dir1/
    //   - dir1/file1
    //   - dir1/subdir/
    // Layer 1 (middle):
    //   - dir2/
    //   - dir2/file2
    // Layer 2 (top):
    //   - dir3/
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
        ],
        vec![("dir2", true, 0o755), ("dir2/file2", false, 0o644)],
        vec![("dir3", true, 0o755)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Test 1: Create file in dir1 (should trigger copy-up)
    let dir1_name = CString::new("dir1").unwrap();
    let dir1_entry = fs.lookup(ctx, 1, &dir1_name)?;
    let file_name = CString::new("new_file.txt").unwrap();
    let (entry, handle, _) = fs.create(
        ctx,
        dir1_entry.inode,
        &file_name,
        0o644,
        0,
        0o022,
        Extensions::default(),
    )?;
    let entry_mode = entry.attr.st_mode as u32;
    assert_eq!(entry_mode & libc::S_IFMT as u32, libc::S_IFREG as u32);

    // Test 2: Create file in dir2 (middle layer, should trigger copy-up)
    let dir2_name = CString::new("dir2").unwrap();
    let dir2_entry = fs.lookup(ctx, 1, &dir2_name)?;
    let middle_file_name = CString::new("middle_file.txt").unwrap();
    let (middle_entry, middle_handle, _) = fs.create(
        ctx,
        dir2_entry.inode,
        &middle_file_name,
        0o644,
        0,
        0o022,
        Extensions::default(),
    )?;
    let middle_mode = middle_entry.attr.st_mode as u32;
    assert_eq!(middle_mode & libc::S_IFMT as u32, libc::S_IFREG as u32);

    // Test 3: Create file in dir3 (top layer, no copy-up needed)
    let dir3_name = CString::new("dir3").unwrap();
    let dir3_entry = fs.lookup(ctx, 1, &dir3_name)?;
    let top_file_name = CString::new("top_file.txt").unwrap();
    let (top_entry, top_handle, _) = fs.create(
        ctx,
        dir3_entry.inode,
        &top_file_name,
        0o644,
        0,
        0o022,
        Extensions::default(),
    )?;
    let top_mode = top_entry.attr.st_mode as u32;
    assert_eq!(top_mode & libc::S_IFMT as u32, libc::S_IFREG as u32);

    // Verify all files exist in appropriate layers
    let top_layer = temp_dirs.last().unwrap().path();
    assert!(top_layer.join("dir1/new_file.txt").exists());
    assert!(top_layer.join("dir2/middle_file.txt").exists());
    assert!(top_layer.join("dir3/top_file.txt").exists());

    // Release handles
    if let Some(h) = handle {
        fs.release(ctx, entry.inode, 0, h, false, false, None)?;
    }
    if let Some(h) = middle_handle {
        fs.release(ctx, middle_entry.inode, 0, h, false, false, None)?;
    }
    if let Some(h) = top_handle {
        fs.release(ctx, top_entry.inode, 0, h, false, false, None)?;
    }

    Ok(())
}

#[test]
fn test_create_with_flags() -> io::Result<()> {
    // Create test layers with a directory
    let layers = vec![vec![("dir1", true, 0o755)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Test different flag combinations
    let test_cases = vec![
        ("file_rdonly.txt", libc::O_RDONLY, 0o644),
        ("file_wronly.txt", libc::O_WRONLY, 0o644),
        ("file_rdwr.txt", libc::O_RDWR, 0o644),
        ("file_append.txt", libc::O_WRONLY | libc::O_APPEND, 0o644),
        ("file_trunc.txt", libc::O_WRONLY | libc::O_TRUNC, 0o644),
        ("file_excl.txt", libc::O_WRONLY | libc::O_EXCL, 0o644),
    ];

    for (name, flags, mode) in test_cases {
        let file_name = CString::new(name).unwrap();
        let (entry, handle, _) = fs.create(
            ctx,
            1,
            &file_name,
            mode,
            flags as u32,
            0o022,
            Extensions::default(),
        )?;

        // Verify file creation
        let entry_mode = entry.attr.st_mode as u32;
        assert_eq!(entry_mode & libc::S_IFMT as u32, libc::S_IFREG as u32);
        assert_eq!(entry_mode & 0o777, mode & !0o022);

        // Verify file exists
        let file_path = temp_dirs.last().unwrap().path().join(name);
        assert!(file_path.exists());
        assert!(file_path.is_file());

        // Release handle if we got one
        if let Some(h) = handle {
            fs.release(ctx, entry.inode, 0, h, false, false, None)?;
        }
    }

    Ok(())
}

#[test]
fn test_create_existing_name() -> io::Result<()> {
    // Create test layers with existing files
    let layers = vec![vec![
        ("dir1", true, 0o755),
        ("existing_file.txt", false, 0o644),
    ]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();
    let file_name = CString::new("existing_file.txt").unwrap();

    // Try to create a file with existing name without O_EXCL
    match fs.create(
        ctx,
        1,
        &file_name,
        0o644,
        libc::O_WRONLY as u32,
        0o022,
        Extensions::default(),
    ) {
        Ok(_) => panic!("Expected create with existing name to fail"),
        Err(e) => assert_eq!(e.kind(), io::ErrorKind::AlreadyExists),
    }

    Ok(())
}

#[test]
fn test_create_invalid_parent() -> io::Result<()> {
    // Create test layers
    let layers = vec![vec![("dir1", true, 0o755)]];

    let (fs, _temp_dirs) = helper::create_overlayfs(layers)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Test with invalid parent inode
    let file_name = CString::new("test.txt").unwrap();
    let invalid_inode = 999999;
    match fs.create(
        ctx,
        invalid_inode,
        &file_name,
        0o644,
        0,
        0o022,
        Extensions::default(),
    ) {
        Ok(_) => panic!("Expected create with invalid parent to fail"),
        Err(e) => assert_eq!(e.raw_os_error(), Some(libc::EBADF)),
    }

    Ok(())
}

#[test]
fn test_mknod_basic() -> io::Result<()> {
    // Create test layers with a directory
    let layers = vec![vec![("dir1", true, 0o755)]];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Test creating different types of nodes
    let test_cases: Vec<(&str, u32, &str)> = vec![
        ("fifo1", libc::S_IFIFO as u32 | 0o644, "named pipe"),
        ("sock1", libc::S_IFSOCK as u32 | 0o644, "unix domain socket"),
    ];

    for (name, mode, node_type) in test_cases {
        let node_name = CString::new(name).unwrap();
        let entry = fs.mknod(ctx, 1, &node_name, mode, 0, 0o022, Extensions::default())?;

        // Verify node creation
        let entry_mode = entry.attr.st_mode as u32;
        #[cfg(target_os = "linux")]
        assert_eq!(entry_mode & libc::S_IFMT as u32, mode & libc::S_IFMT as u32);
        #[cfg(target_os = "macos")]
        assert_eq!(entry_mode & libc::S_IFMT as u32, libc::S_IFREG as u32);
        assert_eq!(entry_mode & 0o777, (0o644 & !0o022) as u32);

        // Verify node exists with correct type
        let node_path = temp_dirs.last().unwrap().path().join(name);
        assert!(node_path.exists());
    }

    Ok(())
}

#[test]
fn test_mknod_nested() -> io::Result<()> {
    // Create test layers with complex structure
    let layers = vec![
        vec![
            ("dir1", true, 0o755),
            ("dir1/file1", false, 0o644),
            ("dir1/subdir", true, 0o755),
        ],
        vec![("dir2", true, 0o755)],
        vec![("dir3", true, 0o755)],
    ];

    let (fs, temp_dirs) = helper::create_overlayfs(layers)?;
    helper::debug_print_layers(&temp_dirs, false)?;

    // Initialize filesystem
    fs.init(FsOptions::empty())?;

    let ctx = Context::default();

    // Create nodes in different directories
    let test_cases = vec![
        ("dir1", "fifo1", libc::S_IFIFO as u32 | 0o644),
        ("dir2", "sock1", libc::S_IFSOCK as u32 | 0o644),
        ("dir3", "fifo2", libc::S_IFIFO as u32 | 0o644),
    ];

    for (dir, name, mode) in test_cases {
        let dir_name = CString::new(dir).unwrap();
        let dir_entry = fs.lookup(ctx, 1, &dir_name)?;
        let node_name = CString::new(name).unwrap();

        let entry = fs.mknod(
            ctx,
            dir_entry.inode,
            &node_name,
            mode,
            0,
            0o022,
            Extensions::default(),
        )?;

        // Verify node creation
        let entry_mode = entry.attr.st_mode as u32;
        #[cfg(target_os = "linux")]
        assert_eq!(entry_mode & libc::S_IFMT as u32, mode & libc::S_IFMT as u32);
        #[cfg(target_os = "macos")]
        assert_eq!(entry_mode & libc::S_IFMT as u32, libc::S_IFREG as u32);
        assert_eq!(entry_mode & 0o777, (0o644 & !0o022) as u32);

        // Verify node exists in the top layer
        let node_path = temp_dirs.last().unwrap().path().join(dir).join(name);
        assert!(node_path.exists());
    }

    Ok(())
}
