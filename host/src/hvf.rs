//! Hand-written FFI bindings for Hypervisor.framework on aarch64 macOS.
#![allow(dead_code)]

pub type HvReturn = i32;
pub type HvVcpu = u64;
pub type HvIpa = u64;
pub type HvMemoryFlags = u64;
pub type HvSimdFpUchar16 = [u8; 16];

pub const HV_SUCCESS: HvReturn = 0;

pub const HV_MEMORY_READ: HvMemoryFlags = 1 << 0;
pub const HV_MEMORY_WRITE: HvMemoryFlags = 1 << 1;
pub const HV_MEMORY_EXEC: HvMemoryFlags = 1 << 2;

// Exit reasons
pub const HV_EXIT_REASON_CANCELED: u32 = 0;
pub const HV_EXIT_REASON_EXCEPTION: u32 = 1;
pub const HV_EXIT_REASON_VTIMER_ACTIVATED: u32 = 2;
pub const HV_EXIT_REASON_UNKNOWN: u32 = 3;

// General-purpose register IDs (sequential enum from hv_vcpu_types.h)
pub const HV_REG_X0: u32 = 0;
pub const HV_REG_X1: u32 = 1;
pub const HV_REG_X2: u32 = 2;
pub const HV_REG_X3: u32 = 3;
pub const HV_REG_X4: u32 = 4;
pub const HV_REG_X5: u32 = 5;
pub const HV_REG_X6: u32 = 6;
pub const HV_REG_X7: u32 = 7;
pub const HV_REG_X8: u32 = 8;
pub const HV_REG_X9: u32 = 9;
pub const HV_REG_X10: u32 = 10;
pub const HV_REG_X11: u32 = 11;
pub const HV_REG_X12: u32 = 12;
pub const HV_REG_X13: u32 = 13;
pub const HV_REG_X14: u32 = 14;
pub const HV_REG_X15: u32 = 15;
pub const HV_REG_X16: u32 = 16;
pub const HV_REG_X17: u32 = 17;
pub const HV_REG_X18: u32 = 18;
pub const HV_REG_X19: u32 = 19;
pub const HV_REG_X20: u32 = 20;
pub const HV_REG_X21: u32 = 21;
pub const HV_REG_X22: u32 = 22;
pub const HV_REG_X23: u32 = 23;
pub const HV_REG_X24: u32 = 24;
pub const HV_REG_X25: u32 = 25;
pub const HV_REG_X26: u32 = 26;
pub const HV_REG_X27: u32 = 27;
pub const HV_REG_X28: u32 = 28;
pub const HV_REG_X29: u32 = 29; // FP
pub const HV_REG_X30: u32 = 30; // LR
pub const HV_REG_PC: u32 = 31;
pub const HV_REG_FPCR: u32 = 32;
pub const HV_REG_FPSR: u32 = 33;
pub const HV_REG_CPSR: u32 = 34;

// SIMD/FP register IDs (sequential enum)
pub const HV_SIMD_FP_REG_Q0: u32 = 0;

// System register IDs (from hv_vcpu_types.h, sorted by hex value)
pub const HV_SYS_REG_MDSCR_EL1: u16 = 0x8012;
pub const HV_SYS_REG_MPIDR_EL1: u16 = 0xc005;
pub const HV_SYS_REG_SCTLR_EL1: u16 = 0xc080;
pub const HV_SYS_REG_CPACR_EL1: u16 = 0xc082;
pub const HV_SYS_REG_TTBR0_EL1: u16 = 0xc100;
pub const HV_SYS_REG_TTBR1_EL1: u16 = 0xc101;
pub const HV_SYS_REG_TCR_EL1: u16 = 0xc102;
pub const HV_SYS_REG_APIAKEYLO_EL1: u16 = 0xc108;
pub const HV_SYS_REG_APIAKEYHI_EL1: u16 = 0xc109;
pub const HV_SYS_REG_APIBKEYLO_EL1: u16 = 0xc10a;
pub const HV_SYS_REG_APIBKEYHI_EL1: u16 = 0xc10b;
pub const HV_SYS_REG_APDAKEYLO_EL1: u16 = 0xc110;
pub const HV_SYS_REG_APDAKEYHI_EL1: u16 = 0xc111;
pub const HV_SYS_REG_APDBKEYLO_EL1: u16 = 0xc112;
pub const HV_SYS_REG_APDBKEYHI_EL1: u16 = 0xc113;
pub const HV_SYS_REG_APGAKEYLO_EL1: u16 = 0xc118;
pub const HV_SYS_REG_APGAKEYHI_EL1: u16 = 0xc119;
pub const HV_SYS_REG_SPSR_EL1: u16 = 0xc200;
pub const HV_SYS_REG_ELR_EL1: u16 = 0xc201;
pub const HV_SYS_REG_SP_EL0: u16 = 0xc208;
pub const HV_SYS_REG_AFSR0_EL1: u16 = 0xc288;
pub const HV_SYS_REG_AFSR1_EL1: u16 = 0xc289;
pub const HV_SYS_REG_ESR_EL1: u16 = 0xc290;
pub const HV_SYS_REG_FAR_EL1: u16 = 0xc300;
pub const HV_SYS_REG_PAR_EL1: u16 = 0xc3a0;
pub const HV_SYS_REG_MAIR_EL1: u16 = 0xc510;
pub const HV_SYS_REG_AMAIR_EL1: u16 = 0xc518;
pub const HV_SYS_REG_VBAR_EL1: u16 = 0xc600;
pub const HV_SYS_REG_CONTEXTIDR_EL1: u16 = 0xc681;
pub const HV_SYS_REG_TPIDR_EL1: u16 = 0xc684;
pub const HV_SYS_REG_CNTKCTL_EL1: u16 = 0xc708;
pub const HV_SYS_REG_CSSELR_EL1: u16 = 0xd000;
pub const HV_SYS_REG_TPIDR_EL0: u16 = 0xde82;
pub const HV_SYS_REG_TPIDRRO_EL0: u16 = 0xde83;
pub const HV_SYS_REG_CNTV_CTL_EL0: u16 = 0xdf19;
pub const HV_SYS_REG_CNTV_CVAL_EL0: u16 = 0xdf1a;
pub const HV_SYS_REG_SP_EL1: u16 = 0xe208;

