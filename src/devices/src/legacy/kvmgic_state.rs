//! KVM VGIC state at the all-vCPU pause boundary.

use std::io;

use kvm_bindings::*;
use kvm_ioctls::DeviceFd;
use serde::{Deserialize, Serialize};

use crate::Error;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const MAX_STATE_BYTES: usize = 1024 * 1024;
const DIST: u32 = KVM_DEV_ARM_VGIC_GRP_DIST_REGS;
const REDIST: u32 = KVM_DEV_ARM_VGIC_GRP_REDIST_REGS;
const CPU: u32 = KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS;
const LEVEL: u32 = KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Register {
    group: u32,
    attr: u64,
}

#[derive(Serialize, Deserialize)]
struct State {
    version: u32,
    mpidrs: Vec<u64>,
    identity: Vec<u64>,
    registers: Vec<(Register, u64)>,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Initialize the VGIC and confirm its userspace register semantics.
pub(super) fn initialize(fd: &DeviceFd) -> Result<(), kvm_ioctls::Error> {
    // New kernels permit IIDR negotiation before CTRL_INIT. Older kernels
    // (including 6.12) reject that read with EBUSY until initialization. No
    // vCPU is running here, so only that specific error permits deferral.
    let deferred = match acknowledge_revision(fd) {
        Ok(()) => false,
        Err(error) if error.errno() == libc::EBUSY => true,
        Err(error) => return Err(error),
    };
    fd.set_device_attr(&kvm_device_attr {
        group: KVM_DEV_ARM_VGIC_GRP_CTRL,
        attr: KVM_DEV_ARM_VGIC_CTRL_INIT as u64,
        addr: 0,
        flags: 0,
    })?;
    if deferred {
        acknowledge_revision(fd)?;
    }
    Ok(())
}

fn acknowledge_revision(fd: &DeviceFd) -> Result<(), kvm_ioctls::Error> {
    let mut value = 0_u32;
    let mut attr = kvm_device_attr {
        group: DIST,
        attr: 8,
        addr: &mut value as *mut u32 as u64,
        flags: 0,
    };
    // The pointer is a live, aligned u32 as required by the distributor ABI.
    unsafe {
        fd.get_device_attr(&mut attr)?;
    }
    fd.set_device_attr(&attr)
}

pub(super) fn capture(fd: &DeviceFd, version: u32, mpidrs: &[u64]) -> Result<Vec<u8>, Error> {
    if version == 3 {
        // KVM owns LPI pending bits until this flush. RAM capture follows the
        // execution-state capture, so those bytes belong to the same snapshot.
        fd.set_device_attr(&kvm_device_attr {
            group: KVM_DEV_ARM_VGIC_GRP_CTRL,
            attr: KVM_DEV_ARM_VGIC_SAVE_PENDING_TABLES as u64,
            addr: 0,
            flags: 0,
        })
        .map_err(failure)?;
    }
    let (identity, layout) = layout(fd, version, mpidrs)?;
    let registers = layout
        .into_iter()
        .map(|reg| read(fd, reg).map(|value| (reg, value)))
        .collect::<Result<Vec<_>, _>>()?;
    bincode::serde::encode_to_vec(
        State {
            version,
            mpidrs: mpidrs.to_vec(),
            identity,
            registers,
        },
        bincode::config::standard(),
    )
    .map_err(failure)
}

pub(super) fn restore(
    fd: &DeviceFd,
    version: u32,
    mpidrs: &[u64],
    bytes: &[u8],
) -> Result<(), Error> {
    let (state, used): (State, usize) = bincode::serde::decode_from_slice(
        bytes,
        bincode::config::standard().with_limit::<MAX_STATE_BYTES>(),
    )
    .map_err(failure)?;
    let (identity, layout) = layout(fd, version, mpidrs)?;
    if used != bytes.len()
        || state.version != version
        || state.mpidrs != mpidrs
        || state.identity != identity
        || state.registers.len() != layout.len()
        || state
            .registers
            .iter()
            .zip(&layout)
            .any(|((reg, value), expected)| {
                reg != expected || (reg.group != CPU && *value > u32::MAX as u64)
            })
    {
        return Err(failure(
            "VGIC state does not match destination topology/capabilities",
        ));
    }
    restore_registers(&state.registers, |reg, value| write(fd, reg, value))
}

fn restore_registers(
    registers: &[(Register, u64)],
    mut write_register: impl FnMut(Register, u64) -> Result<(), Error>,
) -> Result<(), Error> {
    // Fresh VGICs still have enabled SGIs. Clear every enable bank before
    // restoring config, pending latches and active state.
    for (reg, _) in registers {
        if is_enable(*reg) {
            write_register(
                Register {
                    attr: reg.attr + 0x80,
                    ..*reg
                },
                u32::MAX as u64,
            )?;
        }
    }
    // KVM's userspace GICD_CTLR write only changes the distributor flag; unlike
    // a guest MMIO write, it does not queue already-pending interrupts. Restore
    // it before pending/level state and interrupt enables so a high timer line
    // reaches the vCPU's pending list. All vCPUs remain behind the pause barrier.
    for (reg, value) in registers {
        if reg.group == DIST && reg.attr == 0 {
            write_register(*reg, *value)?;
        }
    }
    for (reg, value) in registers {
        if !is_activation(*reg) {
            write_register(*reg, *value)?;
        }
    }
    for (reg, value) in registers {
        if is_activation(*reg) && !(reg.group == DIST && reg.attr == 0) {
            write_register(*reg, *value)?;
        }
    }
    Ok(())
}

fn layout(fd: &DeviceFd, version: u32, mpidrs: &[u64]) -> Result<(Vec<u64>, Vec<Register>), Error> {
    // VGICv2 lacks the line-level state interface: inventing pending latches
    // from its combined pending bitmap would not preserve interrupt semantics.
    if mpidrs.is_empty() || mpidrs.len() > 255 || version != 3 {
        return Err(failure("invalid VGIC topology"));
    }
    let nr_irqs = read(
        fd,
        Register {
            group: KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
            attr: 0,
        },
    )?;
    if !(64..=1024).contains(&nr_irqs) || nr_irqs % 32 != 0 {
        return Err(failure("invalid VGIC IRQ count"));
    }
    let mut identity = vec![
        nr_irqs,
        read(
            fd,
            Register {
                group: DIST,
                attr: 8,
            },
        )?,
        read(
            fd,
            Register {
                group: DIST,
                attr: 4,
            },
        )?,
    ];
    let mut regs = Vec::new();
    for mpidr in mpidrs {
        let affinity = packed_mpidr(*mpidr);
        let (group, base) = (REDIST, 0x10000);
        // Banked private interrupts (SGIs/PPIs), separate from global SPIs.
        for (offset, count) in [
            (0x80, 1),
            (0x100, 1),
            (0x200, 1),
            (0x300, 1),
            (0x400, 8),
            (0xc00, 2),
        ] {
            for word in 0..count {
                regs.push(Register {
                    group,
                    attr: affinity | (base + offset + 4 * word),
                });
            }
        }
        regs.push(Register {
            group: LEVEL,
            attr: affinity,
        });
        if version == 3 {
            for offset in [0x10, 0x14, 0x70, 0x74, 0x78, 0x7c] {
                regs.push(Register {
                    group: REDIST,
                    attr: affinity | offset,
                });
            }
            let ctlr = read(
                fd,
                Register {
                    group: CPU,
                    attr: affinity | 0xc664,
                },
            )?;
            // PRIbits is read-only; the remaining CTLR fields are guest state.
            let priority_bits = ((ctlr >> 8) & 7) + 1;
            if !(5..=7).contains(&priority_bits) {
                return Err(failure("unsupported VGIC priority width"));
            }
            identity.push(ctlr & 0xff00);
            for offset in [0xc230, 0xc643, 0xc663, 0xc664, 0xc665] {
                regs.push(Register {
                    group: CPU,
                    attr: affinity | offset,
                });
            }
            for word in 0..(1 << (priority_bits - 5)) {
                regs.push(Register {
                    group: CPU,
                    attr: affinity | (0xc644 + word),
                });
                regs.push(Register {
                    group: CPU,
                    attr: affinity | (0xc648 + word),
                });
            }
            // CPU group enables and redistributor LPI enable follow all state.
            for offset in [0xc666, 0xc667] {
                regs.push(Register {
                    group: CPU,
                    attr: affinity | offset,
                });
            }
            regs.push(Register {
                group: REDIST,
                attr: affinity,
            });
        }
    }
    for (offset, bits) in [
        (0x80, 1),
        (0x100, 1),
        (0x200, 1),
        (0x300, 1),
        (0x400, 8),
        (0xc00, 2),
    ] {
        for word in (32 * bits / 32)..(nr_irqs * bits / 32) {
            regs.push(Register {
                group: DIST,
                attr: offset + 4 * word,
            });
        }
    }
    if version == 3 {
        for word in 64..(nr_irqs * 2) {
            regs.push(Register {
                group: DIST,
                attr: 0x6000 + 4 * word,
            });
        }
    }
    for irq in (32..nr_irqs).step_by(32) {
        regs.push(Register {
            group: LEVEL,
            attr: irq,
        });
    }
    regs.push(Register {
        group: DIST,
        attr: 0,
    });
    Ok((identity, regs))
}

fn packed_mpidr(mpidr: u64) -> u64 {
    // Drop MPIDR's non-affinity bits; Aff3 lives at bits 39:32, not 31:24.
    (((mpidr >> 8) & 0xff00_0000) | (mpidr & 0x00ff_ffff)) << 32
}

fn is_enable(reg: Register) -> bool {
    let offset = reg.attr as u32;
    (reg.group == DIST && (0x100..0x180).contains(&offset))
        || (reg.group == REDIST && offset == 0x10100)
}

fn is_activation(reg: Register) -> bool {
    let offset = reg.attr as u32;
    is_enable(reg)
        || (matches!(reg.group, DIST | REDIST | KVM_DEV_ARM_VGIC_GRP_CPU_REGS) && offset == 0)
        || (reg.group == CPU && matches!(offset, 0xc666 | 0xc667))
}

fn read(fd: &DeviceFd, reg: Register) -> Result<u64, Error> {
    let mut value = 0_u64;
    let mut attr = kvm_device_attr {
        group: reg.group,
        attr: reg.attr,
        addr: &mut value as *mut u64 as u64,
        flags: 0,
    };
    // CPU sysregs use u64; all other groups use the low u32 of this zeroed,
    // aligned buffer. The descriptor never outlives the stack allocation.
    unsafe { fd.get_device_attr(&mut attr) }
        .map_err(|error| failure(format!("read {reg:?}: {error}")))?;
    Ok(value)
}

fn write(fd: &DeviceFd, reg: Register, value: u64) -> Result<(), Error> {
    let attr = kvm_device_attr {
        group: reg.group,
        attr: reg.attr,
        addr: &value as *const u64 as u64,
        flags: 0,
    };
    fd.set_device_attr(&attr)
        .map_err(|error| failure(format!("write {reg:?}: {error}")))
}

fn failure(error: impl std::fmt::Display) -> Error {
    Error::IoError(io::Error::other(error.to_string()))
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpidr_affinity_excludes_architectural_flags() {
        assert_eq!(packed_mpidr(0x12_c034_5678), 0x1234_5678_0000_0000);
    }

    #[test]
    fn banked_enable_uses_offset_not_affinity() {
        assert!(is_enable(Register {
            group: REDIST,
            attr: 0x100_0001_0100
        }));
        assert!(!is_enable(Register {
            group: REDIST,
            attr: 0x100_0001_0200
        }));
    }

    #[test]
    fn distributor_is_restored_before_pending_interrupts_are_enabled() {
        let distributor = Register {
            group: DIST,
            attr: 0,
        };
        let enable = Register {
            group: REDIST,
            attr: 0x10100,
        };
        let level = Register {
            group: LEVEL,
            attr: 0,
        };
        // Match capture layout: distributor control is serialized last.
        let registers = [(enable, 1 << 27), (level, 1 << 27), (distributor, 0x52)];
        let mut writes = Vec::new();
        restore_registers(&registers, |reg, value| {
            writes.push((reg, value));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            writes,
            vec![
                (
                    Register {
                        attr: 0x10180,
                        ..enable
                    },
                    u32::MAX as u64
                ),
                (distributor, 0x52),
                (level, 1 << 27),
                (enable, 1 << 27),
            ]
        );
    }
}
