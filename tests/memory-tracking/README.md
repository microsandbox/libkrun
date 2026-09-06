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

## Remote results — 2026-09-06

Source transfer was explicitly authorized. Tests used isolated source copies on the OVH x86_64/KVM host and Surface ARM64/WHP host without modifying existing checkouts. Both used identical harness code for original and fixed builds. Windows RSS is explicitly unavailable rather than reported as zero.

The corrected harness merges byte-granular deltas into pages. The earlier sink assumed page alignment and falsely reported one/two-page baseline mismatches on these hosts; those failures are excluded. The original macOS timing table above predates this sink correction and must not be pooled with the following measurements. Linux also required rebuilding the embedded init for x86_64: the copied ARM64 init caused guest boot failures with exit status zero. Only explicit page-comparison result records count as passes.

Five samples per variant and case; medians in milliseconds:

| Host / workload | Full original → fixed | Delta original → fixed | Workload original → fixed | Pause entry original → fixed |
|---|---:|---:|---:|---:|
| Linux, 1 CPU, tmpfs | 93.571 → 93.153 | 3.876 → 3.835 | 184.789 → 174.663 | 0.026 → 0.024 |
| Linux, 2 CPUs, tmpfs | 94.600 → 93.837 | 3.949 → 3.961 | 188.859 → 186.860 | 0.028 → 0.027 |
| Linux, 1 CPU, host reads | 95.311 → 94.428 | 28.183 → 26.923 | 486.817 → 486.458 | 0.028 → 0.026 |
| Linux, 2 CPUs, host reads | 94.509 → 94.212 | 28.408 → 26.836 | 452.945 → 447.700 | 0.029 → 0.026 |
| Windows, 1 CPU, tmpfs | 138.360 → 139.709 | 10.829 → 9.836 | 475.327 → 470.243 | 0.062 → 0.065 |
| Windows, 2 CPUs, tmpfs | 134.146 → 134.742 | 10.030 → 9.245 | 354.409 → 346.324 | 0.124 → 0.124 |

All 40 corrected Linux VM runs and 20 corrected Windows tmpfs runs passed baseline-plus-delta equality against a fresh full capture. Linux compared 74,896 pages including additional mapped firmware RAM; Windows compared 65,536 pages. Four short Linux fault-injection processes overlapped part of the live benchmark batch, so these measurements are diagnostic rather than controlled latency guarantees.

`fault_kvm.c` is an LD_PRELOAD test interposer: compile with `cc -shared -fPIC fault_kvm.c -ldl -o fault_kvm.so`. With `PR121_FAULT_FILE` set, it fails the second `KVM_GET_DIRTY_LOG` after the first has already succeeded. Both original-build tests reproduce the invalid-baseline bug. Both fixed tests reject the old baseline, complete a full rebase, and accept a fresh incremental capture; one additionally resumes before rebasing (`PR121_FAULT_RESUME=1`). This exercises actual multi-slot KVM harvesting, not a simulated ledger-only failure.

Windows host-read qualification is incomplete: all ten fixed host-read attempts stopped when the guest reported `dd: /hostdata: Permission denied`. This has not been attributed to the tracking patch and must not be counted as a passing workload. Windows mapping-transition failure injection, KVM disable-failure injection, Linux ARM64 configuration checks, and the original CI two-vCPU integration test remain outstanding. The isolated two-vCPU KVM runs above pass, but are not a rerun of that CI test. Do not treat these results as complete merge qualification.

Linux `cargo clippy -p msb_krun -- -D warnings`, and the same command with `--features amd-sev` and `--features tdx`, pass. The TEE checks uncovered missing guards on the `VmPauseGeneration` implementation and `std::io` import; these were corrected without changing non-TEE execution. The harness partial-page merge regression test and formatting checks also pass.

## Follow-up: partial-disable and initial-arm faults

For integrated device/memory recovery, build with `--features bitmap,devices`. The harness quiesces supported devices before injecting the memory error, so resume must reconcile mappings before reopening console, filesystem, VM-generation, and metrics workers. The minimal filesystem's known durable-state serialization error is accepted only after it has been quiesced; all other capture errors fail the test. This validates recovery from that failed capture, not durable filesystem serialization.

The integrated ordering passed seven local macOS/HVF live runs (one initial two-vCPU run, then three repetitions each with one/two vCPUs), comparing 65,536 pages with zero mismatches after fault recovery. `cargo clippy -p msb_krun --features blk --offline -- -D warnings` passed. Integrated Linux/Windows runs remain pending source-transfer approval; earlier #121-only tests are not substitutes for this combined device-worker path.

The previously outstanding Linux disable and Windows mapping-transition tests have now been run live. Production source remains unchanged by this follow-up; Windows fault instrumentation is supplied as an inert patch to apply only to an isolated test copy.

| Live case | Runs | Result |
|---|---:|---|
| KVM fails second slot while disabling dirty tracking | 12 | Pass; old baseline rejected, full rebase and resumed delta equal fresh full RAM |
| WHP fails replacement mapping after successful unmap while disabling tracking | 12 | Pass; old baseline rejected, mappings recovered, resumed delta equal fresh full RAM |
| WHP fails initial tracking-enable replacement mapping | 6 | Pass; failed candidate abandoned, resume succeeds, new full capture/publication succeeds |

Each disable matrix covers one/two vCPUs, rebase-before-resume and resume-before-rebase, with three repetitions per combination. Initial-arm tests cover one/two vCPUs with three repetitions each. Disable recovery compares 74,896 pages on Linux and 65,536 on Windows, with zero mismatches. The recovery timing includes a full diagnostic capture, deliberate guest run time, delta capture, a second full verification capture, and comparisons; it is not a production pause-latency measurement.

End-to-end diagnostic recovery medians were 214.09 ms on Linux (204.55–243.70 ms) and 286.06 ms on Windows (260.54–375.02 ms). These are failure-path test timings, not a change in ordinary snapshot latency.

For Linux, build `fault_kvm.c` as above and set `PR121_FAULT_KIND=disable` alongside `PR121_FAULT_FILE` and `LD_PRELOAD`. The second `KVM_SET_USER_MEMORY_REGION` without dirty logging fails after the first has already succeeded. For Windows, apply `fault_whp.patch` to an isolated source copy, build the native ARM64 harness, and set `PR121_FAULT_KIND=disable` and `PR121_FAULT_FILE`. It adds an invalid flag to the actual WHP map call only after the harness arms the marker; WHP rejects the call after the real preceding unmap. `PR121_FAULT_ARM=1` instead arms the marker just before the first full publication. Never apply this patch to production sources.

The separate Windows host-read precondition still fails: a 4 KiB read before the ready marker (before any pause, capture, or tracking enablement) returns `Permission denied`, despite guest stat reporting mode 0777 and uid/gid 0. Diagnostic-only logging did not identify an error in the Windows host-open/read helpers. This is unresolved filesystem coverage, not a failed memory-tracking transition test. No filesystem permissions were relaxed to obtain a pass.
