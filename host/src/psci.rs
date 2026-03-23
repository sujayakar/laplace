//! PSCI (Power State Coordination Interface) handler.
//!
//! Linux calls PSCI via HVC to discover CPUs and manage power states.
//! We implement the minimum subset needed for a single-vCPU boot.

// ARM SMCCC (SMC Calling Convention)
const SMCCC_VERSION: u32 = 0x8000_0000;
const SMCCC_ARCH_FEATURES: u32 = 0x8000_0001;
const SMCCC_ARCH_SOC_ID: u32 = 0x8000_0002;
const SMCCC_ARCH_WORKAROUND_1: u32 = 0x8000_8000;
const SMCCC_ARCH_WORKAROUND_2: u32 = 0x8000_7FFF;

// PSCI extended functions
const PSCI_CPU_FREEZE: u32 = 0x8400_000B;
const PSCI_CPU_DEFAULT_SUSPEND_32: u32 = 0x8400_000C;
const PSCI_NODE_HW_STATE_32: u32 = 0x8400_000D;
const PSCI_SYSTEM_SUSPEND_32: u32 = 0x8400_000E;
const PSCI_SET_SUSPEND_MODE: u32 = 0x8400_000F;
const PSCI_STAT_RESIDENCY_32: u32 = 0x8400_0010;
const PSCI_STAT_COUNT_32: u32 = 0x8400_0011;
const PSCI_SYSTEM_RESET2_32: u32 = 0x8400_0012;
const PSCI_MEM_PROTECT: u32 = 0x8400_0013;
const PSCI_MEM_PROTECT_CHECK_RANGE_32: u32 = 0x8400_0014;

// PSCI 64-bit variants
const PSCI_CPU_DEFAULT_SUSPEND_64: u32 = 0xC400_000C;
const PSCI_NODE_HW_STATE_64: u32 = 0xC400_000D;
const PSCI_SYSTEM_SUSPEND_64: u32 = 0xC400_000E;
const PSCI_STAT_RESIDENCY_64: u32 = 0xC400_0010;
const PSCI_STAT_COUNT_64: u32 = 0xC400_0011;
const PSCI_SYSTEM_RESET2_64: u32 = 0xC400_0012;
const PSCI_MEM_PROTECT_CHECK_RANGE_64: u32 = 0xC400_0014;

// PSCI SMCCC_FILTER
const PSCI_SMCCC_FILTER: u32 = 0x8400_0050;

// Arm FF-A (Firmware Framework)
const FFA_VERSION: u32 = 0x8600_FF01;

// PSCI function IDs (PSCI v1.1, SMC Calling Convention)
const PSCI_VERSION: u32 = 0x8400_0000;
const PSCI_CPU_SUSPEND_32: u32 = 0x8400_0001;
const PSCI_CPU_OFF: u32 = 0x8400_0002;
const PSCI_CPU_ON_32: u32 = 0x8400_0003;
const PSCI_CPU_ON_64: u32 = 0xC400_0003;
const PSCI_AFFINITY_INFO_64: u32 = 0xC400_0004;
const PSCI_MIGRATE_INFO_TYPE: u32 = 0x8400_0006;
const PSCI_SYSTEM_OFF: u32 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u32 = 0x8400_0009;
const PSCI_FEATURES: u32 = 0x8400_000A;

// PSCI return codes
const PSCI_SUCCESS: i64 = 0;
const PSCI_NOT_SUPPORTED: i64 = -1;
const PSCI_ALREADY_ON: i64 = -6;

// PSCI v1.1 version encoding: major=1, minor=1
const PSCI_VERSION_1_1: u64 = (1 << 16) | 1;

/// Result of handling a PSCI call.
pub enum PsciResult {
    /// Return a value in x0 and continue execution.
    Return(u64),
    /// System off — terminate the VM.
    SystemOff,
    /// System reset — terminate the VM (we don't support actual reset).
    SystemReset,
}

