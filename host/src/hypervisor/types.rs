//! Shared types for the hypervisor abstraction layer.
//!
//! These types are used by both HVF and KVM backends.

/// SIMD/FP register value (128-bit). Must be 16-byte aligned.
#[repr(C, align(16))]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct SimdReg(pub [u8; 16]);

/// Decoded MMIO access from a VM exit.
pub struct MmioAccess {
    /// Guest physical address of the access
    pub addr: u64,
    /// true = write, false = read
    pub is_write: bool,
    /// Transfer size in bytes (1, 2, 4, 8)
    pub len: usize,
    /// Destination/source register number (Rt). Only used by HVF;
    /// KVM handles register writeback internally.
    pub reg: u32,
    /// Sign extend? (only for reads: LDRSB, LDRSH, LDRSW)
    pub sign_extend: bool,
    /// For writes: the value being written. For reads: unused (0).
    pub data: u64,
}

/// Unified exit reason from vcpu_run().
pub enum VcpuExit {
    /// MMIO access (data abort on HVF, KVM_EXIT_MMIO on KVM).
    /// On HVF, the backend decodes the syndrome and advances PC.
    /// On KVM, the kernel provides decoded access info and advances PC.
    Mmio(MmioAccess),

    /// HVC or SMC instruction. `syndrome` is the ESR_EL2 value.
    Hvc { syndrome: u64, is_smc: bool },

    /// WFI / WFE (EC=0x01).
    Wfi,

    /// HVF vtimer activated. KVM handles timers in-kernel, so this
    /// only occurs on the HVF backend.
    VtimerActivated,

    /// Forced exit (HVF: hv_vcpus_exit / KVM: signal-based).
    Canceled,

    /// System register trap (EC=0x18).
    SysRegTrap { syndrome: u64 },

    /// KVM system event (shutdown, reset).
    SystemEvent { event_type: u32 },

    /// Undecodable data abort (ISV=0). Backend logs warning.
    UndecodableMmio { syndrome: u64, ipa: u64 },

    /// Debug exit (BRK instruction with guest debug enabled).
    /// On KVM, triggered by patched timer BRK instructions.
    Debug,

    /// Unknown/unexpected exit.
    Unknown(u32),
}

/// System registers that can be get/set on a vCPU.
/// Each backend maps these to its native register IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum SysReg {
    MDSCR_EL1,
    MPIDR_EL1,
    SCTLR_EL1,
    CPACR_EL1,
    TTBR0_EL1,
    TTBR1_EL1,
    TCR_EL1,
    APIAKEYLO_EL1,
    APIAKEYHI_EL1,
    APIBKEYLO_EL1,
    APIBKEYHI_EL1,
    APDAKEYLO_EL1,
    APDAKEYHI_EL1,
    APDBKEYLO_EL1,
    APDBKEYHI_EL1,
    APGAKEYLO_EL1,
    APGAKEYHI_EL1,
    SPSR_EL1,
    ELR_EL1,
    SP_EL0,
    AFSR0_EL1,
    AFSR1_EL1,
    ESR_EL1,
    FAR_EL1,
    PAR_EL1,
    MAIR_EL1,
    AMAIR_EL1,
    VBAR_EL1,
    CONTEXTIDR_EL1,
    TPIDR_EL1,
    CNTKCTL_EL1,
    TPIDR_EL0,
    TPIDRRO_EL0,
    CNTV_CTL_EL0,
    CNTV_CVAL_EL0,
    CNTP_CTL_EL0,
    CNTP_CVAL_EL0,
    CNTHCTL_EL2,
    SP_EL1,
    CSSELR_EL1,
}

/// The list of system registers saved/restored for snapshots.
/// Order must be consistent across save and restore.
pub const SNAPSHOT_SYS_REGS: &[SysReg] = &[
    SysReg::SCTLR_EL1,
    SysReg::CPACR_EL1,
    SysReg::TTBR0_EL1,
    SysReg::TTBR1_EL1,
    SysReg::TCR_EL1,
    SysReg::SPSR_EL1,
    SysReg::ELR_EL1,
    SysReg::SP_EL0,
    SysReg::ESR_EL1,
    SysReg::FAR_EL1,
    SysReg::PAR_EL1,
    SysReg::MAIR_EL1,
    SysReg::AMAIR_EL1,
    SysReg::VBAR_EL1,
    SysReg::CONTEXTIDR_EL1,
    SysReg::TPIDR_EL1,
    SysReg::CNTKCTL_EL1,
    SysReg::TPIDR_EL0,
    SysReg::TPIDRRO_EL0,
    SysReg::CNTV_CTL_EL0,
    SysReg::CNTV_CVAL_EL0,
    SysReg::CNTP_CTL_EL0,
    SysReg::CNTP_CVAL_EL0,
    SysReg::SP_EL1,
    SysReg::AFSR0_EL1,
    SysReg::AFSR1_EL1,
    SysReg::CSSELR_EL1,
    SysReg::MDSCR_EL1,
    // PAC keys
    SysReg::APIAKEYLO_EL1,
    SysReg::APIAKEYHI_EL1,
    SysReg::APIBKEYLO_EL1,
    SysReg::APIBKEYHI_EL1,
    SysReg::APDAKEYLO_EL1,
    SysReg::APDAKEYHI_EL1,
    SysReg::APDBKEYLO_EL1,
    SysReg::APDBKEYHI_EL1,
    SysReg::APGAKEYLO_EL1,
    SysReg::APGAKEYHI_EL1,
];

/// GIC ICC (CPU interface) registers for save/restore.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum IccReg {
    PMR_EL1,
    BPR0_EL1,
    AP0R0_EL1,
    AP1R0_EL1,
    BPR1_EL1,
    CTLR_EL1,
    SRE_EL1,
    IGRPEN0_EL1,
    IGRPEN1_EL1,
}

pub const ICC_REGS: &[IccReg] = &[
    IccReg::PMR_EL1,
    IccReg::BPR0_EL1,
    IccReg::AP0R0_EL1,
    IccReg::AP1R0_EL1,
    IccReg::BPR1_EL1,
    IccReg::CTLR_EL1,
    IccReg::SRE_EL1,
    IccReg::IGRPEN0_EL1,
    IccReg::IGRPEN1_EL1,
];

/// General-purpose register indices (matching HVF numbering).
/// x0-x30 = 0-30, PC = 31, FPCR = 32, FPSR = 33, CPSR/PSTATE = 34.
pub const REG_X0: u32 = 0;
pub const REG_X1: u32 = 1;
pub const REG_X2: u32 = 2;
pub const REG_X3: u32 = 3;
pub const REG_PC: u32 = 31;
pub const REG_FPCR: u32 = 32;
pub const REG_FPSR: u32 = 33;
pub const REG_CPSR: u32 = 34;

/// Total number of GPRs we save (x0-x30 + PC + FPCR + FPSR + CPSR).
pub const GPR_COUNT: usize = 35;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simd_reg_layout() {
        assert_eq!(std::mem::size_of::<SimdReg>(), 16);
        assert_eq!(std::mem::align_of::<SimdReg>(), 16);
    }

    #[test]
    fn snapshot_sys_regs_count() {
        // 28 base regs + 10 PAC keys = 38
        assert_eq!(SNAPSHOT_SYS_REGS.len(), 38);
    }
}
