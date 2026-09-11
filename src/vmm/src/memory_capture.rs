//! Bounded memory streaming with explicit coverage for known-zero unplugged blocks.

#[cfg(not(feature = "tee"))]
use devices::virtio::{Mem, MmioTransport, TYPE_MEM};
#[cfg(not(feature = "tee"))]
use devices::DeviceType;
#[cfg(not(feature = "tee"))]
use vm_memory::{GuestAddress, GuestMemoryBackend};

use crate::memory_state::{
    GuestMemoryRange, MemoryCaptureOptions, MemoryCaptureSink, MemoryCaptureStats,
};
use crate::{Error, Result, Vmm};

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Read actual device occupancy only after the caller has paused CPUs and drained host requests.
#[cfg(not(feature = "tee"))]
pub(crate) fn unplugged_ranges(vmm: &Vmm) -> Result<Vec<GuestMemoryRange>> {
    let mut unplugged = Vec::new();
    // Construction installs at most one virtio-mem device with this stable id. Looking it up
    // directly keeps memory capture available even when block-device checkpoint support is off.
    if let Some(bus) = vmm
        .mmio_device_manager
        .get_device(DeviceType::Virtio(TYPE_MEM), "virtio_mem")
    {
        let mut bus = bus
            .lock()
            .map_err(|_| Error::DeviceState("MMIO device mutex is poisoned".into()))?;
        let transport = bus
            .as_mut_any()
            .downcast_mut::<MmioTransport>()
            .ok_or_else(|| Error::DeviceState("memory device is not on virtio-mmio".into()))?;
        let device = transport.locked_device();
        let memory = device
            .as_any()
            .downcast_ref::<Mem>()
            .ok_or_else(|| Error::DeviceState("invalid virtio-mem device type".into()))?;
        for range in memory.unplugged_ranges() {
            let length = usize::try_from(range.length).map_err(|_| {
                Error::DeviceState("unplugged memory exceeds host address space".into())
            })?;
            if !vmm
                .guest_memory
                .check_range(GuestAddress(range.start), length)
            {
                return Err(Error::DeviceState(
                    "unplugged memory is outside registered RAM".into(),
                ));
            }
            unplugged.push(
                GuestMemoryRange::new(range.start, range.length).map_err(Error::MemoryState)?,
            );
        }
    }
    unplugged.sort_unstable_by_key(|range| range.start());
    for pair in unplugged.windows(2) {
        if pair[0].start() + pair[0].length() > pair[1].start() {
            return Err(Error::DeviceState("unplugged memory ranges overlap".into()));
        }
    }
    Ok(unplugged)
}

#[cfg(feature = "tee")]
pub(crate) fn unplugged_ranges(_vmm: &Vmm) -> Result<Vec<GuestMemoryRange>> {
    // TEE configurations do not instantiate virtio-mem or reserve its hotplug window.
    Ok(Vec::new())
}

