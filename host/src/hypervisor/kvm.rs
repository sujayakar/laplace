//! KVM backend for the hypervisor abstraction layer (Linux aarch64).
//!
//! Uses kvm-ioctls and kvm-bindings crates for type-safe KVM access.

use kvm_bindings::*;
use kvm_ioctls::{DeviceFd, Kvm, VcpuFd, VmFd};

use super::types::*;

// ── System register encoding ─────────────────────────────────────────────────
//
// KVM uses the ARM64 system register encoding:
//   KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM64_SYSREG | (Op0 << 14) | (Op1 << 11) | (CRn << 7) | (CRm << 3) | Op2
//
// We encode each SysReg variant as the low 16 bits (Op0..Op2) and combine
// with the KVM prefix at call time.

const fn kvm_sys_reg_id(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    KVM_REG_ARM64 as u64
        | KVM_REG_SIZE_U64 as u64
        | KVM_REG_ARM64_SYSREG as u64
        | (op0 << 14)
        | (op1 << 11)
        | (crn << 7)
        | (crm << 3)
        | op2
}

/// Map SysReg enum to KVM register ID.
fn sys_reg_to_kvm(reg: SysReg) -> u64 {
    match reg {
        //                              Op0  Op1  CRn  CRm  Op2
        SysReg::MDSCR_EL1       => kvm_sys_reg_id(2, 0, 0, 2, 2),
        SysReg::MPIDR_EL1       => kvm_sys_reg_id(3, 0, 0, 0, 5),
        SysReg::SCTLR_EL1       => kvm_sys_reg_id(3, 0, 1, 0, 0),
        SysReg::CPACR_EL1       => kvm_sys_reg_id(3, 0, 1, 0, 2),
        SysReg::TTBR0_EL1       => kvm_sys_reg_id(3, 0, 2, 0, 0),
        SysReg::TTBR1_EL1       => kvm_sys_reg_id(3, 0, 2, 0, 1),
        SysReg::TCR_EL1         => kvm_sys_reg_id(3, 0, 2, 0, 2),
        // SPSR_EL1 and ELR_EL1 are core registers on KVM (in kvm_regs struct),
        // not accessible via KVM_REG_ARM64_SYSREG.
        SysReg::SPSR_EL1        => kvm_core_reg_id(KVM_REG_SIZE_U64 as u64, SPSR_EL1_OFFSET),
        SysReg::ELR_EL1         => kvm_core_reg_id(KVM_REG_SIZE_U64 as u64, ELR_EL1_OFFSET),
        SysReg::AFSR0_EL1       => kvm_sys_reg_id(3, 0, 5, 1, 0),
        SysReg::AFSR1_EL1       => kvm_sys_reg_id(3, 0, 5, 1, 1),
        SysReg::ESR_EL1         => kvm_sys_reg_id(3, 0, 5, 2, 0),
        SysReg::FAR_EL1         => kvm_sys_reg_id(3, 0, 6, 0, 0),
        SysReg::PAR_EL1         => kvm_sys_reg_id(3, 0, 7, 4, 0),
        SysReg::MAIR_EL1        => kvm_sys_reg_id(3, 0, 10, 2, 0),
        SysReg::AMAIR_EL1       => kvm_sys_reg_id(3, 0, 10, 3, 0),
        SysReg::VBAR_EL1        => kvm_sys_reg_id(3, 0, 12, 0, 0),
        SysReg::CONTEXTIDR_EL1  => kvm_sys_reg_id(3, 0, 13, 0, 1),
        SysReg::TPIDR_EL1       => kvm_sys_reg_id(3, 0, 13, 0, 4),
        SysReg::CNTKCTL_EL1     => kvm_sys_reg_id(3, 0, 14, 1, 0),
        SysReg::CSSELR_EL1      => kvm_sys_reg_id(3, 1, 0, 0, 0),
        // SP_EL0 is the user-mode SP. On KVM it's in user_pt_regs.sp (core register).
        SysReg::SP_EL0          => kvm_core_reg_id(KVM_REG_SIZE_U64 as u64, SP_OFFSET),
        SysReg::TPIDR_EL0       => kvm_sys_reg_id(3, 3, 13, 0, 2),
        SysReg::TPIDRRO_EL0     => kvm_sys_reg_id(3, 3, 13, 0, 3),
        SysReg::CNTV_CTL_EL0    => kvm_sys_reg_id(3, 3, 14, 3, 1),
        SysReg::CNTV_CVAL_EL0   => kvm_sys_reg_id(3, 3, 14, 3, 2),
        SysReg::CNTP_CTL_EL0    => kvm_sys_reg_id(3, 3, 14, 2, 1),
        SysReg::CNTP_CVAL_EL0   => kvm_sys_reg_id(3, 3, 14, 2, 2),
        SysReg::CNTHCTL_EL2     => kvm_sys_reg_id(3, 4, 14, 1, 0),
        // SP_EL1 is accessed via core registers on KVM, not as a sysreg.
        // Encode it as the core reg offset into kvm_regs.sp_el1.
        // However, for snapshot compatibility we use the sysreg encoding:
        // MRS/MSR SP_EL1 is accessible at EL2+ only. On KVM, it's exposed
        // via the kvm_regs.sp_el1 core register, not via KVM_REG_ARM64_SYSREG.
        // We handle this specially in get_sys_reg/set_sys_reg.
        SysReg::SP_EL1          => SP_EL1_CORE_REG_ID, // special: core register, not sysreg
        // PAC keys
        SysReg::APIAKEYLO_EL1   => kvm_sys_reg_id(3, 0, 2, 1, 0),
        SysReg::APIAKEYHI_EL1   => kvm_sys_reg_id(3, 0, 2, 1, 1),
        SysReg::APIBKEYLO_EL1   => kvm_sys_reg_id(3, 0, 2, 1, 2),
        SysReg::APIBKEYHI_EL1   => kvm_sys_reg_id(3, 0, 2, 1, 3),
        SysReg::APDAKEYLO_EL1   => kvm_sys_reg_id(3, 0, 2, 2, 0),
        SysReg::APDAKEYHI_EL1   => kvm_sys_reg_id(3, 0, 2, 2, 1),
        SysReg::APDBKEYLO_EL1   => kvm_sys_reg_id(3, 0, 2, 2, 2),
        SysReg::APDBKEYHI_EL1   => kvm_sys_reg_id(3, 0, 2, 2, 3),
        SysReg::APGAKEYLO_EL1   => kvm_sys_reg_id(3, 0, 2, 3, 0),
        SysReg::APGAKEYHI_EL1   => kvm_sys_reg_id(3, 0, 2, 3, 1),
    }
}

