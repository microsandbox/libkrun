# Memory tracking regression and performance checks

This harness compares PR #121's original `afe3e80` with the bitmap/recovery fix. It uses the same dependency lockfile and release build for both variants. Build the fixed source with `--features bitmap`; build the original without that feature. The feature only enables topology configuration in the harness, not a production alternative implementation.

```sh
cargo build --release --manifest-path tests/memory-tracking/Cargo.toml --features bitmap
tests/memory-tracking/target/release/memory-tracking-live resident
tests/memory-tracking/target/release/memory-tracking-live repeated
tests/memory-tracking/target/release/memory-tracking-live scattered
```

The bookkeeping cases issue two million requests. Tracked requests write a payload and a queue-control range. `scattered` repeatedly visits 32,768 disjoint pages, deliberately exceeding the rejected 4,096-range design. The timed harvest includes the same memory-ledger range coalescing for both builds. Peak RSS includes the program, bookkeeping, and harvest output, not just the bitmap.

## Live VM checks

Use a fresh private copy of `examples/rust_vm/rootfs-minimal/aarch64` on macOS or `x86_64` on Linux. Set `TEST_ROOT` to that copy and `KRUNFW_PATH` to matching firmware. On macOS, codesign the exact binary with the hypervisor entitlement before running it.

```sh
TEST_ROOT=/absolute/private/rootfs KRUNFW_PATH=/absolute/firmware TEST_CPUS=2 tests/memory-tracking/target/release/memory-tracking-live vm
```

The VM has 256 MiB RAM. The harness captures/publishes a full baseline, runs a verified 32 MiB tmpfs write, captures a delta, overlays that delta on the baseline, and compares all 65,536 pages with a fresh full capture while vCPUs remain paused. `TEST_HOST_IO=1` instead reads 256 MiB from a host-backed sparse file through virtio-fs in 4 KiB guest reads. These timings include a diagnostic page-map sink; they are not snapshot archive or restore benchmarks. `full_ms` includes full capture and publication. `delta_ms` includes delta planning and capture. `pause_ms` measures entering the paused boundary, not the entire paused interval. Guest workload timing includes marker polling and scheduling.

The process exits on controller assertion failure, so a failed test does not leave its VM running. Do not reuse a rootfs containing previous `ready`, `go`, or `done` markers.

## macOS fault injection

`fault_interpose.c` is a test-only dynamic-library interposer. It fails the second `hv_vm_protect` after the harness creates `PR121_FAULT_FILE`, exercising a real partially completed backend operation without adding production fault hooks.

```sh
clang -dynamiclib -framework Hypervisor tests/memory-tracking/fault_interpose.c -o /tmp/pr121-fault.dylib
codesign --force -s - /tmp/pr121-fault.dylib
PR121_FAULT_FILE=/absolute/private/rootfs/inject DYLD_INSERT_LIBRARIES=/tmp/pr121-fault.dylib TEST_ROOT=/absolute/private/rootfs KRUNFW_PATH=/absolute/firmware tests/memory-tracking/target/release/memory-tracking-live vm
```

The original baseline fails the assertion that the old baseline is rejected. The fix requires a full rebase and then permits a fresh incremental capture. `PR121_FAULT_RESUME=1` additionally resumes and pauses after the error, testing mapping reconciliation before vCPU release. This verifies HVF error handling; it does not substitute for second-slot KVM/WHP fault tests.

## Results — 2026-09-06, local macOS ARM64/HVF

Five interleaved release-process samples per bookkeeping case; medians below. Same firmware and guest configuration for both VM variants.

| Measurement | Original #121 | Bitmap fix |
|---|---:|---:|
| Untracked: 2M requests | 11.725 ms | 11.699 ms |
| Repeated writes: requests only | 51.698 ms | 54.916 ms |
| Repeated writes: requests + harvest/coalescing | 66.475 ms | 54.927 ms |
| Scattered writes: requests only | 52.253 ms | 56.523 ms |
| Scattered writes: requests + harvest/coalescing | 78.431 ms | 56.740 ms |
| Repeated-write process peak RSS | 69.03 MiB | 5.94 MiB |
| Scattered-write process peak RSS | 69.53 MiB | 7.03 MiB |
| Retained bitmap for 4 GiB RAM | Not applicable; range log grows per request | 128 KiB plus region metadata |
| Live 256 MiB host-read workload, 2 vCPUs | 105.538 ms | 97.643 ms |
| Live host-read delta planning/capture | 15.570 ms | 16.055 ms |
| Live tmpfs full capture/publication, 2 vCPUs | 34.154 ms | 34.303 ms |
| Live tmpfs delta planning/capture, 2 vCPUs | 4.768 ms | 3.761 ms |
| Live tmpfs workload including markers | 72.677 ms | 85.037 ms |

The request-only tracking microbenchmarks increase by roughly 6–8%, while total request-plus-harvest cost falls by roughly 17–28%. The ordinary untracked case is unchanged within sample noise. VM timings are mixed, particularly the short tmpfs workload including marker polling; they do not establish universal throughput improvement or a latency bound. One earlier fixed-build full-capture outlier was 343 ms; the table uses the later interleaved matrix, not that earlier batch. More repetitions and remote qualification are required before claiming performance across platforms.

Validation completed: 72 device tests, 7 memory-ledger tests, formatting, Rust API Clippy with warnings denied, 32 normal live before/after VM runs across tmpfs and host-read workloads (all page comparisons passed), one original-build expected fault assertion, and four fixed-build fault recoveries including three resume-before-rebase cases. The initial harness attempt omitted BusyBox command prefixes and did not execute its workload; it was corrected and excluded. One baseline fault assertion initially stranded a paused test VM; that exact process was terminated and the harness now exits the entire process on panic.

Remaining: Linux KVM live comparison and multi-slot harvest/disable failure injection; Windows WHP live comparison and mapping-transition failure injection; Linux x86_64/aarch64 and TEE CI configurations; the original two-vCPU Linux integration failure. Source transfer to the designated remote test machines needs approval. Do not treat this local result as merge qualification.
