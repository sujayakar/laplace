//! HVF backend for the hypervisor abstraction layer (macOS aarch64).
//!
//! Wraps Apple's Hypervisor.framework with the cross-platform API.
//! Also re-exports raw FFI types for Phase 1 bare-metal code.
#![allow(dead_code)]

use std::ptr;

use super::types::*;

// ─── Raw FFI bindings (from Hypervisor.framework) ────────────────────────────

pub type HvReturn = i32;
pub type HvVcpu = u64;
pub type HvIpa = u64;
pub type HvMemoryFlags = u64;

#[repr(C, align(16))]
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct HvSimdFpUchar16(pub [u8; 16]);

pub const HV_SUCCESS: HvReturn = 0;

pub const HV_MEMORY_READ: HvMemoryFlags = 1 << 0;
pub const HV_MEMORY_WRITE: HvMemoryFlags = 1 << 1;
pub const HV_MEMORY_EXEC: HvMemoryFlags = 1 << 2;

pub const HV_EXIT_REASON_CANCELED: u32 = 0;
pub const HV_EXIT_REASON_EXCEPTION: u32 = 1;
pub const HV_EXIT_REASON_VTIMER_ACTIVATED: u32 = 2;
pub const HV_EXIT_REASON_UNKNOWN: u32 = 3;

pub const HV_REG_X0: u32 = 0;
pub const HV_REG_X1: u32 = 1;
pub const HV_REG_X2: u32 = 2;
pub const HV_REG_X3: u32 = 3;
pub const HV_REG_PC: u32 = 31;
pub const HV_REG_FPCR: u32 = 32;
pub const HV_REG_FPSR: u32 = 33;
pub const HV_REG_CPSR: u32 = 34;

pub const HV_SIMD_FP_REG_Q0: u32 = 0;

pub const HV_INTERRUPT_TYPE_IRQ: u32 = 0;

// System register IDs (from hv_vcpu_types.h)
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
pub const HV_SYS_REG_CNTP_CTL_EL0: u16 = 0xdf11;
pub const HV_SYS_REG_CNTP_CVAL_EL0: u16 = 0xdf12;
pub const HV_SYS_REG_CNTHCTL_EL2: u16 = 0xe708;
pub const HV_SYS_REG_SP_EL1: u16 = 0xe208;

// GIC ICC register IDs
pub const HV_GIC_ICC_REG_PMR_EL1: u16 = 0xc230;
pub const HV_GIC_ICC_REG_BPR0_EL1: u16 = 0xc643;
pub const HV_GIC_ICC_REG_AP0R0_EL1: u16 = 0xc644;
pub const HV_GIC_ICC_REG_AP1R0_EL1: u16 = 0xc648;
pub const HV_GIC_ICC_REG_BPR1_EL1: u16 = 0xc663;
pub const HV_GIC_ICC_REG_CTLR_EL1: u16 = 0xc664;
pub const HV_GIC_ICC_REG_SRE_EL1: u16 = 0xc665;
pub const HV_GIC_ICC_REG_IGRPEN0_EL1: u16 = 0xc666;
pub const HV_GIC_ICC_REG_IGRPEN1_EL1: u16 = 0xc667;

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
    pub fn hv_vcpu_get_simd_fp_reg(vcpu: HvVcpu, reg: u32, value: *mut HvSimdFpUchar16)
        -> HvReturn;
    pub fn hv_vcpu_set_simd_fp_reg(
        vcpu: HvVcpu,
        reg: u32,
        value: *const HvSimdFpUchar16,
    ) -> HvReturn;
    pub fn hv_vcpu_set_vtimer_mask(vcpu: HvVcpu, vtimer_is_masked: bool) -> HvReturn;
    pub fn hv_vcpu_set_vtimer_offset(vcpu: HvVcpu, vtimer_offset: u64) -> HvReturn;
    pub fn hv_vcpu_get_vtimer_offset(vcpu: HvVcpu, vtimer_offset: *mut u64) -> HvReturn;
    pub fn hv_vcpu_set_pending_interrupt(vcpu: HvVcpu, r#type: u32, pending: bool) -> HvReturn;
    pub fn hv_gic_config_create() -> HvGicConfig;
    pub fn hv_gic_config_set_distributor_base(config: HvGicConfig, addr: HvIpa) -> HvReturn;
    pub fn hv_gic_config_set_redistributor_base(config: HvGicConfig, addr: HvIpa) -> HvReturn;
    pub fn hv_gic_create(gic_config: HvGicConfig) -> HvReturn;
    pub fn hv_gic_reset() -> HvReturn;
    pub fn hv_gic_set_spi(intid: u32, level: bool) -> HvReturn;
    pub fn hv_gic_get_spi_interrupt_range(
        spi_intid_base: *mut u32,
        spi_intid_count: *mut u32,
    ) -> HvReturn;
    pub fn hv_vcpus_exit(vcpus: *const HvVcpu, vcpu_count: u32) -> HvReturn;
    pub fn hv_vm_config_create() -> *mut std::ffi::c_void;
    pub fn hv_vm_config_get_el2_supported(el2_supported: *mut bool) -> HvReturn;
    pub fn hv_vm_config_set_el2_enabled(
        config: *mut std::ffi::c_void,
        el2_enabled: bool,
    ) -> HvReturn;
    pub fn hv_gic_state_create() -> *mut std::ffi::c_void;
    pub fn hv_gic_state_get_size(state: *const std::ffi::c_void, size: *mut usize) -> HvReturn;
    pub fn hv_gic_state_get_data(state: *const std::ffi::c_void, data: *mut u8) -> HvReturn;
    pub fn hv_gic_set_state(data: *const u8, size: usize) -> HvReturn;
    pub fn hv_gic_get_icc_reg(vcpu: HvVcpu, reg: u16, value: *mut u64) -> HvReturn;
    pub fn hv_gic_set_icc_reg(vcpu: HvVcpu, reg: u16, value: u64) -> HvReturn;
}