// ── Core register encoding ───────────────────────────────────────────────────
//
// KVM core registers use offsets into struct kvm_regs:
//   struct kvm_regs {
//       struct user_pt_regs { u64 regs[31]; u64 sp; u64 pc; u64 pstate; };
//       u64 sp_el1;
//       u64 elr_el1;
//       u64 spsr[KVM_NR_SPSR];  // 5 spsr registers
//       struct user_fpsimd_state { __uint128_t vregs[32]; u32 fpsr; u32 fpcr; };
//   };

fn kvm_core_reg_id(size: u64, byte_offset: u64) -> u64 {
    KVM_REG_ARM64 as u64
        | KVM_REG_ARM_CORE as u64
        | size
        | (byte_offset / 4) // KVM uses u32 granularity
}

// Byte offsets into kvm_regs for core registers.
// user_pt_regs starts at offset 0.
const fn gpr_offset(n: u32) -> u64 {
    (n as u64) * 8 // regs[0..31] are u64, at byte offsets 0, 8, 16, ...
}
const SP_OFFSET: u64 = 31 * 8;         // user_pt_regs.sp
const PC_OFFSET: u64 = 32 * 8;         // user_pt_regs.pc
const PSTATE_OFFSET: u64 = 33 * 8;     // user_pt_regs.pstate
// After user_pt_regs (34 * 8 = 272 bytes):
const SP_EL1_OFFSET: u64 = 34 * 8;
const ELR_EL1_OFFSET: u64 = 35 * 8;
// SPSR array starts at offset 36 * 8 = 288. SPSR_EL1 is spsr[0].
const SPSR_EL1_OFFSET: u64 = 36 * 8;

