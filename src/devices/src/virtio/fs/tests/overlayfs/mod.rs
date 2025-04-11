#[cfg(test)]
mod create;

#[cfg(test)]
mod lookup;

#[cfg(test)]
mod metadata;

#[cfg(test)]
mod misc;

#[cfg(test)]
mod open;

#[cfg(test)]
mod read;

#[cfg(test)]
mod remove;

#[cfg(test)]
mod write;

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl Default for crate::virtio::fs::filesystem::Context {
    fn default() -> Self {
        Self {
            uid: 0,
            gid: 0,
            pid: 0,
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Modules: Helper
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod helper {
    use std::{
        fs::{self, File},
        io,
        os::unix::fs::PermissionsExt,
        process::Command,
    };

    use crate::virtio::{
        fs::filesystem::{ZeroCopyReader, ZeroCopyWriter},
        fs::overlayfs::{Config, OverlayFs},
    };

    use tempfile::TempDir;

    //--------------------------------------------------------------------------------------------------
    // Types
    //--------------------------------------------------------------------------------------------------

    pub(super) struct TestContainer(pub(super) Vec<u8>);

    //--------------------------------------------------------------------------------------------------
    // Trait Implementations
    //--------------------------------------------------------------------------------------------------

    impl io::Write for TestContainer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl ZeroCopyWriter for TestContainer {
        fn write_from(&mut self, f: &File, count: usize, off: u64) -> io::Result<usize> {
            use std::os::unix::fs::FileExt;

            // Pre-allocate space in our vector to avoid reallocations
            let original_len = self.0.len();
            self.0.resize(original_len + count, 0);

            // Read directly into our vector's buffer
            let bytes_read = f.read_at(&mut self.0[original_len..original_len + count], off)?;

            // Adjust the size to match what was actually read
            self.0.truncate(original_len + bytes_read);

            if bytes_read == 0 && count > 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "unexpected EOF",
                ));
            }

            Ok(bytes_read)
        }
    }

    impl io::Read for TestContainer {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let available = self.0.len();
            if available == 0 {
                return Ok(0);
            }

            let amt = std::cmp::min(buf.len(), available);
            buf[..amt].copy_from_slice(&self.0[..amt]);
            Ok(amt)
        }
    }

    impl ZeroCopyReader for TestContainer {
        fn read_to(&mut self, f: &File, count: usize, off: u64) -> io::Result<usize> {
            use std::os::unix::fs::FileExt;

            let available = self.0.len();
            if available == 0 {
                return Ok(0);
            }

            let to_write = std::cmp::min(count, available);
            let written = f.write_at(&self.0[..to_write], off)?;
            Ok(written)
        }
    }

    //--------------------------------------------------------------------------------------------------
    // Functions
    //--------------------------------------------------------------------------------------------------

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

        let cfg = Config {
            layers: layer_paths,
            ..Default::default()
        };

        let overlayfs = OverlayFs::new(cfg)?;
        Ok((overlayfs, temp_dirs))
    }

    // Debug utility to print the directory structure of each layer using tree command
    pub(super) fn debug_print_layers(temp_dirs: &[TempDir], show_perms: bool) -> io::Result<()> {
        if Command::new("tree").arg("--version").output().is_err() {
            println!("tree command is not accessible. please install it to see the layer directory structures.");
            return Ok(());
        }
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