pub fn check_hv(ret: HvReturn, context: &str) {
    if ret != HV_SUCCESS {
        panic!("{}: HVF returned 0x{:x}", context, ret as u32);
    }
}

pub unsafe fn vcpu_get_reg(vcpu: HvVcpu, reg: u32) -> u64 {
    let mut value: u64 = 0;
    check_hv(hv_vcpu_get_reg(vcpu, reg, &mut value), "hv_vcpu_get_reg");
    value
}

pub unsafe fn vcpu_get_sys_reg(vcpu: HvVcpu, reg: u16) -> u64 {
    let mut value: u64 = 0;
    check_hv(
        hv_vcpu_get_sys_reg(vcpu, reg, &mut value),
        "hv_vcpu_get_sys_reg",
    );
    value
}

// ─── SysReg / IccReg → HVF ID mapping ───────────────────────────────────────

fn sys_reg_to_hvf(reg: SysReg) -> u16 {
    match reg {
        SysReg::MDSCR_EL1 => HV_SYS_REG_MDSCR_EL1,
        SysReg::MPIDR_EL1 => HV_SYS_REG_MPIDR_EL1,
        SysReg::SCTLR_EL1 => HV_SYS_REG_SCTLR_EL1,
        SysReg::CPACR_EL1 => HV_SYS_REG_CPACR_EL1,
        SysReg::TTBR0_EL1 => HV_SYS_REG_TTBR0_EL1,
        SysReg::TTBR1_EL1 => HV_SYS_REG_TTBR1_EL1,
        SysReg::TCR_EL1 => HV_SYS_REG_TCR_EL1,
        SysReg::SPSR_EL1 => HV_SYS_REG_SPSR_EL1,
        SysReg::ELR_EL1 => HV_SYS_REG_ELR_EL1,
        SysReg::SP_EL0 => HV_SYS_REG_SP_EL0,
        SysReg::AFSR0_EL1 => HV_SYS_REG_AFSR0_EL1,
        SysReg::AFSR1_EL1 => HV_SYS_REG_AFSR1_EL1,
        SysReg::ESR_EL1 => HV_SYS_REG_ESR_EL1,
        SysReg::FAR_EL1 => HV_SYS_REG_FAR_EL1,
        SysReg::PAR_EL1 => HV_SYS_REG_PAR_EL1,
        SysReg::MAIR_EL1 => HV_SYS_REG_MAIR_EL1,
        SysReg::AMAIR_EL1 => HV_SYS_REG_AMAIR_EL1,
        SysReg::VBAR_EL1 => HV_SYS_REG_VBAR_EL1,
        SysReg::CONTEXTIDR_EL1 => HV_SYS_REG_CONTEXTIDR_EL1,
        SysReg::TPIDR_EL1 => HV_SYS_REG_TPIDR_EL1,
        SysReg::CNTKCTL_EL1 => HV_SYS_REG_CNTKCTL_EL1,
        SysReg::CSSELR_EL1 => HV_SYS_REG_CSSELR_EL1,
        SysReg::TPIDR_EL0 => HV_SYS_REG_TPIDR_EL0,
        SysReg::TPIDRRO_EL0 => HV_SYS_REG_TPIDRRO_EL0,
        SysReg::CNTV_CTL_EL0 => HV_SYS_REG_CNTV_CTL_EL0,
        SysReg::CNTV_CVAL_EL0 => HV_SYS_REG_CNTV_CVAL_EL0,
        SysReg::CNTP_CTL_EL0 => HV_SYS_REG_CNTP_CTL_EL0,
        SysReg::CNTP_CVAL_EL0 => HV_SYS_REG_CNTP_CVAL_EL0,
        SysReg::CNTHCTL_EL2 => HV_SYS_REG_CNTHCTL_EL2,
        SysReg::SP_EL1 => HV_SYS_REG_SP_EL1,
        SysReg::APIAKEYLO_EL1 => HV_SYS_REG_APIAKEYLO_EL1,
        SysReg::APIAKEYHI_EL1 => HV_SYS_REG_APIAKEYHI_EL1,
        SysReg::APIBKEYLO_EL1 => HV_SYS_REG_APIBKEYLO_EL1,
        SysReg::APIBKEYHI_EL1 => HV_SYS_REG_APIBKEYHI_EL1,
        SysReg::APDAKEYLO_EL1 => HV_SYS_REG_APDAKEYLO_EL1,
        SysReg::APDAKEYHI_EL1 => HV_SYS_REG_APDAKEYHI_EL1,
        SysReg::APDBKEYLO_EL1 => HV_SYS_REG_APDBKEYLO_EL1,
        SysReg::APDBKEYHI_EL1 => HV_SYS_REG_APDBKEYHI_EL1,
        SysReg::APGAKEYLO_EL1 => HV_SYS_REG_APGAKEYLO_EL1,
        SysReg::APGAKEYHI_EL1 => HV_SYS_REG_APGAKEYHI_EL1,
    }
}