/// SP_EL1 is a core register on KVM (not a sysreg). It lives in kvm_regs.sp_el1.
const SP_EL1_CORE_REG_ID: u64 = KVM_REG_ARM64 as u64
    | KVM_REG_ARM_CORE as u64
    | KVM_REG_SIZE_U64 as u64
    | (SP_EL1_OFFSET / 4);
// SPSR array: 5 entries at offset 36 * 8
// const SPSR_OFFSET: u64 = 36 * 8;
// FP/SIMD state starts after spsr[5] + padding, at offset (36 + 5 + 1) * 8 = 42 * 8
const FP_REGS_OFFSET: u64 = 42 * 8;    // start of user_fpsimd_state
// vregs[32] are 128-bit each, total 32 * 16 = 512 bytes
const fn simd_offset(n: u32) -> u64 {
    FP_REGS_OFFSET + (n as u64) * 16
}
const FPSR_OFFSET: u64 = FP_REGS_OFFSET + 32 * 16;      // u32 fpsr
const FPCR_OFFSET: u64 = FP_REGS_OFFSET + 32 * 16 + 4;  // u32 fpcr

/// Map our GPR index (0-34) to KVM core register ID.
/// 0-30 = x0-x30, 31 = PC, 32 = FPCR, 33 = FPSR, 34 = CPSR/PSTATE.
fn gpr_to_kvm_id(reg: u32) -> u64 {
    match reg {
        0..=30 => kvm_core_reg_id(KVM_REG_SIZE_U64 as u64, gpr_offset(reg)),
        31 => kvm_core_reg_id(KVM_REG_SIZE_U64 as u64, PC_OFFSET),     // PC
        32 => kvm_core_reg_id(KVM_REG_SIZE_U32 as u64, FPCR_OFFSET),   // FPCR (32-bit)
        33 => kvm_core_reg_id(KVM_REG_SIZE_U32 as u64, FPSR_OFFSET),   // FPSR (32-bit)
        34 => kvm_core_reg_id(KVM_REG_SIZE_U64 as u64, PSTATE_OFFSET), // CPSR/PSTATE
        _ => panic!("invalid GPR index: {}", reg),
    }
}

// ── VmHandle ─────────────────────────────────────────────────────────────────

pub struct VmHandle {
    kvm: Kvm,
    vm: VmFd,
    next_slot: u32,
}

impl VmHandle {
    /// Create a new KVM VM.
    pub fn create() -> Self {
        let kvm = Kvm::new().expect("failed to open /dev/kvm");

        let api_ver = kvm.get_api_version();
        assert_eq!(api_ver, KVM_API_VERSION as i32, "KVM API version mismatch");

        // On aarch64, pass IPA size as vm_type.
        let vm_type = if kvm.check_extension(kvm_ioctls::Cap::ArmVmIPASize) {
            kvm.get_host_ipa_limit() as u64
        } else {
            0
        };
        let vm = kvm
            .create_vm_with_type(vm_type)
            .expect("KVM_CREATE_VM failed");

        VmHandle {
            kvm,
            vm,
            next_slot: 0,
        }
    }

    /// Map a host memory region into the guest physical address space.
    pub fn map_memory(
        &mut self,
        host_ptr: *mut u8,
        guest_addr: u64,
        size: usize,
        _exec: bool, // KVM doesn't distinguish exec permission at mapping time
    ) {
        let slot = self.next_slot;
        self.next_slot += 1;

        let region = kvm_userspace_memory_region {
            slot,
            guest_phys_addr: guest_addr,
            memory_size: size as u64,
            userspace_addr: host_ptr as u64,
            flags: 0,
        };
        unsafe {
            self.vm
                .set_user_memory_region(region)
                .expect("KVM_SET_USER_MEMORY_REGION failed");
        }
    }

