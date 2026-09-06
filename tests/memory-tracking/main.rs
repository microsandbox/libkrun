//! Run each VM case in a separate process; libkrun owns process termination.
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use devices::virtio::{HostMemoryRange, MemoryAccessDomain};
use msb_krun::{
    GuestMemoryRange, IncrementalCaptureDecision, MemoryCaptureOptions, MemoryCaptureSink,
    VmBuilder,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Default)]
struct Memory(BTreeMap<u64, Vec<u8>>);

impl MemoryCaptureSink for Memory {
    fn write_bytes(&mut self, range: GuestMemoryRange, bytes: &[u8]) -> io::Result<()> {
        for (index, page) in bytes.chunks(4096).enumerate() {
            self.0
                .insert(range.start() + index as u64 * 4096, page.to_vec());
        }
        Ok(())
    }
    fn write_zero(&mut self, range: GuestMemoryRange) -> io::Result<()> {
        self.write_bytes(range, &vec![0; range.length() as usize])
    }
}

fn bookkeeping(mode: &str) -> Result<()> {
    let domain = MemoryAccessDomain::new();
    let participant = domain.register_participant();
    if mode != "resident" {
        domain.freeze(Duration::from_secs(1))?;
        #[cfg(feature = "bitmap")]
        domain.configure_tracking(vec![HostMemoryRange {
            start: 0,
            length: 4 << 30,
        }])?;
        domain.begin_tracking()?;
    }
    let iterations = 2_000_000_u64;
    let begin = Instant::now();
    for index in 0..iterations {
        let address = if mode == "scattered" {
            (index % 32768) * 8192
        } else {
            4096
        };
        assert!(participant.begin_request(|| vec![
            HostMemoryRange {
                start: address,
                length: 4096
            },
            HostMemoryRange {
                start: 1 << 30,
                length: 128
            },
        ]));
        participant.end_request();
    }
    let request_ms = begin.elapsed().as_secs_f64() * 1000.;
    domain.freeze(Duration::from_secs(1))?;
    let begin = Instant::now();
    let ranges = domain.take_dirty_ranges();
    let raw_ranges = ranges.len();
    let mut ledger = vmm::memory_state::MemoryGenerationLedger::new(
        vmm::memory_state::MemoryTopologyGeneration::new(1),
    );
    let full = ledger.plan_full_capture()?;
    let base = ledger.publish(&full)?;
    ledger.retain_dirty_coverage(base)?;
    let plan = ledger.plan_incremental_capture(
        base,
        ranges
            .into_iter()
            .map(|range| GuestMemoryRange::new(range.start, range.length).unwrap()),
    )?;
    std::hint::black_box(&plan);
    let harvest_ms = begin.elapsed().as_secs_f64() * 1000.;
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
    let rss_bytes = if cfg!(target_os = "macos") {
        usage.ru_maxrss as u64
    } else {
        usage.ru_maxrss as u64 * 1024
    };
    println!("mode={mode} iterations={iterations} request_ms={request_ms:.3} harvest_ms={harvest_ms:.3} ranges={raw_ranges} peak_rss_bytes={rss_bytes}");
    Ok(())
}

