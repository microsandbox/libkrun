//! Bounded host-write tracking over registered RAM, excluding address-space holes.

use super::memory_access::HostMemoryRange;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const PAGE_SIZE: u64 = 4096;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

pub(super) struct DirtyBitmap {
    regions: Vec<Region>,
    pub(super) valid: bool,
}

struct Region {
    start: u64,
    end: u64,
    words: Vec<u64>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl DirtyBitmap {
    pub(super) fn new(mut ranges: Vec<HostMemoryRange>) -> Option<Self> {
        ranges.sort_unstable_by_key(|range| range.start);
        let mut regions = Vec::with_capacity(ranges.len());
        let mut previous_end = 0;
        for range in ranges {
            let end = range.start.checked_add(range.length)?;
            if range.length == 0 || range.start < previous_end {
                return None;
            }
            let count = usize::try_from(range.length.div_ceil(PAGE_SIZE).div_ceil(64)).ok()?;
            let mut words = Vec::new();
            words.try_reserve_exact(count).ok()?;
            words.resize(count, 0);
            regions.push(Region {
                start: range.start,
                end,
                words,
            });
            previous_end = end;
        }
        if regions.is_empty() {
            return None;
        }
        Some(Self {
            regions,
            valid: true,
        })
    }

    pub(super) fn clear(&mut self) {
        for region in &mut self.regions {
            region.words.fill(0);
        }
        self.valid = true;
    }

    pub(super) fn mark(&mut self, range: HostMemoryRange) {
        if range.length == 0 {
            return;
        }
        let Some(end) = range.start.checked_add(range.length) else {
            self.valid = false;
            return;
        };
        let mut cursor = range.start;
        for region in &mut self.regions {
            if region.end <= cursor {
                continue;
            }
            if region.start > cursor {
                break;
            }
            let limit = end.min(region.end);
            let first = (cursor - region.start) / PAGE_SIZE;
            let last = (limit - 1 - region.start) / PAGE_SIZE;
            // Mark whole words for large writes; duplicate writes allocate nothing.
            for word in first / 64..=last / 64 {
                let low = if word == first / 64 { first % 64 } else { 0 };
                let high = if word == last / 64 { last % 64 } else { 63 };
                region.words[word as usize] |= (u64::MAX << low) & (u64::MAX >> (63 - high));
            }
            cursor = limit;
            if cursor == end {
                return;
            }
        }
        // A writer outside the frozen topology is a correctness failure, never silent loss.
        self.valid = false;
    }

    pub(super) fn take(&mut self) -> Vec<HostMemoryRange> {
        let mut result: Vec<HostMemoryRange> = Vec::new();
        for region in &mut self.regions {
            for (index, word) in region.words.iter_mut().enumerate() {
                let mut bits = std::mem::take(word);
                while bits != 0 {
                    let first = bits.trailing_zeros();
                    let count = (bits >> first).trailing_ones();
                    let start = region.start + (index as u64 * 64 + u64::from(first)) * PAGE_SIZE;
                    let end = start + (u64::from(count) * PAGE_SIZE).min(region.end - start);
                    if let Some(previous) =
                        result.last_mut().filter(|p| p.start + p.length == start)
                    {
                        previous.length = end - previous.start;
                    } else {
                        result.push(HostMemoryRange {
                            start,
                            length: end - start,
                        });
                    }
                    if first + count == 64 {
                        break;
                    }
                    bits &= u64::MAX << (first + count);
                }
            }
        }
        result
    }

    #[cfg(test)]
    pub(super) fn bytes(&self) -> usize {
        self.regions
            .iter()
            .map(|region| region.words.len() * 8)
            .sum()
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_and_fragmented_writes_stay_bounded() {
        let mut bitmap = DirtyBitmap::new(vec![HostMemoryRange {
            start: 0,
            length: 4 << 30,
        }])
        .unwrap();
        assert_eq!(bitmap.bytes(), 128 * 1024);
        for index in 0..1_000_000 {
            bitmap.mark(HostMemoryRange {
                start: (index % 32768) * 8192 + 1,
                length: 1,
            });
        }
        assert!(bitmap.valid);
        assert_eq!(bitmap.bytes(), 128 * 1024);
        let ranges = bitmap.take();
        assert_eq!(ranges.len(), 32768);
        assert!(ranges.iter().all(|r| r.length == 4096));
        assert!(bitmap.take().is_empty());
    }

    #[test]
    fn clips_partial_pages_and_never_captures_holes() {
        let mut bitmap = DirtyBitmap::new(vec![
            HostMemoryRange {
                start: 7,
                length: 8193,
            },
            HostMemoryRange {
                start: 16384,
                length: 4096,
            },
        ])
        .unwrap();
        bitmap.mark(HostMemoryRange {
            start: 4095,
            length: 4105,
        });
        assert_eq!(
            bitmap.take(),
            vec![HostMemoryRange {
                start: 7,
                length: 8193
            }]
        );
        bitmap.mark(HostMemoryRange {
            start: 8199,
            length: 9000,
        });
        assert!(!bitmap.valid);
        bitmap.clear();
        bitmap.mark(HostMemoryRange {
            start: u64::MAX,
            length: 2,
        });
        assert!(!bitmap.valid);
    }
}