    /// Create a vCPU (always CPU 0 for our single-vCPU design).
    /// On aarch64, this also initializes the vCPU with KVM_ARM_VCPU_INIT.
    pub fn create_vcpu(&self) -> VcpuHandle {
        let vcpu = self.vm.create_vcpu(0).expect("KVM_CREATE_VCPU failed");

        // On aarch64, we must initialize the vCPU with KVM_ARM_VCPU_INIT
        // before we can get/set any registers.
        let mut kvi = kvm_vcpu_init::default();
        self.vm.get_preferred_target(&mut kvi)
            .expect("KVM_ARM_PREFERRED_TARGET failed");
        // Enable PSCI v0.2+ (the kernel expects PSCI via HVC)
        kvi.features[0] |= 1 << kvm_bindings::KVM_ARM_VCPU_PSCI_0_2;
        vcpu.vcpu_init(&kvi).expect("KVM_ARM_VCPU_INIT failed");

        eprintln!("KVM vCPU created and initialized (target type {})", kvi.target);
        VcpuHandle { vcpu }
    }

    /// Create and initialize a GICv3 interrupt controller.
    pub fn create_gic(&self, gicd_base: u64, gicr_base: u64) -> GicHandle {
        let mut gic_device = kvm_create_device {
            type_: kvm_device_type_KVM_DEV_TYPE_ARM_VGIC_V3,
            fd: 0,
            flags: 0,
        };
        let device = self
            .vm
            .create_device(&mut gic_device)
            .expect("KVM_CREATE_DEVICE(VGIC_V3) failed");

        let gic = GicHandle { device };

        // Set distributor base address
        gic.set_device_attr(
            KVM_DEV_ARM_VGIC_GRP_ADDR,
            KVM_VGIC_V3_ADDR_TYPE_DIST as u64,
            &gicd_base as *const u64 as u64,
        );

        // Set redistributor base address
        gic.set_device_attr(
            KVM_DEV_ARM_VGIC_GRP_ADDR,
            KVM_VGIC_V3_ADDR_TYPE_REDIST as u64,
            &gicr_base as *const u64 as u64,
        );

        // Set number of IRQs (128 is typical minimum: 32 SGI/PPI + 96 SPI)
        let nr_irqs: u32 = 128;
        gic.set_device_attr(
            KVM_DEV_ARM_VGIC_GRP_NR_IRQS,
            0,
            &nr_irqs as *const u32 as u64,
        );

        // Initialize the GIC
        gic.set_device_attr(
            KVM_DEV_ARM_VGIC_GRP_CTRL,
            KVM_DEV_ARM_VGIC_CTRL_INIT as u64,
            0,
        );

        eprintln!("KVM GIC created: GICD=0x{:x}, GICR=0x{:x}", gicd_base, gicr_base);
        gic
    }

    /// Get the raw VM fd (for operations that need it).
    pub fn vm_fd(&self) -> &VmFd {
        &self.vm
    }
}

impl Drop for VmHandle {
    fn drop(&mut self) {
        // VmFd is closed automatically by kvm-ioctls Drop impl
    }
}

// ── VcpuHandle ───────────────────────────────────────────────────────────────

pub struct VcpuHandle {
    vcpu: VcpuFd,
}

