//! Construction-only immutable backing for privately mapped guest RAM.

use std::fs::File;
use std::io;
use std::sync::Arc;

#[cfg(any(test, not(feature = "tee")))]
use vm_memory::{GuestAddress, GuestMemoryMmap};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// One complete guest RAM region in an immutable flat memory image.
#[derive(Clone, Debug)]
pub struct PrivateMemoryRegion {
    /// Guest physical start; must exactly match the constructed RAM topology.
    pub guest_address: u64,
    /// Region length in bytes.
    pub length: u64,
    /// Native-page-aligned offset in the backing file.
    pub file_offset: u64,
}

/// Immutable, read-only file ownership retained by every resulting mapping.
///
/// The caller must exclude writes and truncation through any other file handle for the entire
/// mapping lifetime. Unlinking the file is safe; modifying its bytes is not. This is a trusted
/// embedding-runtime contract, not protection against a malicious host process.
#[derive(Clone, Debug)]
pub struct PrivateMemoryBacking {
    file: Arc<File>,
    regions: Vec<PrivateMemoryRegion>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl PrivateMemoryBacking {
    /// Construct an unlinked, immutable sparse zero base for a fresh private-memory VM.
    /// Boot writes fault into private pages; the zero file itself is never modified afterward.
    // TEE builders do not use private mappings; keep the helper available for mapping unit tests.
    #[cfg(any(test, not(feature = "tee")))]
    pub(crate) fn zeroed(ranges: &[(GuestAddress, usize)]) -> io::Result<Self> {
        #[cfg(not(unix))]
        {
            let _ = ranges;
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "private boot memory is not qualified on this backend",
            ));
        }
        #[cfg(unix)]
        {
            let mut offset = 0u64;
            let mut regions = Vec::with_capacity(ranges.len());
            for &(address, length) in ranges {
                regions.push(PrivateMemoryRegion {
                    guest_address: address.0,
                    length: length as u64,
                    file_offset: offset,
                });
                offset = offset
                    .checked_add(length as u64)
                    .ok_or_else(|| invalid("zero memory size overflows"))?;
            }
            let (fd, path) = nix::unistd::mkstemp(&std::env::temp_dir().join("krun-zero-XXXXXX"))?;
            let writer = File::from(fd);
            let opened = File::open(&path);
            // Unlink before operations that can fail; descriptor ownership handles all later cleanup.
            std::fs::remove_file(&path)?;
            writer.set_len(offset)?;
            Self::new(opened?, regions)
        }
    }

    /// Bind a read-only complete memory image to its exact guest RAM topology.
    ///
    /// This represents restored memory, not a mutable file-backed boot allocation. The VMM must
    /// suppress cold-boot writes after installing it and restore execution before activation.
    pub fn new(file: File, regions: Vec<PrivateMemoryRegion>) -> io::Result<Self> {
        if regions.is_empty() || !file.metadata()?.is_file() {
            return Err(invalid(
                "private memory requires a regular file and nonempty topology",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // Require a read-only handle so neither the VMM nor its devices can accidentally
            // write shared state through the descriptor retained by FileOffset.
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            if flags & libc::O_ACCMODE != libc::O_RDONLY {
                return Err(invalid("private memory backing handle must be read-only"));
            }
        }
        #[cfg(not(any(unix, windows)))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private guest memory is not qualified on this backend",
        ));

        #[cfg(any(unix, windows))]
        {
            let backing = Self {
                file: Arc::new(file),
                regions,
            };
            backing.validate()?;
            Ok(backing)
        }
    }

    /// Map the complete image without reading or copying its RAM-sized contents.
    #[cfg(any(test, not(feature = "tee")))]
    pub(crate) fn map(&self, expected: &[(GuestAddress, usize)]) -> io::Result<GuestMemoryMmap> {
        self.validate()?;
        // A portable image can coalesce contiguous extents that the VMM constructs as several
        // RAM slots (for example a kernel slot beside ordinary RAM). Preserve those VMM slot
        // boundaries while requiring exactly the same address coverage, including every hole.
        let mut mappings = Vec::with_capacity(expected.len());
        let mut region_index = 0;
        let mut consumed = 0;
        for &(address, length) in expected {
            let region = self
                .regions
                .get(region_index)
                .ok_or_else(|| invalid("unexpected guest RAM region"))?;
            let length = length as u64;
            if length == 0
                || address.0 != region.guest_address + consumed
                || length > region.length - consumed
            {
                return Err(invalid(
                    "private memory image does not match guest RAM topology",
                ));
            }
            mappings.push(PrivateMemoryRegion {
                guest_address: address.0,
                length,
                file_offset: region.file_offset + consumed,
            });
            consumed += length;
            if consumed == region.length {
                consumed = 0;
                region_index += 1;
            }
        }
        if region_index != self.regions.len() || consumed != 0 {
            return Err(invalid("private memory image has unmapped guest RAM"));
        }
        #[cfg(unix)]
        {
            use vm_memory::mmap::MmapRegionBuilder;
            use vm_memory::{FileOffset, GuestRegionMmap};
            let regions = mappings
                .iter()
                .map(|region| {
                    let length = usize::try_from(region.length)
                        .map_err(|_| invalid("RAM region exceeds host address space"))?;
                    let mapping = MmapRegionBuilder::new(length)
                        .with_file_offset(FileOffset::from_arc(
                            self.file.clone(),
                            region.file_offset,
                        ))
                        .with_mmap_prot(libc::PROT_READ | libc::PROT_WRITE)
                        .with_mmap_flags(libc::MAP_PRIVATE)
                        .build()
                        .map_err(io::Error::other)?;
                    GuestRegionMmap::new(mapping, GuestAddress(region.guest_address))
                        .ok_or_else(|| invalid("private memory guest range overflows"))
                })
                .collect::<io::Result<Vec<_>>>()?;
            GuestMemoryMmap::from_regions(regions).map_err(io::Error::other)
        }
        #[cfg(windows)]
        {
            use vm_memory::mmap::MmapRegion;
            use vm_memory::{FileOffset, GuestRegionMmap};

            // Windows view offsets must be allocation-granularity aligned (typically 64 KiB),
            // while RAM slots may be only page aligned. Map the packed file once at zero;
            // checked slices share its private view and preserve the exact guest topology.
            let length = usize::try_from(self.file.metadata()?.len())
                .map_err(|_| invalid("RAM image exceeds host address space"))?;
            let view = MmapRegion::<()>::from_file_private(
                FileOffset::from_arc(self.file.clone(), 0),
                length,
            )?;
            let regions = mappings
                .iter()
                .map(|region| {
                    let offset = usize::try_from(region.file_offset)
                        .map_err(|_| invalid("RAM offset exceeds host address space"))?;
                    let length = usize::try_from(region.length)
                        .map_err(|_| invalid("RAM region exceeds host address space"))?;
                    GuestRegionMmap::new(
                        view.file_slice(offset, length)?,
                        GuestAddress(region.guest_address),
                    )
                    .ok_or_else(|| invalid("private memory guest range overflows"))
                })
                .collect::<io::Result<Vec<_>>>()?;
            GuestMemoryMmap::from_regions(regions).map_err(io::Error::other)
        }
        #[cfg(not(any(unix, windows)))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private guest memory is not qualified on this backend",
        ))
    }

    fn validate(&self) -> io::Result<()> {
        #[cfg(unix)]
        let page = {
            let value = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            if value <= 0 {
                return Err(io::Error::last_os_error());
            }
            value as u64
        };
        #[cfg(not(unix))]
        let page = 4096;
        let file_len = self.file.metadata()?.len();
        let mut end = 0;
        for region in &self.regions {
            if region.length == 0
                || region.guest_address < end
                || !region.guest_address.is_multiple_of(page)
                || !region.length.is_multiple_of(page)
                || !region.file_offset.is_multiple_of(page)
            {
                return Err(invalid(
                    "private memory regions must be ordered, disjoint, and native-page aligned",
                ));
            }
            end = region
                .guest_address
                .checked_add(region.length)
                .ok_or_else(|| invalid("private guest RAM range overflows"))?;
            let file_end = region
                .file_offset
                .checked_add(region.length)
                .ok_or_else(|| invalid("private memory file range overflows"))?;
            if file_end > file_len {
                return Err(invalid("private memory file is shorter than its topology"));
            }
        }
        Ok(())
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;
    use vm_memory::Bytes;

    fn fixture() -> (File, File, Vec<PrivateMemoryRegion>) {
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let (fd, path) =
            nix::unistd::mkstemp(&std::env::temp_dir().join("krun-private-XXXXXX")).unwrap();
        let mut file = File::from(fd);
        let readonly = File::open(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        file.write_all(&vec![0x5a; page * 2]).unwrap();
        let regions = vec![PrivateMemoryRegion {
            guest_address: 0x80000000,
            length: page as u64,
            file_offset: page as u64,
        }];
        (file, readonly, regions)
    }

    #[test]
    fn fresh_zero_backing_is_sparse_unlinked_and_private() {
        use std::os::unix::fs::{FileExt, MetadataExt};
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let ranges = [(GuestAddress(0x80000000), page * 8)];
        let backing = PrivateMemoryBacking::zeroed(&ranges).unwrap();
        assert_eq!(backing.file.metadata().unwrap().nlink(), 0);
        assert_eq!(backing.file.metadata().unwrap().blocks(), 0);
        let memory = backing.map(&ranges).unwrap();
        memory.write_obj(0x7fu8, ranges[0].0).unwrap();
        let mut original = [1u8];
        backing.file.read_exact_at(&mut original, 0).unwrap();
        assert_eq!(original, [0]);
        assert_eq!(memory.read_obj::<u8>(ranges[0].0).unwrap(), 0x7f);
    }

    #[test]
    fn siblings_and_file_are_immutable_after_host_write_and_unlink() {
        let (file, readonly, regions) = fixture();
        let expected = [(
            GuestAddress(regions[0].guest_address),
            regions[0].length as usize,
        )];
        let backing = PrivateMemoryBacking::new(readonly, regions.clone()).unwrap();
        let first = backing.map(&expected).unwrap();
        let second = backing.map(&expected).unwrap();
        drop(backing);
        first.write_obj(0x11u8, expected[0].0).unwrap();
        assert_eq!(first.read_obj::<u8>(expected[0].0).unwrap(), 0x11);
        assert_eq!(second.read_obj::<u8>(expected[0].0).unwrap(), 0x5a);
        use std::os::unix::fs::FileExt;
        let mut original = [0];
        file.read_exact_at(&mut original, regions[0].file_offset)
            .unwrap();
        assert_eq!(original, [0x5a]);
    }

    #[test]
    fn reject_writable_handle_and_wrong_topology() {
        let (file, readonly, regions) = fixture();
        assert!(PrivateMemoryBacking::new(file, regions.clone()).is_err());
        let backing = PrivateMemoryBacking::new(readonly, regions).unwrap();
        assert!(backing.map(&[]).is_err());
        assert!(backing.map(&[(GuestAddress(0), 4096)]).is_err());
    }

    #[test]
    fn reject_truncation_alignment_and_overflow() {
        let (file, readonly, regions) = fixture();
        let mut invalid = regions.clone();
        invalid[0].file_offset += 1;
        assert!(PrivateMemoryBacking::new(readonly.try_clone().unwrap(), invalid).is_err());
        let mut overflow = regions.clone();
        overflow[0].guest_address = u64::MAX - (regions[0].length - 1);
        assert!(PrivateMemoryBacking::new(readonly.try_clone().unwrap(), overflow).is_err());
        let mut overflow = regions.clone();
        overflow[0].file_offset = u64::MAX - (regions[0].length - 1);
        assert!(PrivateMemoryBacking::new(readonly.try_clone().unwrap(), overflow).is_err());
        let backing = PrivateMemoryBacking::new(readonly, regions.clone()).unwrap();
        file.set_len(0).unwrap();
        assert!(backing
            .map(&[(
                GuestAddress(regions[0].guest_address),
                regions[0].length as usize
            )])
            .is_err());
    }

    #[test]
    fn preserve_vmm_slot_boundaries_without_losing_coverage() {
        let (file, readonly, mut regions) = fixture();
        let page = regions[0].length;
        file.set_len(page * 3).unwrap();
        regions[0].length = page * 2;
        let start = regions[0].guest_address;
        let backing = PrivateMemoryBacking::new(readonly, regions).unwrap();
        let memory = backing
            .map(&[
                (GuestAddress(start), page as usize),
                (GuestAddress(start + page), page as usize),
            ])
            .unwrap();
        use vm_memory::GuestMemoryBackend;
        assert_eq!(memory.num_regions(), 2);
        assert!(backing
            .map(&[(GuestAddress(start), page as usize)])
            .is_err());
        assert!(backing
            .map(&[
                (GuestAddress(start), page as usize),
                (GuestAddress(start + 2 * page), page as usize),
            ])
            .is_err());
        assert!(backing
            .map(&[
                (GuestAddress(start), page as usize),
                (GuestAddress(start), page as usize),
            ])
            .is_err());
    }
}