// Interrupt types
pub const HV_INTERRUPT_TYPE_IRQ: u32 = 0;

/// System registers we snapshot/restore for a vCPU.
pub const SNAPSHOT_SYS_REGS: &[u16] = &[
    HV_SYS_REG_SCTLR_EL1,
    HV_SYS_REG_CPACR_EL1,
    HV_SYS_REG_TTBR0_EL1,
    HV_SYS_REG_TTBR1_EL1,
    HV_SYS_REG_TCR_EL1,
    HV_SYS_REG_SPSR_EL1,
    HV_SYS_REG_ELR_EL1,
    HV_SYS_REG_SP_EL0,
    HV_SYS_REG_ESR_EL1,
    HV_SYS_REG_FAR_EL1,
    HV_SYS_REG_PAR_EL1,
    HV_SYS_REG_MAIR_EL1,
    HV_SYS_REG_AMAIR_EL1,
    HV_SYS_REG_VBAR_EL1,
    HV_SYS_REG_CONTEXTIDR_EL1,
    HV_SYS_REG_TPIDR_EL1,
    HV_SYS_REG_CNTKCTL_EL1,
    HV_SYS_REG_TPIDR_EL0,
    HV_SYS_REG_TPIDRRO_EL0,
    HV_SYS_REG_CNTV_CTL_EL0,
    HV_SYS_REG_CNTV_CVAL_EL0,
    HV_SYS_REG_SP_EL1,
    HV_SYS_REG_AFSR0_EL1,
    HV_SYS_REG_AFSR1_EL1,
    HV_SYS_REG_CSSELR_EL1,
    HV_SYS_REG_MDSCR_EL1,
    // PAC keys — essential for pointer authentication
    HV_SYS_REG_APIAKEYLO_EL1,
    HV_SYS_REG_APIAKEYHI_EL1,
    HV_SYS_REG_APIBKEYLO_EL1,
    HV_SYS_REG_APIBKEYHI_EL1,
    HV_SYS_REG_APDAKEYLO_EL1,
    HV_SYS_REG_APDAKEYHI_EL1,
    HV_SYS_REG_APDBKEYLO_EL1,
    HV_SYS_REG_APDBKEYHI_EL1,
    HV_SYS_REG_APGAKEYLO_EL1,
    HV_SYS_REG_APGAKEYHI_EL1,
];

#[repr(C)]
pub struct HvVcpuExitException {
    pub syndrome: u64,
    pub virtual_address: u64,
    pub physical_address: u64,
}

#[repr(C)]
pub struct HvVcpuExit {
    pub reason: u32,
    pub exception: HvVcpuExitException,
}

type HvVmConfig = *const std::ffi::c_void;
type HvVcpuConfig = *const std::ffi::c_void;
pub type HvGicConfig = *const std::ffi::c_void;