fn main() -> Result<()> {
    // Assertions run on the controller thread. Terminate the whole test VM on failure,
    // rather than leaving its vCPUs alive after only the controller unwinds.
    let panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        panic_hook(info);
        std::process::exit(1);
    }));
    let mode = std::env::args().nth(1).unwrap_or_else(|| "vm".into());
    if mode != "vm" {
        return bookkeeping(&mode);
    }
    let root = PathBuf::from(std::env::var("TEST_ROOT")?);
    if std::env::var_os("TEST_HOST_IO").is_some() {
        fs::File::create(root.join("hostdata"))?.set_len(256 * 1024 * 1024)?;
    }
    let cpus: u8 = std::env::var("TEST_CPUS")
        .unwrap_or_else(|_| "2".into())
        .parse()?;
    let firmware = std::env::var("KRUNFW_PATH")?;
    let vm = VmBuilder::new()
        .machine(|m| m.vcpus(cpus).memory_mib(256).balloon(false).rng(false))
        .kernel(|k| k.krunfw_path(firmware))
        .fs(|f| f.root(&root))
        .console(|c| c.output(root.join("console.log")))
        .exec(|e| e.path("/bin/busybox").args(["sh", "-c",
            "set -eu; /bin/busybox mkdir -p /ram; /bin/busybox mount -t tmpfs tmpfs /ram; echo ready > /ready; while [ ! -e /go ]; do /bin/busybox sleep 0.01; done; if [ -f /hostdata ]; then /bin/busybox dd if=/hostdata of=/dev/null bs=4096 count=65536; else /bin/busybox dd if=/dev/zero of=/ram/data bs=1M count=32; test $(/bin/busybox wc -c < /ram/data) -eq 33554432; fi; echo done > /done; while :; do :; done"]))
        .build()?;
    let control = vm.control_handle();
    let exit = vm.exit_handle();
    thread::spawn(move || {
        let result = (|| -> Result<()> {
            control.wait_until_running(Duration::from_secs(20))?;
            wait_file(&root.join("ready"))?;
            let pause = control.pause()?;
            let options = MemoryCaptureOptions::new(4096, false)?;
            let mut memory = Memory::default();
            let begin = Instant::now();
            let full = control.plan_full_memory_capture()?;
            control.capture_memory(&full, options, &mut memory)?;
            let baseline = control.publish_memory_capture(&full)?;
            let full_ms = begin.elapsed().as_secs_f64() * 1000.;
            control.resume(pause)?;
            let begin = Instant::now();
            fs::write(root.join("go"), b"go")?;
            wait_file(&root.join("done"))?;
            let workload_ms = begin.elapsed().as_secs_f64() * 1000.;
            // Guest performs no more host I/O after this marker.
            thread::sleep(Duration::from_millis(100));
            let begin = Instant::now();
            let pause = control.pause()?;
            let pause_ms = begin.elapsed().as_secs_f64() * 1000.;
            if let Ok(path) = std::env::var("PR121_FAULT_FILE") {
                fs::write(&path, b"armed")?;
                assert!(
                    control
                        .plan_incremental_memory_capture_with_threshold(baseline, 100)
                        .is_err(),
                    "fault did not fire"
                );
                assert!(
                    matches!(
                        control.plan_incremental_memory_capture(baseline)?,
                        IncrementalCaptureDecision::FullRequired(_)
                    ),
                    "failed harvest left baseline usable"
                );
                let pause = if std::env::var_os("PR121_FAULT_RESUME").is_some() {
                    control.resume(pause)?;
                    thread::sleep(Duration::from_millis(10));
                    control.pause()?
                } else {
                    pause
                };
                let full = control.plan_full_memory_capture()?;
                let mut recovered = Memory::default();
                control.capture_memory(&full, options, &mut recovered)?;
                let fresh = control.publish_memory_capture(&full)?;
                let delta =
                    match control.plan_incremental_memory_capture_with_threshold(fresh, 100)? {
                        IncrementalCaptureDecision::Incremental(plan) => plan,
                        other => return Err(format!("rearmed tracker rejected: {other:?}").into()),
                    };
                control.capture_memory(&delta, options, &mut recovered)?;
                control.abandon_memory_capture(&delta)?;
                control.release_memory_baseline()?;
                control.resume(pause)?;
                println!("fault_recovery=pass old_baseline=rejected full_rebase=pass tracking_rearmed=pass");
                return Ok(());
            }
            let begin = Instant::now();
            let delta =
                match control.plan_incremental_memory_capture_with_threshold(baseline, 100)? {
                    IncrementalCaptureDecision::Incremental(plan) => plan,
                    other => return Err(format!("unexpected delta decision: {other:?}").into()),
                };
            let stats = control.capture_memory(&delta, options, &mut memory)?;
            let delta_ms = begin.elapsed().as_secs_f64() * 1000.;
            control.abandon_memory_capture(&delta)?;
            let complete = control.plan_full_memory_capture()?;
            let mut check = Memory::default();
            control.capture_memory(&complete, options, &mut check)?;
            let mismatches = check
                .0
                .iter()
                .filter(|(address, bytes)| memory.0.get(address) != Some(bytes))
                .count();
            assert_eq!(mismatches, 0, "baseline plus delta differs from full RAM");
            control.abandon_memory_capture(&complete)?;
            control.release_memory_baseline()?;
            control.resume(pause)?;
            println!("vm cpus={cpus} full_ms={full_ms:.3} workload_ms={workload_ms:.3} pause_ms={pause_ms:.3} delta_ms={delta_ms:.3} delta_bytes={} compared_pages={} mismatches={mismatches}", stats.logical_bytes, check.0.len());
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("LIVE FAILURE: {error}");
            std::process::exit(1);
        }
        exit.trigger();
    });
    vm.enter()?;
    Ok(())
}

fn wait_file(path: &std::path::Path) -> Result<()> {
    let begin = Instant::now();
    while !path.exists() {
        if begin.elapsed() > Duration::from_secs(30) {
            return Err(format!("timeout: {}", path.display()).into());
        }
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}