fn icc_reg_to_hvf(reg: IccReg) -> u16 {
    match reg {
        IccReg::PMR_EL1 => HV_GIC_ICC_REG_PMR_EL1,
        IccReg::BPR0_EL1 => HV_GIC_ICC_REG_BPR0_EL1,
        IccReg::AP0R0_EL1 => HV_GIC_ICC_REG_AP0R0_EL1,
        IccReg::AP1R0_EL1 => HV_GIC_ICC_REG_AP1R0_EL1,
        IccReg::BPR1_EL1 => HV_GIC_ICC_REG_BPR1_EL1,
        IccReg::CTLR_EL1 => HV_GIC_ICC_REG_CTLR_EL1,
        IccReg::SRE_EL1 => HV_GIC_ICC_REG_SRE_EL1,
        IccReg::IGRPEN0_EL1 => HV_GIC_ICC_REG_IGRPEN0_EL1,
        IccReg::IGRPEN1_EL1 => HV_GIC_ICC_REG_IGRPEN1_EL1,
    }
}

// ─── MMIO decode (data abort syndrome) ───────────────────────────────────────

fn decode_data_abort(syndrome: u64, ipa: u64) -> Option<MmioAccess> {
    let isv = (syndrome >> 24) & 1;
    if isv == 0 {
        return None;
    }
    let sas = (syndrome >> 22) & 3;
    let sse = (syndrome >> 21) & 1;
    let srt = (syndrome >> 16) & 0x1f;
    let wnr = (syndrome >> 6) & 1;
    let len = 1usize << sas;

    // For writes, read the value from the source register.
    // For reads, data is 0 (will be filled by complete_mmio_read).
    // Note: we can't read the register here because we don't have the vcpu handle.
    // The data for writes is filled in by run() after decoding.
    Some(MmioAccess {
        addr: ipa,
        is_write: wnr != 0,
        len,
        reg: srt as u32,
        sign_extend: sse != 0,
        data: 0,
    })
}

