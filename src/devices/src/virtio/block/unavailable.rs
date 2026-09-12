//! Storage-free placeholder for an explicitly unavailable block device.

use std::{fmt, io};

use imago::{
    io_buffers::{IoVector, IoVectorMut},
    storage::drivers::CommonStorageHelper,
    Storage,
};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Debug)]
pub(super) struct UnavailableStorage {
    pub(super) size: u64,
    pub(super) helper: CommonStorageHelper,
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl fmt::Display for UnavailableStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unavailable block storage")
    }
}

impl Storage for UnavailableStorage {
    fn size(&self) -> io::Result<u64> {
        Ok(self.size)
    }

    async unsafe fn pure_readv(&self, _: IoVectorMut<'_>, _: u64) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EIO))
    }

    async unsafe fn pure_writev(&self, _: IoVector<'_>, _: u64) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EIO))
    }

    async unsafe fn pure_write_zeroes(&self, _: u64, _: u64) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EIO))
    }

    async unsafe fn pure_discard(&self, _: u64, _: u64) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::EIO))
    }

    // Host teardown has no data to flush. Guest FLUSH requests are rejected by the
    // worker before reaching this path, including when unsafe caching was captured.
    async fn flush(&self) -> io::Result<()> {
        Ok(())
    }
    async fn sync(&self) -> io::Result<()> {
        Ok(())
    }
    async unsafe fn invalidate_cache(&self) -> io::Result<()> {
        Ok(())
    }
    fn get_storage_helper(&self) -> &CommonStorageHelper {
        &self.helper
    }
}
