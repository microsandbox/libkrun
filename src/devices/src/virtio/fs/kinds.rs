

use std::{ffi::CStr, io, path::PathBuf, sync::{atomic::AtomicI32, Arc}, time::Duration};

#[cfg(target_os = "macos")]
use crossbeam_channel::Sender;
#[cfg(target_os = "macos")]
use hvf::MemoryMapping;

use crate::virtio::bindings;

use super::{
    filesystem::{
        Context, DirEntry, Entry, Extensions, FileSystem, GetxattrReply, ListxattrReply,
        ZeroCopyReader, ZeroCopyWriter,
    },
    fuse::{FsOptions, OpenOptions, RemovemappingOne, SetattrValid},
    overlayfs::{self, OverlayFs},
    passthrough::{self, PassthroughFs},
};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum FsImplConfig {
    Passthrough(passthrough::Config),
    Overlayfs(overlayfs::Config),
}

pub enum FsImpl {
    Passthrough(PassthroughFs),
    Overlayfs(OverlayFs),
}

#[derive(Clone, Debug)]
pub enum FsImplShare {
    Passthrough(String),
    Overlayfs(Vec<PathBuf>),
}

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

impl FileSystem for FsImpl {
    type Inode = u64;
    type Handle = u64;

    fn init(&self, capable: FsOptions) -> io::Result<FsOptions> {
        match self {
            FsImpl::Passthrough(fs) => fs.init(capable),
            FsImpl::Overlayfs(fs) => fs.init(capable),
        }
    }

    fn destroy(&self) {
        match self {
            FsImpl::Passthrough(fs) => fs.destroy(),
            FsImpl::Overlayfs(fs) => fs.destroy(),
        }
    }

    fn lookup(&self, ctx: Context, parent: Self::Inode, name: &CStr) -> io::Result<Entry> {
        match self {
            FsImpl::Passthrough(fs) => fs.lookup(ctx, parent, name),
            FsImpl::Overlayfs(fs) => fs.lookup(ctx, parent, name),
        }
    }

    fn forget(&self, ctx: Context, inode: Self::Inode, count: u64) {
        match self {
            FsImpl::Passthrough(fs) => fs.forget(ctx, inode, count),
            FsImpl::Overlayfs(fs) => fs.forget(ctx, inode, count),
        }
    }

    fn batch_forget(&self, ctx: Context, requests: Vec<(Self::Inode, u64)>) {
        match self {
            FsImpl::Passthrough(fs) => fs.batch_forget(ctx, requests),
            FsImpl::Overlayfs(fs) => fs.batch_forget(ctx, requests),
        }
    }

    fn getattr(
        &self,
        ctx: Context,
        inode: Self::Inode,
        handle: Option<Self::Handle>,
    ) -> io::Result<(bindings::stat64, Duration)> {
        match self {
            FsImpl::Passthrough(fs) => fs.getattr(ctx, inode, handle),
            FsImpl::Overlayfs(fs) => fs.getattr(ctx, inode, handle),
        }
    }

    fn setattr(
        &self,
        ctx: Context,
        inode: Self::Inode,
        attr: bindings::stat64,
        handle: Option<Self::Handle>,
        valid: SetattrValid,
    ) -> io::Result<(bindings::stat64, Duration)> {
        match self {
            FsImpl::Passthrough(fs) => fs.setattr(ctx, inode, attr, handle, valid),
            FsImpl::Overlayfs(fs) => fs.setattr(ctx, inode, attr, handle, valid),
        }
    }

    fn readlink(&self, ctx: Context, inode: Self::Inode) -> io::Result<Vec<u8>> {
        match self {
            FsImpl::Passthrough(fs) => fs.readlink(ctx, inode),
            FsImpl::Overlayfs(fs) => fs.readlink(ctx, inode),
        }
    }

