//! Optional GIC register APIs, resolved once instead of linked into ordinary VM startup.

use std::sync::LazyLock;

use crate::bindings::{hv_gic_icc_reg_t, hv_gic_ich_reg_t, hv_return_t, hv_vcpu_t};
use crate::{Error, HVF};

type ReadIcc = unsafe extern "C" fn(hv_vcpu_t, hv_gic_icc_reg_t, *mut u64) -> hv_return_t;
type WriteIcc = unsafe extern "C" fn(hv_vcpu_t, hv_gic_icc_reg_t, u64) -> hv_return_t;
type ReadIch = unsafe extern "C" fn(hv_vcpu_t, hv_gic_ich_reg_t, *mut u64) -> hv_return_t;
type WriteIch = unsafe extern "C" fn(hv_vcpu_t, hv_gic_ich_reg_t, u64) -> hv_return_t;

#[derive(Default)]
pub(super) struct GicRegisters {
    pub read_icc: Option<ReadIcc>,
    pub write_icc: Option<WriteIcc>,
    pub read_ich: Option<ReadIch>,
    pub write_ich: Option<WriteIch>,
}

// HVF remains loaded for the process lifetime, so copied function pointers cannot outlive it.
// Missing symbols are cached too: old hosts can still boot ordinary VMs without these APIs.
static REGISTERS: LazyLock<GicRegisters> = LazyLock::new(|| unsafe {
    GicRegisters {
        read_icc: HVF.get::<ReadIcc>(b"hv_gic_get_icc_reg\0").ok().map(|f| *f),
        write_icc: HVF
            .get::<WriteIcc>(b"hv_gic_set_icc_reg\0")
            .ok()
            .map(|f| *f),
        read_ich: HVF.get::<ReadIch>(b"hv_gic_get_ich_reg\0").ok().map(|f| *f),
        write_ich: HVF
            .get::<WriteIch>(b"hv_gic_set_ich_reg\0")
            .ok()
            .map(|f| *f),
    }
});

impl GicRegisters {
    /// Check both capture and restore support before reading or mutating any vCPU state.
    pub fn require(&self, nested: bool) -> Result<(), Error> {
        for (available, symbol) in [
            (self.read_icc.is_some(), "hv_gic_get_icc_reg"),
            (self.write_icc.is_some(), "hv_gic_set_icc_reg"),
            (!nested || self.read_ich.is_some(), "hv_gic_get_ich_reg"),
            (!nested || self.write_ich.is_some(), "hv_gic_set_ich_reg"),
        ] {
            if !available {
                return Err(Error::GicStateApiUnavailable(symbol));
            }
        }
        Ok(())
    }
}

pub(super) fn get() -> &'static GicRegisters {
    &REGISTERS
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn read(_: hv_vcpu_t, _: u16, _: *mut u64) -> hv_return_t {
        0
    }
    unsafe extern "C" fn write(_: hv_vcpu_t, _: u16, _: u64) -> hv_return_t {
        0
    }

    #[test]
    fn missing_api_matrix_rejects_before_state_access() {
        for bits in 0..16 {
            let api = GicRegisters {
                read_icc: (bits & 1 != 0).then_some(read),
                write_icc: (bits & 2 != 0).then_some(write),
                read_ich: (bits & 4 != 0).then_some(read),
                write_ich: (bits & 8 != 0).then_some(write),
            };
            assert_eq!(api.require(false).is_ok(), bits & 3 == 3);
            assert_eq!(api.require(true).is_ok(), bits == 15);
        }
    }

    #[test]
    fn unsupported_error_identifies_the_missing_os_api() {
        let error = GicRegisters::default().require(false).unwrap_err();
        assert!(error.to_string().contains("hv_gic_get_icc_reg"));
        assert!(error.to_string().contains("unavailable"));
    }
}