#[link(name = "Hypervisor", kind = "framework")]
extern "C" {
    pub fn hv_vm_create(config: HvVmConfig) -> HvReturn;
    pub fn hv_vm_destroy() -> HvReturn;
    pub fn hv_vm_map(addr: *mut u8, ipa: HvIpa, size: usize, flags: HvMemoryFlags) -> HvReturn;
    pub fn hv_vm_unmap(ipa: HvIpa, size: usize) -> HvReturn;
    pub fn hv_vcpu_create(
        vcpu: *mut HvVcpu,
        exit: *mut *const HvVcpuExit,
        config: HvVcpuConfig,
    ) -> HvReturn;
    pub fn hv_vcpu_destroy(vcpu: HvVcpu) -> HvReturn;
    pub fn hv_vcpu_run(vcpu: HvVcpu) -> HvReturn;
    pub fn hv_vcpu_get_reg(vcpu: HvVcpu, reg: u32, value: *mut u64) -> HvReturn;
    pub fn hv_vcpu_set_reg(vcpu: HvVcpu, reg: u32, value: u64) -> HvReturn;
    pub fn hv_vcpu_get_sys_reg(vcpu: HvVcpu, reg: u16, value: *mut u64) -> HvReturn;
    pub fn hv_vcpu_set_sys_reg(vcpu: HvVcpu, reg: u16, value: u64) -> HvReturn;
    pub fn hv_vcpu_get_simd_fp_reg(
        vcpu: HvVcpu,
        reg: u32,
        value: *mut HvSimdFpUchar16,
    ) -> HvReturn;
    pub fn hv_vcpu_set_simd_fp_reg(
        vcpu: HvVcpu,
        reg: u32,
        value: *const HvSimdFpUchar16,
    ) -> HvReturn;

    // vtimer mask
    pub fn hv_vcpu_set_vtimer_mask(vcpu: HvVcpu, vtimer_is_masked: bool) -> HvReturn;

    // Pending interrupt injection
    pub fn hv_vcpu_set_pending_interrupt(
        vcpu: HvVcpu,
        r#type: u32,
        pending: bool,
    ) -> HvReturn;

    // GIC configuration
    pub fn hv_gic_config_create() -> HvGicConfig;
    pub fn hv_gic_config_set_distributor_base(
        config: HvGicConfig,
        distributor_base_address: HvIpa,
    ) -> HvReturn;
    pub fn hv_gic_config_set_redistributor_base(
        config: HvGicConfig,
        redistributor_base_address: HvIpa,
    ) -> HvReturn;

    // GIC lifecycle
    pub fn hv_gic_create(gic_config: HvGicConfig) -> HvReturn;
    pub fn hv_gic_reset() -> HvReturn;
    pub fn hv_gic_set_spi(intid: u32, level: bool) -> HvReturn;

    // GIC parameter queries
    pub fn hv_gic_get_distributor_size(size: *mut usize) -> HvReturn;
    pub fn hv_gic_get_distributor_base_alignment(alignment: *mut usize) -> HvReturn;
    pub fn hv_gic_get_redistributor_region_size(size: *mut usize) -> HvReturn;
    pub fn hv_gic_get_redistributor_base_alignment(alignment: *mut usize) -> HvReturn;
    pub fn hv_gic_get_spi_interrupt_range(
        spi_intid_base: *mut u32,
        spi_intid_count: *mut u32,
    ) -> HvReturn;
    pub fn hv_gic_get_redistributor_base(
        vcpu: HvVcpu,
        redistributor_base_address: *mut HvIpa,
    ) -> HvReturn;
    pub fn hv_gic_get_intid(interrupt: u16, intid: *mut u32) -> HvReturn;

    // Force vCPU exit
    pub fn hv_vcpus_exit(vcpus: *const HvVcpu, vcpu_count: u32) -> HvReturn;

    // GIC state save/restore
    pub fn hv_gic_state_create() -> *mut std::ffi::c_void; // returns hv_gic_state_t
    pub fn hv_gic_state_get_size(
        state: *const std::ffi::c_void,
        gic_state_size: *mut usize,
    ) -> HvReturn;
    pub fn hv_gic_state_get_data(
        state: *const std::ffi::c_void,
        gic_state_data: *mut u8,
    ) -> HvReturn;
    pub fn hv_gic_set_state(
        gic_state_data: *const u8,
        gic_state_size: usize,
    ) -> HvReturn;

    // GIC ICC (CPU interface) registers for save/restore
    pub fn hv_gic_get_icc_reg(
        vcpu: HvVcpu,
        reg: u16,
        value: *mut u64,
    ) -> HvReturn;
    pub fn hv_gic_set_icc_reg(
        vcpu: HvVcpu,
        reg: u16,
        value: u64,
    ) -> HvReturn;
}

pub fn check_hv(ret: HvReturn, context: &str) {
    if ret != HV_SUCCESS {
        panic!("{}: HVF returned 0x{:x}", context, ret as u32);
    }
}

/// Convenience: get a register value, panicking on error.
pub unsafe fn vcpu_get_reg(vcpu: HvVcpu, reg: u32) -> u64 {
    let mut value: u64 = 0;
    check_hv(hv_vcpu_get_reg(vcpu, reg, &mut value), "hv_vcpu_get_reg");
    value
}

/// Convenience: get a system register value, panicking on error.
pub unsafe fn vcpu_get_sys_reg(vcpu: HvVcpu, reg: u16) -> u64 {
    let mut value: u64 = 0;
    check_hv(
        hv_vcpu_get_sys_reg(vcpu, reg, &mut value),
        "hv_vcpu_get_sys_reg",
    );
    value
}