// ─── VmHandle ────────────────────────────────────────────────────────────────

pub struct VmHandle {
    _private: (),
}

impl VmHandle {
    /// Create a new HVF VM, with EL2 if supported.
    pub fn create() -> Self {
        unsafe {
            let mut el2_supported = false;
            check_hv(
                hv_vm_config_get_el2_supported(&mut el2_supported),
                "el2_supported",
            );

            if el2_supported {
                let config = hv_vm_config_create();
                assert!(!config.is_null());
                check_hv(hv_vm_config_set_el2_enabled(config, true), "el2_enable");
                check_hv(hv_vm_create(config as *const _), "hv_vm_create (EL2)");
                eprintln!("VM created with EL2 enabled (timer trapping available)");
            } else {
                check_hv(hv_vm_create(ptr::null()), "hv_vm_create");
                eprintln!("VM created without EL2 (timer trapping not available)");
            }
        }
        VmHandle { _private: () }
    }

    pub fn map_memory(&mut self, host_ptr: *mut u8, guest_addr: u64, size: usize, exec: bool) {
        let mut flags = HV_MEMORY_READ | HV_MEMORY_WRITE;
        if exec {
            flags |= HV_MEMORY_EXEC;
        }
        unsafe {
            check_hv(hv_vm_map(host_ptr, guest_addr, size, flags), "hv_vm_map");
        }
    }

    pub fn create_vcpu(&self) -> VcpuHandle {
        let mut vcpu: HvVcpu = 0;
        let mut exit_ptr: *const HvVcpuExit = ptr::null();
        unsafe {
            check_hv(
                hv_vcpu_create(&mut vcpu, &mut exit_ptr, ptr::null()),
                "hv_vcpu_create",
            );
        }
        VcpuHandle {
            vcpu,
            exit_ptr,
            last_mmio_reg: 0,
            last_mmio_sign_extend: false,
            last_mmio_len: 0,
        }
    }

    pub fn create_gic(&self, gicd_base: u64, gicr_base: u64) -> GicHandle {
        unsafe {
            let gic_config = hv_gic_config_create();
            assert!(!gic_config.is_null());
            check_hv(
                hv_gic_config_set_distributor_base(gic_config, gicd_base),
                "set dist base",
            );
            check_hv(
                hv_gic_config_set_redistributor_base(gic_config, gicr_base),
                "set redist base",
            );
            check_hv(hv_gic_create(gic_config), "hv_gic_create");

            let mut spi_base: u32 = 0;
            let mut spi_count: u32 = 0;
            check_hv(
                hv_gic_get_spi_interrupt_range(&mut spi_base, &mut spi_count),
                "get SPI range",
            );
            eprintln!(
                "GIC created: SPI range {}..{}",
                spi_base,
                spi_base + spi_count
            );
        }
        GicHandle { _private: () }
    }
}

impl Drop for VmHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = hv_vm_destroy();
        }
    }
}

// ─── VcpuHandle ──────────────────────────────────────────────────────────────

pub struct VcpuHandle {
    vcpu: HvVcpu,
    exit_ptr: *const HvVcpuExit,
    // Saved from last MMIO read decode, used by complete_mmio_read
    last_mmio_reg: u32,
    last_mmio_sign_extend: bool,
    last_mmio_len: usize,
}