impl VcpuHandle {
    /// Run the vCPU until a VM exit occurs.
    ///
    /// For MMIO reads on KVM: the caller must call `complete_mmio_read()`
    /// with the response data before the next `run()`.
    pub fn run(&mut self) -> VcpuExit {
        match self.vcpu.run() {
            Ok(exit) => match exit {
                kvm_ioctls::VcpuExit::MmioRead(addr, data) => {
                    let len = data.len();
                    VcpuExit::Mmio(MmioAccess {
                        addr,
                        is_write: false,
                        len,
                        reg: 0,
                        sign_extend: false,
                        data: 0,
                    })
                }
                kvm_ioctls::VcpuExit::MmioWrite(addr, data) => {
                    let mut val: u64 = 0;
                    for (i, &b) in data.iter().enumerate() {
                        val |= (b as u64) << (i * 8);
                    }
                    VcpuExit::Mmio(MmioAccess {
                        addr,
                        is_write: true,
                        len: data.len(),
                        reg: 0,
                        sign_extend: false,
                        data: val,
                    })
                }
                kvm_ioctls::VcpuExit::SystemEvent(event_type, _flags) => {
                    VcpuExit::SystemEvent { event_type }
                }
                kvm_ioctls::VcpuExit::Hlt => {
                    VcpuExit::Wfi
                }
                other => {
                    eprintln!("KVM: unexpected exit: {:?}", other);
                    VcpuExit::Unknown(0)
                }
            },
            Err(ref e) => {
                match e.errno() {
                    libc::EAGAIN | libc::EINTR => VcpuExit::Canceled,
                    errno => {
                        panic!("KVM_RUN failed: {} (errno={})", e, errno);
                    }
                }
            }
        }
    }

    /// Complete an MMIO read by writing the response data into kvm_run.
    /// Must be called after `run()` returns `VcpuExit::Mmio` with `is_write=false`
    /// and before the next `run()`.
    pub fn complete_mmio_read(&mut self, data: &[u8]) {
        // The kvm_run struct has the MMIO data field that we need to fill in.
        // After KVM_EXIT_MMIO for a read, we write the data into kvm_run.mmio.data
        // and the next KVM_RUN will deliver it to the guest.
        unsafe {
            let run = &mut *self.vcpu.get_kvm_run();
            let mmio = &mut run.__bindgen_anon_1.mmio;
            let len = mmio.len as usize;
            let dest = &mut mmio.data[..len.min(data.len())];
            dest.copy_from_slice(&data[..dest.len()]);
        }
    }

    /// Get a general-purpose register by index (0-34).
    pub fn get_reg(&self, reg: u32) -> u64 {
        let id = gpr_to_kvm_id(reg);
        // FPCR/FPSR are 32-bit registers
        if reg == REG_FPCR || reg == REG_FPSR {
            let mut bytes = [0u8; 4];
            self.vcpu.get_one_reg(id, &mut bytes)
                .unwrap_or_else(|e| panic!("KVM get_one_reg(GPR {}) failed: {}", reg, e));
            u32::from_le_bytes(bytes) as u64
        } else {
            let mut bytes = [0u8; 8];
            self.vcpu.get_one_reg(id, &mut bytes)
                .unwrap_or_else(|e| panic!("KVM get_one_reg(GPR {}) failed: {}", reg, e));
            u64::from_le_bytes(bytes)
        }
    }

    /// Set a general-purpose register by index (0-34).
    pub fn set_reg(&self, reg: u32, val: u64) {
        let id = gpr_to_kvm_id(reg);
        if reg == REG_FPCR || reg == REG_FPSR {
            let bytes = (val as u32).to_le_bytes();
            self.vcpu.set_one_reg(id, &bytes)
                .unwrap_or_else(|e| panic!("KVM set_one_reg(GPR {}) failed: {}", reg, e));
        } else {
            let bytes = val.to_le_bytes();
            self.vcpu.set_one_reg(id, &bytes)
                .unwrap_or_else(|e| panic!("KVM set_one_reg(GPR {}) failed: {}", reg, e));
        }
    }

    /// Get a system register. Returns 0 if the register is not available
    /// on this KVM (ENOENT — e.g., pKVM restricts some registers).
    /// Panics on unexpected errors (wrong encoding, kernel bug).
    pub fn get_sys_reg(&self, reg: SysReg) -> u64 {
        let id = sys_reg_to_kvm(reg);
        let mut bytes = [0u8; 8];
        match self.vcpu.get_one_reg(id, &mut bytes) {
            Ok(_) => u64::from_le_bytes(bytes),
            Err(ref e) if e.errno() == libc::ENOENT => {
                // Register not available on this KVM (pKVM restriction).
                static WARNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if WARNED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 5 {
                    eprintln!("KVM: get_sys_reg({:?}) unavailable (ENOENT)", reg);
                }
                0
            }
            Err(e) => panic!("KVM get_one_reg({:?}) failed: {}", reg, e),
        }
    }

