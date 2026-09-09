# Private memory construction probes

The construction API accepts a read-only immutable file plus guest address spans. The caller must exclude mutation or truncation through other handles for the lifetime of every mapping. A pathname is not lifetime ownership; mappings retain the file handle after unlinking.

The current implementation is groundwork, not a fully qualified sandbox policy. It requires execution restore, excludes a simultaneous eager memory source, preserves VMM slot boundaries, and suppresses cold-boot RAM writes after installing the backing. NUMA placement, balloon, and virtio-mem combinations are temporarily rejected until backing-aware discard and placement are implemented. Those restrictions must not be mistaken for completed memory-resize support.

Run mapper tests with:

```sh
cargo test --locked -p msb_krun_vmm --features blk private_memory --lib
```

On Apple Silicon, compile the native HVF probe and sign it with an entitlement plist granting `com.apple.security.hypervisor`:

```sh
clang -O2 -Wall -Wextra tests/private-memory/hvf_probe.c -framework Hypervisor -o /tmp/hvf-private-memory-probe
codesign --entitlements "$ENTITLEMENTS" --force -s - /tmp/hvf-private-memory-probe
/tmp/hvf-private-memory-probe
```

The probe creates two independent VM processes backed by the same unlinked, read-only 256 MiB sparse file. Both register RAM before either guest writes. It checks distinct guest writes, host writes to a sparse-zero page, sibling isolation, immutable file contents, and orderly teardown. `mapping_us` includes two private mappings and HVF VM creation/registration; `run_us` covers one guest store and its HVC exit. Neither number measures full sandbox restore, useful-work readiness, working-set sharing under pressure, or first/repeated snapshot capture.

The separate clock-only control API requires the guest's new clock-only capability. Old kernels are refused rather than receiving a clone notification on ordinary resume. State containing the new capability bit is rejected by older VMM state decoders; this change does not make cross-version full state portable.