/// Try to handle an HVC as a PSCI/SMCCC call.
/// `func_id` is x0, `arg1` is x1 (used by PSCI_FEATURES to identify the queried function).
/// Returns None if the function ID is not a PSCI/SMCCC function.
pub fn handle_psci(func_id: u32, arg1: u64) -> Option<PsciResult> {
    match func_id {
        // SMCCC calls
        SMCCC_VERSION => {
            // SMCCC v1.2
            Some(PsciResult::Return((1 << 16) | 2))
        }
        SMCCC_ARCH_FEATURES => {
            // Return "supported" for known features, "not supported" otherwise
            Some(PsciResult::Return(PSCI_SUCCESS as u64))
        }
        SMCCC_ARCH_SOC_ID => {
            // SOC ID not available
            Some(PsciResult::Return(PSCI_NOT_SUPPORTED as u64))
        }
        SMCCC_ARCH_WORKAROUND_1 | SMCCC_ARCH_WORKAROUND_2 => {
            // Not needed in a VM — return "not required"
            Some(PsciResult::Return(PSCI_NOT_SUPPORTED as u64))
        }

        // PSCI calls
        PSCI_VERSION => Some(PsciResult::Return(PSCI_VERSION_1_1)),

        PSCI_CPU_ON_32 | PSCI_CPU_ON_64 => {
            // We only have one CPU and it's already on
            Some(PsciResult::Return(PSCI_ALREADY_ON as u64))
        }

        PSCI_CPU_OFF => {
            // Single CPU can't turn itself off
            Some(PsciResult::Return(PSCI_NOT_SUPPORTED as u64))
        }

        PSCI_CPU_SUSPEND_32 => {
            // Treat suspend as a no-op (return success, resume immediately)
            Some(PsciResult::Return(PSCI_SUCCESS as u64))
        }

        PSCI_AFFINITY_INFO_64 => {
            // CPU 0 is always on (return 0 = ON)
            Some(PsciResult::Return(0))
        }

        PSCI_MIGRATE_INFO_TYPE => {
            // Not a uniprocessor, migration not supported
            Some(PsciResult::Return(2)) // 2 = migration not supported
        }

        PSCI_FEATURES => {
            // Check if the queried function (in arg1/x1) is one we support
            let queried = arg1 as u32;
            let supported = matches!(
                queried,
                PSCI_VERSION | PSCI_CPU_ON_32 | PSCI_CPU_ON_64 | PSCI_CPU_OFF
                    | PSCI_CPU_SUSPEND_32 | PSCI_AFFINITY_INFO_64
                    | PSCI_MIGRATE_INFO_TYPE | PSCI_SYSTEM_OFF | PSCI_SYSTEM_RESET
                    | PSCI_FEATURES | SMCCC_VERSION
            );
            if supported {
                Some(PsciResult::Return(PSCI_SUCCESS as u64))
            } else {
                Some(PsciResult::Return(PSCI_NOT_SUPPORTED as u64))
            }
        }

        PSCI_SYSTEM_OFF => Some(PsciResult::SystemOff),

        PSCI_SYSTEM_RESET => Some(PsciResult::SystemReset),

        // PSCI_FEATURES query for functions we don't implement
        PSCI_SMCCC_FILTER | PSCI_SYSTEM_RESET2_32 | PSCI_SYSTEM_RESET2_64 |
        PSCI_MEM_PROTECT | PSCI_MEM_PROTECT_CHECK_RANGE_32 |
        PSCI_MEM_PROTECT_CHECK_RANGE_64 | PSCI_STAT_RESIDENCY_32 |
        PSCI_STAT_RESIDENCY_64 | PSCI_STAT_COUNT_32 | PSCI_STAT_COUNT_64 |
        PSCI_SET_SUSPEND_MODE | PSCI_SYSTEM_SUSPEND_32 | PSCI_SYSTEM_SUSPEND_64 |
        PSCI_NODE_HW_STATE_32 | PSCI_NODE_HW_STATE_64 |
        PSCI_CPU_DEFAULT_SUSPEND_32 | PSCI_CPU_DEFAULT_SUSPEND_64 |
        PSCI_CPU_FREEZE => {
            Some(PsciResult::Return(PSCI_NOT_SUPPORTED as u64))
        }

        // FF-A (Firmware Framework for Arm)
        FFA_VERSION => {
            // Not supported
            Some(PsciResult::Return(PSCI_NOT_SUPPORTED as u64))
        }

        // Not a PSCI/SMCCC function ID
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn returns(func_id: u32) -> Option<u64> {
        match handle_psci(func_id, 0) {
            Some(PsciResult::Return(v)) => Some(v),
            _ => None,
        }
    }

    #[test]
    fn psci_version() {
        assert_eq!(returns(PSCI_VERSION), Some(PSCI_VERSION_1_1));
    }

    #[test]
    fn smccc_version() {
        assert_eq!(returns(SMCCC_VERSION), Some((1 << 16) | 2));
    }

    #[test]
    fn system_off() {
        assert!(matches!(handle_psci(PSCI_SYSTEM_OFF, 0), Some(PsciResult::SystemOff)));
    }

    #[test]
    fn system_reset() {
        assert!(matches!(handle_psci(PSCI_SYSTEM_RESET, 0), Some(PsciResult::SystemReset)));
    }

    #[test]
    fn cpu_on_returns_already_on() {
        assert_eq!(returns(PSCI_CPU_ON_64), Some(PSCI_ALREADY_ON as u64));
    }

    #[test]
    fn features_returns_supported_for_known_functions() {
        // PSCI_VERSION is supported
        match handle_psci(PSCI_FEATURES, PSCI_VERSION as u64) {
            Some(PsciResult::Return(v)) => assert_eq!(v, PSCI_SUCCESS as u64),
            other => panic!("Expected Return(SUCCESS), got {:?}", other.is_some()),
        }
    }

    #[test]
    fn features_returns_not_supported_for_unknown_functions() {
        // Some random function ID
        match handle_psci(PSCI_FEATURES, 0x12345678) {
            Some(PsciResult::Return(v)) => assert_eq!(v, PSCI_NOT_SUPPORTED as u64),
            other => panic!("Expected Return(NOT_SUPPORTED), got {:?}", other.is_some()),
        }
    }

    #[test]
    fn unknown_function_returns_none() {
        assert!(handle_psci(0x12345678, 0).is_none());
    }

    #[test]
    fn ffa_version_not_supported() {
        assert_eq!(returns(FFA_VERSION), Some(PSCI_NOT_SUPPORTED as u64));
    }
}