    /// Set a system register. Silently ignores ENOENT (register not available
    /// on this KVM). Panics on unexpected errors.
    pub fn set_sys_reg(&self, reg: SysReg, val: u64) {
        let id = sys_reg_to_kvm(reg);
        let bytes = val.to_le_bytes();
        match self.vcpu.set_one_reg(id, &bytes) {
            Ok(_) => {}
            Err(ref e) if e.errno() == libc::ENOENT => {
                static WARNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if WARNED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 5 {
                    eprintln!("KVM: set_sys_reg({:?}) unavailable (ENOENT)", reg);
                }
            }
            Err(e) => panic!("KVM set_one_reg({:?}) failed: {}", reg, e),
        }
    }

    /// Try to set a system register, returning Ok/Err instead of panicking.
    pub fn try_set_sys_reg(&self, reg: SysReg, val: u64) -> Result<(), String> {
        let id = sys_reg_to_kvm(reg);
        let bytes = val.to_le_bytes();
        self.vcpu.set_one_reg(id, &bytes)
            .map(|_| ())
            .map_err(|e| format!("KVM set_one_reg({:?}) failed: {}", reg, e))
    }

    /// Get a SIMD/FP register (Q0-Q31, 128-bit).
    pub fn get_simd_reg(&self, reg: u32) -> SimdReg {
        assert!(reg < 32, "SIMD register index out of range");
        let id = kvm_core_reg_id(KVM_REG_SIZE_U128 as u64, simd_offset(reg));
        let mut bytes = [0u8; 16];
        self.vcpu.get_one_reg(id, &mut bytes)
            .unwrap_or_else(|e| panic!("KVM get_one_reg(SIMD Q{}) failed: {}", reg, e));
        SimdReg(bytes)
    }

    /// Set a SIMD/FP register (Q0-Q31, 128-bit).
    pub fn set_simd_reg(&self, reg: u32, val: &SimdReg) {
        assert!(reg < 32, "SIMD register index out of range");
        let id = kvm_core_reg_id(KVM_REG_SIZE_U128 as u64, simd_offset(reg));
        self.vcpu.set_one_reg(id, &val.0)
            .unwrap_or_else(|e| panic!("KVM set_one_reg(SIMD Q{}) failed: {}", reg, e));
    }

    /// Set a pending IRQ interrupt on the vCPU.
    /// On KVM, we use KVM_IRQ_LINE to signal via the GIC.
    /// For the simple case of injecting a timer interrupt, we can
    /// set the vtimer directly.
    pub fn set_pending_interrupt(&self, _pending: bool) {
        // On KVM, timer interrupts are handled by the in-kernel GIC.
        // The vtimer PPI (27) is automatically managed.
        // For manual interrupt injection, use GicHandle::set_spi() instead.
        //
        // TODO: If we need to inject timer interrupts manually for
        // deterministic mode, we'll need a different approach.
    }

    /// Force the vCPU to exit from KVM_RUN.
    /// This is called from a different thread (watchdog).
    pub fn force_exit(&mut self) {
        let run = self.vcpu.get_kvm_run();
        run.immediate_exit = 1;
    }

    /// Clear the immediate_exit flag (call before vcpu.run()).
    pub fn clear_immediate_exit(&mut self) {
        let run = self.vcpu.get_kvm_run();
        run.immediate_exit = 0;
    }

    /// Set vtimer mask. On KVM, the vtimer is managed in-kernel.
    /// This is a no-op for now.
    pub fn set_vtimer_mask(&self, _masked: bool) {
        // KVM manages the vtimer in-kernel. No manual masking needed.
    }

    /// Set vtimer offset. On KVM, this is done via the CNTVOFF_EL2 register.
    pub fn set_vtimer_offset(&self, _offset: u64) {
        // TODO: Set CNTVOFF_EL2 via KVM_SET_ONE_REG when needed for snapshot/fork.
    }