    fn symlink(
        &self,
        ctx: Context,
        linkname: &CStr,
        parent: Self::Inode,
        name: &CStr,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        match self {
            FsImpl::Passthrough(fs) => fs.symlink(ctx, linkname, parent, name, extensions),
            FsImpl::Overlayfs(fs) => fs.symlink(ctx, linkname, parent, name, extensions),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn mknod(
        &self,
        ctx: Context,
        inode: Self::Inode,
        name: &CStr,
        mode: u32,
        rdev: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        match self {
            FsImpl::Passthrough(fs) => fs.mknod(ctx, inode, name, mode, rdev, umask, extensions),
            FsImpl::Overlayfs(fs) => fs.mknod(ctx, inode, name, mode, rdev, umask, extensions),
        }
    }

    fn mkdir(
        &self,
        ctx: Context,
        parent: Self::Inode,
        name: &CStr,
        mode: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<Entry> {
        match self {
            FsImpl::Passthrough(fs) => fs.mkdir(ctx, parent, name, mode, umask, extensions),
            FsImpl::Overlayfs(fs) => fs.mkdir(ctx, parent, name, mode, umask, extensions),
        }
    }

    fn unlink(&self, ctx: Context, parent: Self::Inode, name: &CStr) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.unlink(ctx, parent, name),
            FsImpl::Overlayfs(fs) => fs.unlink(ctx, parent, name),
        }
    }

    fn rmdir(&self, ctx: Context, parent: Self::Inode, name: &CStr) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.rmdir(ctx, parent, name),
            FsImpl::Overlayfs(fs) => fs.rmdir(ctx, parent, name),
        }
    }

    fn rename(
        &self,
        ctx: Context,
        olddir: Self::Inode,
        oldname: &CStr,
        newdir: Self::Inode,
        newname: &CStr,
        flags: u32,
    ) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.rename(ctx, olddir, oldname, newdir, newname, flags),
            FsImpl::Overlayfs(fs) => fs.rename(ctx, olddir, oldname, newdir, newname, flags),
        }
    }

    fn link(
        &self,
        ctx: Context,
        inode: Self::Inode,
        newparent: Self::Inode,
        newname: &CStr,
    ) -> io::Result<Entry> {
        match self {
            FsImpl::Passthrough(fs) => fs.link(ctx, inode, newparent, newname),
            FsImpl::Overlayfs(fs) => fs.link(ctx, inode, newparent, newname),
        }
    }

    fn open(
        &self,
        ctx: Context,
        inode: Self::Inode,
        flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        match self {
            FsImpl::Passthrough(fs) => fs.open(ctx, inode, flags),
            FsImpl::Overlayfs(fs) => fs.open(ctx, inode, flags),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        ctx: Context,
        parent: Self::Inode,
        name: &CStr,
        mode: u32,
        flags: u32,
        umask: u32,
        extensions: Extensions,
    ) -> io::Result<(Entry, Option<Self::Handle>, OpenOptions)> {
        match self {
            FsImpl::Passthrough(fs) => fs.create(ctx, parent, name, mode, flags, umask, extensions),
            FsImpl::Overlayfs(fs) => fs.create(ctx, parent, name, mode, flags, umask, extensions),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read<W: io::Write + ZeroCopyWriter>(
        &self,
        ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        w: W,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        flags: u32,
    ) -> io::Result<usize> {
        match self {
            FsImpl::Passthrough(fs) => {
                fs.read(ctx, inode, handle, w, size, offset, lock_owner, flags)
            }
            FsImpl::Overlayfs(fs) => {
                fs.read(ctx, inode, handle, w, size, offset, lock_owner, flags)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write<R: io::Read + ZeroCopyReader>(
        &self,
        ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        r: R,
        size: u32,
        offset: u64,
        lock_owner: Option<u64>,
        delayed_write: bool,
        kill_priv: bool,
        flags: u32,
    ) -> io::Result<usize> {
        match self {
            FsImpl::Passthrough(fs) => fs.write(
                ctx,
                inode,
                handle,
                r,
                size,
                offset,
                lock_owner,
                delayed_write,
                kill_priv,
                flags,
            ),
            FsImpl::Overlayfs(fs) => fs.write(
                ctx,
                inode,
                handle,
                r,
                size,
                offset,
                lock_owner,
                delayed_write,
                kill_priv,
                flags,
            ),
        }
    }

    fn flush(
        &self,
        ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        lock_owner: u64,
    ) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.flush(ctx, inode, handle, lock_owner),
            FsImpl::Overlayfs(fs) => fs.flush(ctx, inode, handle, lock_owner),
        }
    }

    fn fsync(
        &self,
        ctx: Context,
        inode: Self::Inode,
        datasync: bool,
        handle: Self::Handle,
    ) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.fsync(ctx, inode, datasync, handle),
            FsImpl::Overlayfs(fs) => fs.fsync(ctx, inode, datasync, handle),
        }
    }

    fn fallocate(
        &self,
        ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        mode: u32,
        offset: u64,
        length: u64,
    ) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.fallocate(ctx, inode, handle, mode, offset, length),
            FsImpl::Overlayfs(fs) => fs.fallocate(ctx, inode, handle, mode, offset, length),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn release(
        &self,
        ctx: Context,
        inode: Self::Inode,
        flags: u32,
        handle: Self::Handle,
        flush: bool,
        flock_release: bool,
        lock_owner: Option<u64>,
    ) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => {
                fs.release(ctx, inode, flags, handle, flush, flock_release, lock_owner)
            }
            FsImpl::Overlayfs(fs) => {
                fs.release(ctx, inode, flags, handle, flush, flock_release, lock_owner)
            }
        }
    }

    fn statfs(&self, ctx: Context, inode: Self::Inode) -> io::Result<bindings::statvfs64> {
        match self {
            FsImpl::Passthrough(fs) => fs.statfs(ctx, inode),
            FsImpl::Overlayfs(fs) => fs.statfs(ctx, inode),
        }
    }

    fn setxattr(
        &self,
        ctx: Context,
        inode: Self::Inode,
        name: &CStr,
        value: &[u8],
        flags: u32,
    ) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.setxattr(ctx, inode, name, value, flags),
            FsImpl::Overlayfs(fs) => fs.setxattr(ctx, inode, name, value, flags),
        }
    }

    fn getxattr(
        &self,
        ctx: Context,
        inode: Self::Inode,
        name: &CStr,
        size: u32,
    ) -> io::Result<GetxattrReply> {
        match self {
            FsImpl::Passthrough(fs) => fs.getxattr(ctx, inode, name, size),
            FsImpl::Overlayfs(fs) => fs.getxattr(ctx, inode, name, size),
        }
    }

    fn listxattr(&self, ctx: Context, inode: Self::Inode, size: u32) -> io::Result<ListxattrReply> {
        match self {
            FsImpl::Passthrough(fs) => fs.listxattr(ctx, inode, size),
            FsImpl::Overlayfs(fs) => fs.listxattr(ctx, inode, size),
        }
    }

    fn removexattr(&self, ctx: Context, inode: Self::Inode, name: &CStr) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.removexattr(ctx, inode, name),
            FsImpl::Overlayfs(fs) => fs.removexattr(ctx, inode, name),
        }
    }

    fn opendir(
        &self,
        ctx: Context,
        inode: Self::Inode,
        flags: u32,
    ) -> io::Result<(Option<Self::Handle>, OpenOptions)> {
        match self {
            FsImpl::Passthrough(fs) => fs.opendir(ctx, inode, flags),
            FsImpl::Overlayfs(fs) => fs.opendir(ctx, inode, flags),
        }
    }

    fn readdir<F>(
        &self,
        ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        size: u32,
        offset: u64,
        add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry) -> io::Result<usize>,
    {
        match self {
            FsImpl::Passthrough(fs) => fs.readdir(ctx, inode, handle, size, offset, add_entry),
            FsImpl::Overlayfs(fs) => fs.readdir(ctx, inode, handle, size, offset, add_entry),
        }
    }

    fn readdirplus<F>(
        &self,
        ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        size: u32,
        offset: u64,
        add_entry: F,
    ) -> io::Result<()>
    where
        F: FnMut(DirEntry, Entry) -> io::Result<usize>,
    {
        match self {
            FsImpl::Passthrough(fs) => fs.readdirplus(ctx, inode, handle, size, offset, add_entry),
            FsImpl::Overlayfs(fs) => fs.readdirplus(ctx, inode, handle, size, offset, add_entry),
        }
    }

    fn fsyncdir(
        &self,
        ctx: Context,
        inode: Self::Inode,
        datasync: bool,
        handle: Self::Handle,
    ) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.fsyncdir(ctx, inode, datasync, handle),
            FsImpl::Overlayfs(fs) => fs.fsyncdir(ctx, inode, datasync, handle),
        }
    }

    fn releasedir(
        &self,
        ctx: Context,
        inode: Self::Inode,
        flags: u32,
        handle: Self::Handle,
    ) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.releasedir(ctx, inode, flags, handle),
            FsImpl::Overlayfs(fs) => fs.releasedir(ctx, inode, flags, handle),
        }
    }

    fn access(&self, ctx: Context, inode: Self::Inode, mask: u32) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.access(ctx, inode, mask),
            FsImpl::Overlayfs(fs) => fs.access(ctx, inode, mask),
        }
    }

    fn lseek(
        &self,
        ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        offset: u64,
        whence: u32,
    ) -> io::Result<u64> {
        match self {
            FsImpl::Passthrough(fs) => fs.lseek(ctx, inode, handle, offset, whence),
            FsImpl::Overlayfs(fs) => fs.lseek(ctx, inode, handle, offset, whence),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn copyfilerange(
        &self,
        ctx: Context,
        inode_in: Self::Inode,
        handle_in: Self::Handle,
        offset_in: u64,
        inode_out: Self::Inode,
        handle_out: Self::Handle,
        offset_out: u64,
        len: u64,
        flags: u64,
    ) -> io::Result<usize> {
        match self {
            FsImpl::Passthrough(fs) => fs.copyfilerange(
                ctx, inode_in, handle_in, offset_in, inode_out, handle_out, offset_out, len, flags,
            ),
            FsImpl::Overlayfs(fs) => fs.copyfilerange(
                ctx, inode_in, handle_in, offset_in, inode_out, handle_out, offset_out, len, flags,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setupmapping(
        &self,
        ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        foffset: u64,
        len: u64,
        flags: u64,
        moffset: u64,
        host_shm_base: u64,
        shm_size: u64,
        #[cfg(target_os = "macos")] map_sender: &Option<Sender<MemoryMapping>>,
    ) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.setupmapping(
                ctx,
                inode,
                handle,
                foffset,
                len,
                flags,
                moffset,
                host_shm_base,
                shm_size,
                #[cfg(target_os = "macos")] map_sender,
            ),
            FsImpl::Overlayfs(fs) => fs.setupmapping(
                ctx,
                inode,
                handle,
                foffset,
                len,
                flags,
                moffset,
                host_shm_base,
                shm_size,
                #[cfg(target_os = "macos")] map_sender,
            ),
        }
    }

    fn removemapping(
        &self,
        ctx: Context,
        requests: Vec<RemovemappingOne>,
        host_shm_base: u64,
        shm_size: u64,
        #[cfg(target_os = "macos")] map_sender: &Option<Sender<MemoryMapping>>,
    ) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => {
                fs.removemapping(ctx, requests, host_shm_base, shm_size, #[cfg(target_os = "macos")] map_sender)
            }
            FsImpl::Overlayfs(fs) => {
                fs.removemapping(ctx, requests, host_shm_base, shm_size, #[cfg(target_os = "macos")] map_sender)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn ioctl(
        &self,
        ctx: Context,
        inode: Self::Inode,
        handle: Self::Handle,
        flags: u32,
        cmd: u32,
        arg: u64,
        in_size: u32,
        out_size: u32,
        exit_code: &Arc<AtomicI32>,
    ) -> io::Result<Vec<u8>> {
        match self {
            FsImpl::Passthrough(fs) => {
                fs.ioctl(ctx, inode, handle, flags, cmd, arg, in_size, out_size, exit_code)
            }
            FsImpl::Overlayfs(fs) => {
                fs.ioctl(ctx, inode, handle, flags, cmd, arg, in_size, out_size, exit_code)
            }
        }
    }

    fn getlk(&self) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.getlk(),
            FsImpl::Overlayfs(fs) => fs.getlk(),
        }
    }

    fn setlk(&self) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.setlk(),
            FsImpl::Overlayfs(fs) => fs.setlk(),
        }
    }

    fn setlkw(&self) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.setlkw(),
            FsImpl::Overlayfs(fs) => fs.setlkw(),
        }
    }

    fn bmap(&self) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.bmap(),
            FsImpl::Overlayfs(fs) => fs.bmap(),
        }
    }

    fn poll(&self) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.poll(),
            FsImpl::Overlayfs(fs) => fs.poll(),
        }
    }

    fn notify_reply(&self) -> io::Result<()> {
        match self {
            FsImpl::Passthrough(fs) => fs.notify_reply(),
            FsImpl::Overlayfs(fs) => fs.notify_reply(),
        }
    }
}