/// Streams sorted capture ranges against a same-epoch, sorted unplugged inventory. Both lists
/// describe registered RAM. Zero extents are emitted even for deltas, so a semantic discard
/// overrides bytes inherited from an older generation instead of leaving a hole in the delta.
pub(crate) fn stream_memory(
    ranges: &[GuestMemoryRange],
    unplugged: &[GuestMemoryRange],
    options: MemoryCaptureOptions,
    mut read: impl FnMut(GuestMemoryRange, &mut [u8]) -> Result<()>,
    sink: &mut dyn MemoryCaptureSink,
) -> Result<MemoryCaptureStats> {
    let mut buffer = vec![0_u8; options.chunk_size()];
    let mut stats = MemoryCaptureStats::default();
    let mut zero_index = 0;
    for range in ranges {
        let mut start = range.start();
        let end = range.start() + range.length();
        while start < end {
            while zero_index < unplugged.len()
                && unplugged[zero_index].start() + unplugged[zero_index].length() <= start
            {
                zero_index += 1;
            }
            let mut chunk_end = end.min(start.saturating_add(options.chunk_size() as u64));
            let mut known_zero = false;
            if let Some(zero) = unplugged.get(zero_index) {
                if zero.start() <= start {
                    chunk_end = chunk_end.min(zero.start() + zero.length());
                    known_zero = true;
                } else {
                    chunk_end = chunk_end.min(zero.start());
                }
            }
            let length = chunk_end - start;
            let chunk = GuestMemoryRange::new(start, length)
                .expect("nonempty subsets of validated capture ranges are bounded");
            if known_zero {
                // Do not even expose a source-memory pointer for unplugged capacity. The zero
                // promise belongs to the device ownership contract, independent of zero scanning.
                sink.write_zero(chunk).map_err(Error::MemorySink)?;
                stats.zero_bytes += length;
                stats.unplugged_bytes_skipped += length;
            } else {
                let bytes = &mut buffer[..length as usize];
                read(chunk, bytes)?;
                stats.guest_bytes_read += length;
                if options.detects_zero() && bytes.iter().all(|byte| *byte == 0) {
                    sink.write_zero(chunk).map_err(Error::MemorySink)?;
                    stats.zero_bytes += length;
                } else {
                    sink.write_bytes(chunk, bytes).map_err(Error::MemorySink)?;
                    stats.emitted_bytes += length;
                }
            }
            stats.logical_bytes += length;
            stats.chunks += 1;
            start = chunk_end;
        }
    }
    Ok(stats)
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[derive(Default)]
    struct ImageSink {
        bytes: Vec<u8>,
        ranges: Vec<GuestMemoryRange>,
        fail_after: Option<usize>,
    }

    impl ImageSink {
        fn check(&mut self, range: GuestMemoryRange) -> io::Result<()> {
            if self.fail_after == Some(self.ranges.len()) {
                return Err(io::Error::other("injected sink failure"));
            }
            self.ranges.push(range);
            let end = (range.start() + range.length()) as usize;
            self.bytes.resize(self.bytes.len().max(end), 0xcc);
            Ok(())
        }
    }

    impl MemoryCaptureSink for ImageSink {
        fn write_bytes(&mut self, range: GuestMemoryRange, bytes: &[u8]) -> io::Result<()> {
            self.check(range)?;
            self.bytes[range.start() as usize..(range.start() + range.length()) as usize]
                .copy_from_slice(bytes);
            Ok(())
        }

        fn write_zero(&mut self, range: GuestMemoryRange) -> io::Result<()> {
            self.check(range)?;
            self.bytes[range.start() as usize..(range.start() + range.length()) as usize].fill(0);
            Ok(())
        }
    }

    fn range(start: u64, length: u64) -> GuestMemoryRange {
        GuestMemoryRange::new(start, length).unwrap()
    }

    #[test]
    fn every_noncontiguous_inventory_skips_only_unplugged_bytes() {
        // Exhaustively vary occupancy and chunk alignment. The two registered regions have a
        // guest-physical hole, and plugged blocks must be read even when their contents are zero.
        for mask in 0..512u16 {
            let unplugged = (0..9)
                .filter(|index| mask & (1 << index) == 0)
                .map(|index| range(32 + index * 4, 4))
                .collect::<Vec<_>>();
            for chunk_size in [1, 3, 4, 7, 32] {
                for detect_zero in [false, true] {
                    let mut sink = ImageSink::default();
                    let stats = stream_memory(
                        &[range(0, 16), range(32, 36)],
                        &unplugged,
                        MemoryCaptureOptions::new(chunk_size, detect_zero).unwrap(),
                        |chunk, bytes| {
                            for (offset, byte) in bytes.iter_mut().enumerate() {
                                let address = chunk.start() + offset as u64;
                                assert!(address < 16 || mask & (1 << ((address - 32) / 4)) != 0);
                                // Include genuine zero bytes inside plugged blocks; those still
                                // require source reads regardless of the zero-scanning option.
                                *byte = if address % 2 == 0 { 0 } else { 0xa5 };
                            }
                            Ok(())
                        },
                        &mut sink,
                    )
                    .unwrap();
                    let skipped = unplugged.len() as u64 * 4;
                    assert_eq!(stats.logical_bytes, 52);
                    assert_eq!(stats.guest_bytes_read, 52 - skipped);
                    assert_eq!(stats.unplugged_bytes_skipped, skipped);
                    assert_eq!(stats.zero_bytes + stats.emitted_bytes, 52);
                    for address in 0..68usize {
                        let expected = if (16..32).contains(&address) {
                            0xcc
                        } else if address >= 32 && mask & (1 << ((address - 32) / 4)) == 0
                            || address % 2 == 0
                        {
                            0
                        } else {
                            0xa5
                        };
                        assert_eq!(
                            sink.bytes[address], expected,
                            "mask={mask}, address={address}"
                        );
                    }
                    assert!(sink
                        .ranges
                        .iter()
                        .all(|range| range.length() <= chunk_size as u64));
                }
            }
        }
    }

    #[test]
    fn shrink_delta_overrides_nonzero_base_and_replug_wins_on_retry() {
        let baseline = vec![0xa5; 80];
        let mut shrunk = ImageSink {
            bytes: baseline.clone(),
            ..Default::default()
        };
        let stats = stream_memory(
            &[range(16, 16), range(48, 16)],
            &[range(16, 16), range(48, 16)],
            MemoryCaptureOptions::new(7, false).unwrap(),
            |_, _| panic!("discard delta read unplugged memory"),
            &mut shrunk,
        )
        .unwrap();
        assert_eq!(stats.guest_bytes_read, 0);
        assert_eq!(stats.unplugged_bytes_skipped, 32);
        assert!(shrunk.bytes[16..32]
            .iter()
            .chain(&shrunk.bytes[48..64])
            .all(|byte| *byte == 0));
        assert_eq!(&shrunk.bytes[32..48], &baseline[32..48]);

        // One block was replugged and written before this epoch, while the other remains zero.
        // Inject a sink failure, then retry the identical inventory against a fresh base clone.
        let ranges = [range(16, 16), range(48, 16)];
        let unplugged = [range(48, 16)];
        let options = MemoryCaptureOptions::new(7, true).unwrap();
        let mut failed = ImageSink {
            bytes: shrunk.bytes.clone(),
            fail_after: Some(1),
            ..Default::default()
        };
        assert!(stream_memory(
            &ranges,
            &unplugged,
            options,
            |_, bytes| {
                bytes.fill(0x5a);
                Ok(())
            },
            &mut failed
        )
        .is_err());
        let mut retry = ImageSink {
            bytes: shrunk.bytes.clone(),
            ..Default::default()
        };
        let stats = stream_memory(
            &ranges,
            &unplugged,
            options,
            |chunk, bytes| {
                assert!(chunk.start() < 32);
                bytes.fill(0x5a);
                Ok(())
            },
            &mut retry,
        )
        .unwrap();
        assert_eq!(stats.guest_bytes_read, 16);
        assert_eq!(stats.unplugged_bytes_skipped, 16);
        assert!(retry.bytes[16..32].iter().all(|byte| *byte == 0x5a));
        assert!(retry.bytes[48..64].iter().all(|byte| *byte == 0));
        assert!(baseline.iter().all(|byte| *byte == 0xa5));
        assert!(shrunk.bytes[16..32].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn unplugged_ceiling_adds_coverage_without_guest_reads() {
        struct CountingSink(u64);
        impl MemoryCaptureSink for CountingSink {
            fn write_bytes(&mut self, _: GuestMemoryRange, _: &[u8]) -> io::Result<()> {
                panic!("unused ceiling emitted bytes")
            }
            fn write_zero(&mut self, range: GuestMemoryRange) -> io::Result<()> {
                self.0 += range.length();
                Ok(())
            }
        }
        for capacity in [8u64 << 30, 16u64 << 30, 32u64 << 30] {
            let mut sink = CountingSink(0);
            let ranges = [range(8 << 30, capacity)];
            let stats = stream_memory(
                &ranges,
                &ranges,
                MemoryCaptureOptions::default(),
                |_, _| panic!("unused ceiling read source memory"),
                &mut sink,
            )
            .unwrap();
            assert_eq!(stats.guest_bytes_read, 0);
            assert_eq!(stats.unplugged_bytes_skipped, capacity);
            assert_eq!(stats.logical_bytes, capacity);
            assert_eq!(sink.0, capacity);
        }
    }

    #[test]
    fn incremental_subranges_clip_unplugged_ranges_exactly() {
        let mut sink = ImageSink::default();
        let stats = stream_memory(
            &[range(19, 3), range(30, 12), range(90, 1)],
            &[range(0, 8), range(16, 16), range(80, 16)],
            MemoryCaptureOptions::new(64, false).unwrap(),
            |chunk, bytes| {
                assert_eq!(chunk, range(32, 10));
                bytes.fill(0x5a);
                Ok(())
            },
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            sink.ranges,
            [range(19, 3), range(30, 2), range(32, 10), range(90, 1)]
        );
        assert_eq!(stats.logical_bytes, 16);
        assert_eq!(stats.guest_bytes_read, 10);
        assert_eq!(stats.unplugged_bytes_skipped, 6);
    }
}