    /// Get vtimer offset.
    pub fn get_vtimer_offset(&self) -> u64 {
        // TODO: Get CNTVOFF_EL2 via KVM_GET_ONE_REG when needed for snapshot/fork.
        0
    }

    /// Get the raw vcpu fd for advanced operations.
    pub fn vcpu_fd(&self) -> &VcpuFd {
        &self.vcpu
    }
}

// ── GicHandle ────────────────────────────────────────────────────────────────

pub struct GicHandle {
    device: DeviceFd,
}

impl GicHandle {
    fn set_device_attr(&self, group: u32, attr: u64, addr: u64) {
        let kvm_attr = kvm_device_attr {
            flags: 0,
            group,
            attr,
            addr,
        };
        self.device
            .set_device_attr(&kvm_attr)
            .unwrap_or_else(|e| {
                panic!(
                    "KVM GIC set_device_attr(group={}, attr={}) failed: {}",
                    group, attr, e
                )
            });
    }

    /// Assert/deassert an SPI (Shared Peripheral Interrupt).
    pub fn set_spi(&self, intid: u32, level: bool) {
        // On KVM, SPIs are typically injected via KVM_IRQ_LINE on the VM fd.
        // However, we don't have access to the VM fd here.
        // For now, this is a placeholder — the timer mechanism works differently on KVM.
        eprintln!(
            "KVM GIC: set_spi({}, {}) - TODO: implement via VM fd",
            intid, level
        );
    }

    /// Save GIC distributor + redistributor state.
    pub fn save_state(&self) -> Vec<u8> {
        // TODO: Iterate over KVM_DEV_ARM_VGIC_GRP_DIST_REGS and
        // KVM_DEV_ARM_VGIC_GRP_REDIST_REGS to save state.
        Vec::new()
    }

    /// Restore GIC state.
    pub fn restore_state(&self, _data: &[u8]) {
        // TODO: Restore via set_device_attr on DIST/REDIST groups.
    }

    /// Get an ICC (CPU interface) register.
    pub fn get_icc_reg(&self, _vcpu: &VcpuHandle, reg: IccReg) -> u64 {
        // ICC registers on KVM are accessed via KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS
        // with the MPIDR and register encoding.
        // TODO: Implement for snapshot/fork.
        let _ = reg;
        0
    }

    /// Set an ICC (CPU interface) register.
    pub fn set_icc_reg(&self, _vcpu: &VcpuHandle, reg: IccReg, val: u64) {
        // TODO: Implement for snapshot/fork.
        let _ = (reg, val);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gpr_to_kvm_id_x0() {
        let id = gpr_to_kvm_id(0);
        // Should have ARM64 | CORE | SIZE_U64 | offset=0
        assert_ne!(id, 0);
        assert_eq!(id & KVM_REG_ARM64 as u64, KVM_REG_ARM64 as u64);
    }

    #[test]
    fn test_gpr_to_kvm_id_pc() {
        let id_pc = gpr_to_kvm_id(REG_PC);
        let id_x0 = gpr_to_kvm_id(0);
        assert_ne!(id_pc, id_x0);
    }

    #[test]
    fn test_sys_reg_encoding() {
        // SCTLR_EL1: Op0=3, Op1=0, CRn=1, CRm=0, Op2=0
        let id = sys_reg_to_kvm(SysReg::SCTLR_EL1);
        assert_eq!(id & KVM_REG_ARM64 as u64, KVM_REG_ARM64 as u64);
        assert_eq!(id & KVM_REG_ARM64_SYSREG as u64, KVM_REG_ARM64_SYSREG as u64);
    }

    #[test]
    fn test_kvm_available() {
        // Verify we can open /dev/kvm
        let kvm = Kvm::new();
        assert!(kvm.is_ok(), "Failed to open /dev/kvm: {:?}", kvm.err());
    }

    #[test]
    fn test_create_vm() {
        let vm = VmHandle::create();
        // If we get here, VM creation succeeded
        drop(vm);
    }
}
