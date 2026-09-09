//! WHP x86-64 state, completed by the controller while every worker is stopped.

use serde::{Deserialize, Serialize};
use windows_sys::Win32::System::Hypervisor::*;

use super::{ApParkState, ApStartupRouter, Error, Result};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct VmState {
    features: Vec<Vec<u8>>,
    tsc_frequency: u64,
    startup: Vec<ApParkState>,
}

#[derive(Serialize, Deserialize)]
struct CpuState {
    registers: Vec<(WHV_REGISTER_NAME, [u8; 16])>,
    xsave: Vec<u8>,
    apic: Vec<u8>,
    synic: [Vec<u8>; 3],
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

pub(super) fn capture_vm(
    partition: WHV_PARTITION_HANDLE,
    router: &ApStartupRouter,
) -> Result<Vec<u8>> {
    let startup = router
        .slots
        .iter()
        .map(|slot| {
            slot.lock()
                .map(|state| *state)
                .map_err(|_| codec("startup state mutex poisoned"))
        })
        .collect::<Result<Vec<_>>>()?;
    encode(&VmState {
        features: features(partition)?,
        tsc_frequency: frequency()?,
        startup,
    })
}

pub(super) fn restore_vm(
    partition: WHV_PARTITION_HANDLE,
    router: &ApStartupRouter,
    bytes: &[u8],
) -> Result<()> {
    let state: VmState = decode(bytes)?;
    if state.features != features(partition)?
        || state.tsc_frequency != frequency()?
        || state.startup.len() != router.slots.len()
        || state.startup.first() != Some(&ApParkState::Running)
    {
        return Err(codec(
            "WHP x86 processor features, frequency or topology differ",
        ));
    }
    // A newly constructed partition has never run. Explicitly freeze before
    // installing TSC/APIC state so construction latency cannot age the snapshot.
    super::set_partition_time_running(partition, false)?;
    for (slot, value) in router.slots.iter().zip(state.startup) {
        *slot
            .lock()
            .map_err(|_| codec("startup state mutex poisoned"))? = value;
    }
    Ok(())
}

pub(super) fn capture_cpu(partition: WHV_PARTITION_HANDLE, id: u32) -> Result<Vec<u8>> {
    let cpu = u8::try_from(id).map_err(codec)?;
    let names = register_names(partition, cpu)?;
    let values = super::get_vcpu_register_bytes(partition, cpu, &names)?;
    let get = |kind| super::get_virtual_processor_state(partition, id, kind);
    encode(&CpuState {
        registers: names.into_iter().zip(values).collect(),
        xsave: get(WHvVirtualProcessorStateTypeXsaveState)?,
        apic: get(WHvVirtualProcessorStateTypeInterruptControllerState2)?,
        synic: [
            get(WHvVirtualProcessorStateTypeSynicMessagePage)?,
            get(WHvVirtualProcessorStateTypeSynicEventFlagPage)?,
            get(WHvVirtualProcessorStateTypeSynicTimerState)?,
        ],
    })
}

pub(super) fn restore_cpu(partition: WHV_PARTITION_HANDLE, id: u32, bytes: &[u8]) -> Result<()> {
    let cpu = u8::try_from(id).map_err(codec)?;
    let state: CpuState = decode(bytes)?;
    let names = register_names(partition, cpu)?;
    if names
        != state
            .registers
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
    {
        return Err(codec("WHP x86 register contract differs"));
    }
    let values = state
        .registers
        .iter()
        .map(|(_, bytes)| super::register_value_from_bytes(bytes))
        .collect::<Vec<_>>();
    super::set_vcpu_registers(partition, cpu, &names, &values)?;
    // The opaque XSAVE payload retains extended state, not just x87/SSE.
    // APIC v2 state includes pending/in-service vectors and timer bookkeeping.
    super::set_virtual_processor_state(
        partition,
        id,
        WHvVirtualProcessorStateTypeXsaveState,
        &state.xsave,
    )?;
    super::set_virtual_processor_state(
        partition,
        id,
        WHvVirtualProcessorStateTypeInterruptControllerState2,
        &state.apic,
    )?;
    for (kind, payload) in [
        WHvVirtualProcessorStateTypeSynicMessagePage,
        WHvVirtualProcessorStateTypeSynicEventFlagPage,
        WHvVirtualProcessorStateTypeSynicTimerState,
    ]
    .into_iter()
    .zip(&state.synic)
    {
        super::set_virtual_processor_state(partition, id, kind, payload)?;
    }
    Ok(())
}

fn register_names(partition: WHV_PARTITION_HANDLE, id: u8) -> Result<Vec<WHV_REGISTER_NAME>> {
    // General/segment/control/debug state is mandatory. XSAVE is separate.
    let mut names = (WHvX64RegisterRax..=WHvX64RegisterXCr0).collect::<Vec<_>>();
    names.extend([
        WHvX64RegisterTsc,
        WHvX64RegisterEfer,
        WHvX64RegisterKernelGsBase,
        WHvX64RegisterApicBase,
        WHvX64RegisterPat,
        WHvX64RegisterSysenterCs,
        WHvX64RegisterSysenterEip,
        WHvX64RegisterSysenterEsp,
        WHvX64RegisterStar,
        WHvX64RegisterLstar,
        WHvX64RegisterCstar,
        WHvX64RegisterSfmask,
        WHvRegisterPendingInterruption,
        WHvRegisterInterruptState,
        WHvRegisterPendingEvent,
        WHvX64RegisterDeliverabilityNotifications,
        WHvRegisterInternalActivityState,
    ]);
    // Architectural MSRs vary with WHP's exposed feature set. Probe only known
    // register names, retain every supported one, and require the same contract
    // on restore. Unexpected backend failures never mean "optional".
    let mut optional = vec![
        WHvX64RegisterTscAux,
        WHvX64RegisterTscDeadline,
        WHvX64RegisterTscAdjust,
        WHvX64RegisterBndcfgs,
        WHvX64RegisterSpecCtrl,
        WHvX64RegisterTsxCtrl,
        WHvX64RegisterXss,
        WHvX64RegisterUCet,
        WHvX64RegisterSCet,
        WHvX64RegisterSsp,
        WHvX64RegisterPl0Ssp,
        WHvX64RegisterPl1Ssp,
        WHvX64RegisterPl2Ssp,
        WHvX64RegisterPl3Ssp,
        WHvX64RegisterInterruptSspTableAddr,
        WHvX64RegisterUmwaitControl,
        WHvX64RegisterXfd,
        WHvX64RegisterXfdErr,
        WHvX64RegisterPendingDebugException,
        WHvX64RegisterMsrMtrrDefType,
        WHvRegisterGuestOsId,
        WHvX64RegisterHypercall,
        WHvRegisterVpAssistPage,
        WHvRegisterReferenceTsc,
        WHvRegisterReferenceTscSequence,
        WHvRegisterScontrol,
        WHvRegisterSiefp,
        WHvRegisterSimp,
    ];
    optional.extend(WHvX64RegisterMsrMtrrPhysBase0..=WHvX64RegisterMsrMtrrPhysBaseF);
    optional.extend(WHvX64RegisterMsrMtrrPhysMask0..=WHvX64RegisterMsrMtrrPhysMaskF);
    optional.extend(WHvX64RegisterMsrMtrrFix64k00000..=WHvX64RegisterMsrMtrrFix4kF8000);
    optional.extend(WHvRegisterSint0..=WHvRegisterSint15);
    for name in optional {
        match super::get_vcpu_register_bytes(partition, id, &[name]) {
            Ok(_) => names.push(name),
            Err(Error::GetVirtualProcessorRegisters { hresult, .. })
                if matches!(hresult as u32, 0x8007_0057 | 0xc035_0005 | 0x8037_0302) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(names)
}

fn features(partition: WHV_PARTITION_HANDLE) -> Result<Vec<Vec<u8>>> {
    [
        WHvPartitionPropertyCodeProcessorFeaturesBanks,
        WHvPartitionPropertyCodeProcessorXsaveFeatures,
        WHvPartitionPropertyCodeLocalApicEmulationMode,
        WHvPartitionPropertyCodeInterruptClockFrequency,
    ]
    .into_iter()
    .map(|code| {
        let mut buffer = [0_u64; 32];
        let mut written = 0;
        let status = unsafe {
            WHvGetPartitionProperty(
                partition,
                code,
                buffer.as_mut_ptr().cast(),
                std::mem::size_of_val(&buffer) as u32,
                &mut written,
            )
        };
        if status < 0 || written == 0 || written as usize > std::mem::size_of_val(&buffer) {
            return Err(codec(format!(
                "partition feature {code:#x}: HRESULT {status:#x}"
            )));
        }
        let bytes = buffer
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .take(written as usize)
            .collect();
        Ok(bytes)
    })
    .collect()
}

fn frequency() -> Result<u64> {
    super::host_tsc_frequency_hz().ok_or_else(|| codec("WHP processor clock frequency unavailable"))
}

fn encode<T: Serialize>(state: &T) -> Result<Vec<u8>> {
    bincode::serde::encode_to_vec(state, bincode::config::standard()).map_err(codec)
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (state, used) = bincode::serde::decode_from_slice(
        bytes,
        bincode::config::standard().with_limit::<{ 64 * 1024 * 1024 }>(),
    )
    .map_err(codec)?;
    if used != bytes.len() {
        return Err(codec("trailing WHP x86 state bytes"));
    }
    Ok(state)
}

fn codec(error: impl std::fmt::Display) -> Error {
    Error::StateCodec(error.to_string())
}
