// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter, Result};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

// The matching x86 guest kernel uses this opt-in to select libkrun's existing i8042 exit
// transport for poweroff. Hypervisor CPUID alone would also match unrelated virtual machines.
#[cfg(all(
    target_arch = "x86_64",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
pub const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 \
                                          rootfstype=virtiofs rw quiet no-kvmapf krun.poweroff=i8042";
#[cfg(all(
    not(target_arch = "x86_64"),
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
pub const DEFAULT_KERNEL_CMDLINE: &str = "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 \
                                          rootfstype=virtiofs rw quiet no-kvmapf";

/// Strongly typed data structure used to configure the boot source of the
/// microvm.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct KernelCmdlineConfig {
    pub prolog: Option<String>,
    pub krun_env: Option<String>,
    pub epilog: Option<String>,
}

/// Errors associated with actions on `KernelCmdlineConfig`.
#[derive(Debug)]
pub enum KernelCmdlineConfigError {
    /// The kernel command line is invalid.
    InvalidKernelCommandLine(String),
}

impl Display for KernelCmdlineConfigError {
    fn fmt(&self, f: &mut Formatter) -> Result {
        use self::KernelCmdlineConfigError::*;
        match *self {
            InvalidKernelCommandLine(ref e) => {
                write!(f, "The kernel command line is invalid: {}", e.as_str())
            }
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::DEFAULT_KERNEL_CMDLINE;

    #[test]
    fn default_poweroff_opt_in_is_x86_64_only() {
        let poweroff_options: Vec<_> = DEFAULT_KERNEL_CMDLINE
            .split_whitespace()
            .filter(|option| option.starts_with("krun.poweroff="))
            .collect();

        #[cfg(target_arch = "x86_64")]
        assert_eq!(poweroff_options, ["krun.poweroff=i8042"]);
        #[cfg(not(target_arch = "x86_64"))]
        assert!(poweroff_options.is_empty());
    }

    #[test]
    fn default_poweroff_opt_in_preserves_existing_boot_options() {
        let boot_options = DEFAULT_KERNEL_CMDLINE
            .split_whitespace()
            .filter(|option| !option.starts_with("krun.poweroff="))
            .collect::<Vec<_>>()
            .join(" ");

        assert_eq!(
            boot_options,
            "reboot=k panic=-1 panic_print=0 nomodule console=hvc0 \
             rootfstype=virtiofs rw quiet no-kvmapf"
        );
    }
}