impl VcpuHandle {
    pub fn run(&mut self) -> VcpuExit {
        unsafe {
            check_hv(hv_vcpu_run(self.vcpu), "hv_vcpu_run");
        }

        let exit = unsafe { &*self.exit_ptr };
        match exit.reason {
            HV_EXIT_REASON_EXCEPTION => {
                let syndrome = exit.exception.syndrome;
                let ec = (syndrome >> 26) & 0x3f;
                let ipa = exit.exception.physical_address;

                match ec {
                    // HVC (0x16) or SMC (0x17)
                    0x16 | 0x17 => VcpuExit::Hvc {
                        syndrome,
                        is_smc: ec == 0x17,
                    },
                    // Data abort from lower EL (MMIO)
                    0x24 => {
                        if let Some(mut access) = decode_data_abort(syndrome, ipa) {
                            // For writes, read the value from the guest register
                            if access.is_write {
                                access.data = if access.reg == 31 {
                                    0
                                } else {
                                    unsafe { vcpu_get_reg(self.vcpu, access.reg) }
                                };
                            } else {
                                // Save for complete_mmio_read
                                self.last_mmio_reg = access.reg;
                                self.last_mmio_sign_extend = access.sign_extend;
                                self.last_mmio_len = access.len;
                            }
                            // Advance PC past the faulting instruction
                            unsafe {
                                let pc = vcpu_get_reg(self.vcpu, HV_REG_PC);
                                check_hv(
                                    hv_vcpu_set_reg(self.vcpu, HV_REG_PC, pc + 4),
                                    "advance PC",
                                );
                            }
                            VcpuExit::Mmio(access)
                        } else {
                            VcpuExit::UndecodableMmio { syndrome, ipa }
                        }
                    }
                    // WFI/WFE
                    0x01 => VcpuExit::Wfi,
                    // MSR/MRS trap
                    0x18 => VcpuExit::SysRegTrap { syndrome },
                    _ => VcpuExit::Unknown(ec as u32),
                }
            }
            HV_EXIT_REASON_VTIMER_ACTIVATED => VcpuExit::VtimerActivated,
            HV_EXIT_REASON_CANCELED => VcpuExit::Canceled,
            other => VcpuExit::Unknown(other),
        }
    }

    /// Complete an MMIO read by writing the value to the guest register.
    pub fn complete_mmio_read(&mut self, data: &[u8]) {
        if self.last_mmio_reg == 31 {
            return;
        } // XZR — discard
        let mut value: u64 = 0;
        for (i, &b) in data.iter().enumerate() {
            value |= (b as u64) << (i * 8);
        }
        if self.last_mmio_sign_extend {
            value = match self.last_mmio_len {
                1 => value as u8 as i8 as i64 as u64,
                2 => value as u16 as i16 as i64 as u64,
                4 => value as u32 as i32 as i64 as u64,
                _ => value,
            };
        }
        unsafe {
            check_hv(
                hv_vcpu_set_reg(self.vcpu, self.last_mmio_reg, value),
                "mmio read writeback",
            );
        }
    }

    pub fn get_reg(&self, reg: u32) -> u64 {
        unsafe { vcpu_get_reg(self.vcpu, reg) }
    }

    pub fn set_reg(&self, reg: u32, val: u64) {
        unsafe {
            check_hv(hv_vcpu_set_reg(self.vcpu, reg, val), "set_reg");
        }
    }

    pub fn get_sys_reg(&self, reg: SysReg) -> u64 {
        unsafe { vcpu_get_sys_reg(self.vcpu, sys_reg_to_hvf(reg)) }
    }

    pub fn set_sys_reg(&self, reg: SysReg, val: u64) {
        unsafe {
            check_hv(
                hv_vcpu_set_sys_reg(self.vcpu, sys_reg_to_hvf(reg), val),
                "set_sys_reg",
            );
        }
    }

    pub fn try_set_sys_reg(&self, reg: SysReg, val: u64) -> Result<(), String> {
        let ret = unsafe { hv_vcpu_set_sys_reg(self.vcpu, sys_reg_to_hvf(reg), val) };
        if ret == HV_SUCCESS {
            Ok(())
        } else {
            Err(format!(
                "HVF set_sys_reg({:?}) returned 0x{:x}",
                reg, ret as u32
            ))
        }
    }

    pub fn get_simd_reg(&self, reg: u32) -> SimdReg {
        let mut hvf_val = HvSimdFpUchar16::default();
        unsafe {
            check_hv(
                hv_vcpu_get_simd_fp_reg(self.vcpu, reg, &mut hvf_val),
                "get_simd",
            );
        }
        SimdReg(hvf_val.0)
    }

    pub fn set_simd_reg(&self, reg: u32, val: &SimdReg) {
        let hvf_val = HvSimdFpUchar16(val.0);
        unsafe {
            check_hv(
                hv_vcpu_set_simd_fp_reg(self.vcpu, reg, &hvf_val),
                "set_simd",
            );
        }
    }

