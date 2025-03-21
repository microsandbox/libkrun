use devices::virtio::fs::FsImplShare;

#[derive(Clone, Debug)]
pub struct FsDeviceConfig {
    pub fs_id: String,
    pub fs_share: FsImplShare,
    pub shm_size: Option<usize>,
}