    pub fn set_pending_interrupt(&self, pending: bool) {
        unsafe {
            check_hv(
                hv_vcpu_set_pending_interrupt(self.vcpu, HV_INTERRUPT_TYPE_IRQ, pending),
                "set_pending",
            );
        }
    }

    pub fn force_exit(&mut self) {
        unsafe {
            let mut vcpus = [self.vcpu];
            hv_vcpus_exit(vcpus.as_mut_ptr(), 1);
        }
    }

    pub fn clear_immediate_exit(&mut self) {
        // No-op on HVF (force_exit uses hv_vcpus_exit which is one-shot)
    }

    pub fn set_vtimer_mask(&self, masked: bool) {
        unsafe {
            check_hv(hv_vcpu_set_vtimer_mask(self.vcpu, masked), "vtimer_mask");
        }
    }

    pub fn set_vtimer_offset(&self, offset: u64) {
        unsafe {
            check_hv(
                hv_vcpu_set_vtimer_offset(self.vcpu, offset),
                "vtimer_offset",
            );
        }
    }

    pub fn get_vtimer_offset(&self) -> u64 {
        let mut offset: u64 = 0;
        unsafe {
            check_hv(
                hv_vcpu_get_vtimer_offset(self.vcpu, &mut offset),
                "get_vtimer_offset",
            );
        }
        offset
    }

    /// Get the raw HVF vcpu handle (for Phase 1 code that needs direct FFI access).
    /// Enable guest debug. No-op on HVF (BRK patching is KVM-only;
    /// HVF uses HVC patching which doesn't need guest debug).
    pub fn enable_guest_debug(&self) {}

    pub fn raw_vcpu(&self) -> HvVcpu {
        self.vcpu
    }

    /// Get the raw exit pointer (for Phase 1 code).
    pub fn raw_exit_ptr(&self) -> *const HvVcpuExit {
        self.exit_ptr
    }
}

impl Drop for VcpuHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = hv_vcpu_destroy(self.vcpu);
        }
    }
}

// ─── GicHandle ───────────────────────────────────────────────────────────────

pub struct GicHandle {
    _private: (),
}

impl GicHandle {
    pub fn set_spi(&self, intid: u32, level: bool) {
        unsafe {
            check_hv(hv_gic_set_spi(intid, level), "gic_set_spi");
        }
    }

    pub fn save_state(&self) -> Vec<u8> {
        unsafe {
            let state = hv_gic_state_create();
            assert!(!state.is_null());
            let mut size: usize = 0;
            check_hv(hv_gic_state_get_size(state, &mut size), "gic_state_size");
            let mut data = vec![0u8; size];
            check_hv(
                hv_gic_state_get_data(state, data.as_mut_ptr()),
                "gic_state_data",
            );
            data
        }
    }

    pub fn restore_state(&self, data: &[u8]) {
        unsafe {
            check_hv(hv_gic_set_state(data.as_ptr(), data.len()), "gic_set_state");
        }
    }

    pub fn get_icc_reg(&self, vcpu: &VcpuHandle, reg: IccReg) -> u64 {
        let mut val: u64 = 0;
        unsafe {
            check_hv(
                hv_gic_get_icc_reg(vcpu.vcpu, icc_reg_to_hvf(reg), &mut val),
                "get_icc",
            );
        }
        val
    }

    pub fn set_icc_reg(&self, vcpu: &VcpuHandle, reg: IccReg, val: u64) {
        unsafe {
            check_hv(
                hv_gic_set_icc_reg(vcpu.vcpu, icc_reg_to_hvf(reg), val),
                "set_icc",
            );
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simd_fp_type_layout() {
        assert_eq!(std::mem::size_of::<HvSimdFpUchar16>(), 16);
        assert_eq!(std::mem::align_of::<HvSimdFpUchar16>(), 16);
    }

    #[test]
    fn hvf_exit_struct_layout() {
        assert_eq!(std::mem::size_of::<HvVcpuExit>(), 4 + 4 + 24);
        assert_eq!(std::mem::align_of::<HvVcpuExit>(), 8);
    }
}
