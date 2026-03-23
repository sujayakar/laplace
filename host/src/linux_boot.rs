//! Linux kernel boot support.
//!
//! Loads an ARM64 Linux kernel Image + optional initramfs into guest memory,
//! generates a DTB, sets up the GIC, and boots the kernel.

use std::path::Path;
use std::ptr;

use crate::dtb;
use crate::hvf;
use crate::hvf::check_hv;
use crate::pl011::Pl011;
use crate::psci;
use crate::vtimer::VirtualTimer;

const PAGE_SIZE: usize = 16384;

/// Guest memory layout for Linux boot.
/// We place RAM at a standard base address and use the top of RAM for DTB.
const GUEST_RAM_BASE: u64 = 0x4000_0000;
const GUEST_RAM_SIZE: u64 = 256 * 1024 * 1024; // 256 MiB

/// Kernel is loaded at RAM_BASE + 0x80000 (standard ARM64 Image offset)
const KERNEL_OFFSET: u64 = 0x8_0000;

/// DTB is placed near the top of RAM (last 2 MiB)
const DTB_MAX_SIZE: usize = 2 * 1024 * 1024;

fn page_align(size: usize) -> usize {
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// Allocate page-aligned anonymous memory.
fn alloc_pages(size: usize) -> *mut u8 {
    unsafe {
        let ptr = libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_ANON | libc::MAP_PRIVATE,
            -1,
            0,
        );
        if ptr == libc::MAP_FAILED {
            panic!("mmap failed: {}", std::io::Error::last_os_error());
        }
        ptr as *mut u8
    }
}

/// Parse the MMIO access details from a data abort syndrome (ESR_EL2).
/// EC=0x24 (data abort from lower EL) is expected.
struct MmioAccess {
    /// Guest physical address of the access
    addr: u64,
    /// true = write, false = read
    is_write: bool,
    /// Transfer size in bytes (1, 2, 4, 8)
    len: usize,
    /// Destination/source register number (Rt)
    reg: u32,
    /// Sign extend? (only for reads)
    #[allow(dead_code)]
    sign_extend: bool,
}

fn decode_data_abort(syndrome: u64, ipa: u64) -> Option<MmioAccess> {
    // ISV (Instruction Syndrome Valid) must be set for us to decode
    let isv = (syndrome >> 24) & 1;
    if isv == 0 {
        eprintln!(
            "MMIO data abort without ISV: syndrome=0x{:x}, IPA=0x{:x}",
            syndrome, ipa
        );
        return None;
    }

    let sas = (syndrome >> 22) & 3; // Access size: 0=byte, 1=halfword, 2=word, 3=dword
    let sse = (syndrome >> 21) & 1; // Sign extend
    let srt = (syndrome >> 16) & 0x1f; // Register transfer
    let wnr = (syndrome >> 6) & 1; // Write not Read

    let len = 1usize << sas;

    Some(MmioAccess {
        addr: ipa,
        is_write: wnr != 0,
        len,
        reg: srt as u32,
        sign_extend: sse != 0,
    })
}

/// Boot a Linux kernel.
pub fn cmd_boot_linux(kernel_path: &Path, initrd_path: Option<&Path>) {
    let kernel_data = std::fs::read(kernel_path)
        .unwrap_or_else(|e| panic!("Failed to read kernel: {}", e));
    eprintln!("Kernel image: {} bytes", kernel_data.len());

    let initrd_data = initrd_path.map(|p| {
        let data = std::fs::read(p).unwrap_or_else(|e| panic!("Failed to read initrd: {}", e));
        eprintln!("Initrd: {} bytes", data.len());
        data
    });

    let ram_size = page_align(GUEST_RAM_SIZE as usize);
    let mem = alloc_pages(ram_size);

    // Load kernel at RAM_BASE + KERNEL_OFFSET
    let kernel_load_offset = KERNEL_OFFSET as usize;
    assert!(
        kernel_load_offset + kernel_data.len() < ram_size,
        "Kernel too large for guest RAM"
    );
    unsafe {
        ptr::copy_nonoverlapping(
            kernel_data.as_ptr(),
            mem.add(kernel_load_offset),
            kernel_data.len(),
        );
    }
    let kernel_entry = GUEST_RAM_BASE + KERNEL_OFFSET;
    eprintln!("Kernel loaded at GPA 0x{:x}", kernel_entry);

    // Load initrd after kernel (page-aligned)
    let (initrd_start, initrd_end) = if let Some(ref initrd) = initrd_data {
        let initrd_offset = page_align(kernel_load_offset + kernel_data.len());
        assert!(
            initrd_offset + initrd.len() < ram_size - DTB_MAX_SIZE,
            "Initrd too large"
        );
        unsafe {
            ptr::copy_nonoverlapping(initrd.as_ptr(), mem.add(initrd_offset), initrd.len());
        }
        let start = GUEST_RAM_BASE + initrd_offset as u64;
        let end = start + initrd.len() as u64;
        eprintln!("Initrd loaded at GPA 0x{:x}..0x{:x}", start, end);
        (Some(start), Some(end))
    } else {
        (None, None)
    };

    // Generate DTB and place it near end of RAM
    let dtb_data = dtb::build_dtb(GUEST_RAM_BASE, GUEST_RAM_SIZE, initrd_start, initrd_end);
    let dtb_offset = ram_size - page_align(dtb_data.len());
    unsafe {
        ptr::copy_nonoverlapping(dtb_data.as_ptr(), mem.add(dtb_offset), dtb_data.len());
    }
    let dtb_addr = GUEST_RAM_BASE + dtb_offset as u64;
    eprintln!(
        "DTB placed at GPA 0x{:x} ({} bytes)",
        dtb_addr,
        dtb_data.len()
    );

    // Dump DTB for debugging
    let dtb_path = "/tmp/hvf-linux.dtb";
    std::fs::write(dtb_path, &dtb_data).expect("write DTB dump");
    eprintln!("DTB dumped to {} (inspect with: dtc -I dtb -O dts {})", dtb_path, dtb_path);

    // Create VM
    unsafe {
        check_hv(hvf::hv_vm_create(ptr::null()), "hv_vm_create");
    }

    // Map guest RAM
    unsafe {
        check_hv(
            hvf::hv_vm_map(
                mem,
                GUEST_RAM_BASE,
                ram_size,
                hvf::HV_MEMORY_READ | hvf::HV_MEMORY_WRITE | hvf::HV_MEMORY_EXEC,
            ),
            "hv_vm_map RAM",
        );
    }

    // Create and configure GIC
    setup_gic();

    // Create vCPU
    let mut vcpu: u64 = 0;
    let mut exit_ptr: *const hvf::HvVcpuExit = ptr::null();
    unsafe {
        check_hv(
            hvf::hv_vcpu_create(&mut vcpu, &mut exit_ptr, ptr::null()),
            "hv_vcpu_create",
        );
    }

    // Set up initial CPU state for Linux boot
    setup_cpu_for_linux(vcpu, kernel_entry, dtb_addr);

    // Create devices
    let uart = Pl011::new(dtb::UART_BASE);
    let mut vtimer = VirtualTimer::new();

    // Query the redistributor base for this vCPU
    unsafe {
        let mut redist_base: u64 = 0;
        let ret = hvf::hv_gic_get_redistributor_base(vcpu, &mut redist_base);
        eprintln!("Redistributor base for vCPU: 0x{:x} (ret={})", redist_base, ret);
    }

    eprintln!("Starting Linux kernel...\n");

    // Spawn a watchdog thread that periodically forces VM exits so we can
    // check status and inject timer interrupts if needed.
    let vcpu_copy = vcpu;
    let watchdog = std::thread::spawn(move || {
        for _ in 0..100 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            unsafe {
                let mut vcpus = [vcpu_copy];
                hvf::hv_vcpus_exit(vcpus.as_mut_ptr(), 1);
            }
        }
    });

    // Run the vCPU loop
    run_linux_vcpu_loop(vcpu, exit_ptr, mem, ram_size, &uart, &mut vtimer);
    let _ = watchdog.join();

    // Cleanup
    unsafe {
        check_hv(hvf::hv_vcpu_destroy(vcpu), "hv_vcpu_destroy");
        check_hv(hvf::hv_vm_destroy(), "hv_vm_destroy");
        libc::munmap(mem as *mut libc::c_void, ram_size);
    }
}

fn setup_gic() {
    unsafe {
        let gic_config = hvf::hv_gic_config_create();
        assert!(!gic_config.is_null(), "hv_gic_config_create returned null");

        check_hv(
            hvf::hv_gic_config_set_distributor_base(gic_config, dtb::GICD_BASE),
            "set distributor base",
        );
        check_hv(
            hvf::hv_gic_config_set_redistributor_base(gic_config, dtb::GICR_BASE),
            "set redistributor base",
        );
        check_hv(hvf::hv_gic_create(gic_config), "hv_gic_create");

        // Query SPI range for debugging
        let mut spi_base: u32 = 0;
        let mut spi_count: u32 = 0;
        check_hv(
            hvf::hv_gic_get_spi_interrupt_range(&mut spi_base, &mut spi_count),
            "get SPI range",
        );
        eprintln!("GIC created: SPI range {}..{}", spi_base, spi_base + spi_count);
    }
}

fn setup_cpu_for_linux(vcpu: u64, kernel_entry: u64, dtb_addr: u64) {
    unsafe {
        // PC = kernel entry point
        check_hv(hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_PC, kernel_entry), "set PC");

        // CPSR = EL1h with all interrupts masked
        check_hv(hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_CPSR, 0x3c5), "set CPSR");

        // x0 = DTB address (Linux boot protocol)
        check_hv(hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_X0, dtb_addr), "set x0=dtb");

        // x1, x2, x3 = 0 (reserved by boot protocol)
        check_hv(hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_X1, 0), "set x1");
        check_hv(hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_X2, 0), "set x2");
        check_hv(hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_X3, 0), "set x3");

        // SCTLR_EL1: MMU off, caches off (kernel will enable these itself)
        check_hv(
            hvf::hv_vcpu_set_sys_reg(vcpu, hvf::HV_SYS_REG_SCTLR_EL1, 0x30d00800),
            "set SCTLR_EL1",
        );

        // Enable SIMD/FP at EL1
        check_hv(
            hvf::hv_vcpu_set_sys_reg(vcpu, hvf::HV_SYS_REG_CPACR_EL1, 3 << 20),
            "set CPACR_EL1",
        );

        // Set MPIDR_EL1 for CPU 0 (affinity 0.0.0.0)
        // The GIC needs this to route interrupts
        check_hv(
            hvf::hv_vcpu_set_sys_reg(vcpu, hvf::HV_SYS_REG_MPIDR_EL1, 0x8000_0000),
            "set MPIDR_EL1",
        );

        // Don't mask the vtimer — let HVF handle it for now so the kernel
        // can boot with real-time timer interrupts. We'll switch to fully
        // virtualized timer once boot works.
        check_hv(hvf::hv_vcpu_set_vtimer_mask(vcpu, false), "unmask vtimer");
    }
}

fn run_linux_vcpu_loop(
    vcpu: u64,
    exit_ptr: *const hvf::HvVcpuExit,
    guest_mem: *mut u8,
    _mem_size: usize,
    uart: &Pl011,
    vtimer: &mut VirtualTimer,
) {
    let mut exit_count: u64 = 0;
    let mut mmio_count: u64 = 0;
    let mut hvc_count: u64 = 0;
    let mut timer_count: u64 = 0;
    let mut wfi_count: u64 = 0;
    let mut sysreg_count: u64 = 0;

    loop {
        if exit_count > 0 && exit_count % 100_000 == 0 {
            let pc = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC) };
            eprintln!(
                "[{} exits] PC=0x{:x} mmio={} hvc={} timer={} wfi={} sysreg={}",
                exit_count, pc, mmio_count, hvc_count, timer_count, wfi_count, sysreg_count,
            );
        }
        if exit_count > 10_000_000 {
            eprintln!("Too many exits, aborting");
            print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
            return;
        }
        // Check for pending timer interrupt before entry
        if vtimer.check_pending() {
            unsafe {
                check_hv(
                    hvf::hv_vcpu_set_pending_interrupt(
                        vcpu,
                        hvf::HV_INTERRUPT_TYPE_IRQ,
                        true,
                    ),
                    "set pending IRQ",
                );
            }
        }

        unsafe {
            check_hv(hvf::hv_vcpu_run(vcpu), "hv_vcpu_run");
        }

        exit_count += 1;
        let exit = unsafe { &*exit_ptr };

        match exit.reason {
            hvf::HV_EXIT_REASON_EXCEPTION => {
                let syndrome = exit.exception.syndrome;
                let ec = (syndrome >> 26) & 0x3f;
                let ipa = exit.exception.physical_address;

                match ec {
                    // HVC from AArch64
                    0x16 => {
                        hvc_count += 1;
                        let x0 = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_X0) };
                        // Try PSCI first
                        if let Some(result) = psci::handle_psci(x0 as u32) {
                            match result {
                                psci::PsciResult::Return(val) => unsafe {
                                    check_hv(
                                        hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_X0, val),
                                        "set x0 psci",
                                    );
                                },
                                psci::PsciResult::SystemOff => {
                                    eprintln!("\nPSCI SYSTEM_OFF");
                                    print_exit_stats(
                                        exit_count, mmio_count, hvc_count, timer_count, wfi_count,
                                    );
                                    return;
                                }
                                psci::PsciResult::SystemReset => {
                                    eprintln!("\nPSCI SYSTEM_RESET");
                                    print_exit_stats(
                                        exit_count, mmio_count, hvc_count, timer_count, wfi_count,
                                    );
                                    return;
                                }
                            }
                        } else {
                            // Unknown HVC — log and return error
                            eprintln!("Unknown HVC: x0=0x{:x}", x0);
                            unsafe {
                                check_hv(
                                    hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_X0, u64::MAX),
                                    "set x0 err",
                                );
                            }
                        }
                    }

                    // SMC from AArch64
                    0x17 => {
                        hvc_count += 1;
                        let x0 = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_X0) };

                        // PSCI can also come via SMC
                        if let Some(result) = psci::handle_psci(x0 as u32) {
                            match result {
                                psci::PsciResult::Return(val) => unsafe {
                                    check_hv(
                                        hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_X0, val),
                                        "set x0 psci",
                                    );
                                },
                                psci::PsciResult::SystemOff => {
                                    eprintln!("\nPSCI SYSTEM_OFF (via SMC)");
                                    print_exit_stats(
                                        exit_count, mmio_count, hvc_count, timer_count, wfi_count,
                                    );
                                    return;
                                }
                                psci::PsciResult::SystemReset => {
                                    eprintln!("\nPSCI SYSTEM_RESET (via SMC)");
                                    print_exit_stats(
                                        exit_count, mmio_count, hvc_count, timer_count, wfi_count,
                                    );
                                    return;
                                }
                            }
                        } else {
                            eprintln!("Unknown SMC: x0=0x{:x}", x0);
                            unsafe {
                                check_hv(
                                    hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_X0, u64::MAX),
                                    "set x0 err",
                                );
                            }
                        }
                        // SMC traps don't auto-advance PC; we must skip the instruction
                        unsafe {
                            let pc = hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC);
                            check_hv(
                                hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_PC, pc + 4),
                                "advance PC past SMC",
                            );
                        }
                    }

                    // Data abort from lower EL (MMIO)
                    0x24 => {
                        mmio_count += 1;
                        if let Some(access) = decode_data_abort(syndrome, ipa) {
                            handle_mmio(vcpu, &access, guest_mem, uart, vtimer);
                        } else {
                            let pc = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC) };
                            eprintln!(
                                "Undecodable data abort: syndrome=0x{:x} IPA=0x{:x} PC=0x{:x}",
                                syndrome, ipa, pc
                            );
                            // Skip the faulting instruction to avoid infinite loop
                            unsafe {
                                check_hv(
                                    hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_PC, pc + 4),
                                    "advance PC past data abort",
                                );
                            }
                        }
                    }

                    // WFI/WFE
                    0x01 => {
                        wfi_count += 1;
                        let inject = vtimer.handle_wfi();
                        if inject {
                            timer_count += 1;
                            unsafe {
                                check_hv(
                                    hvf::hv_vcpu_set_pending_interrupt(
                                        vcpu,
                                        hvf::HV_INTERRUPT_TYPE_IRQ,
                                        true,
                                    ),
                                    "inject timer IRQ after WFI",
                                );
                            }
                        }
                    }

                    // MSR/MRS trap (system register access)
                    0x18 => {
                        sysreg_count += 1;
                        handle_sys_reg_trap(vcpu, syndrome, vtimer);
                    }

                    _ => {
                        let pc = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC) };
                        let esr = unsafe {
                            hvf::vcpu_get_sys_reg(vcpu, hvf::HV_SYS_REG_ESR_EL1)
                        };
                        eprintln!(
                            "Unexpected exception: EC=0x{:x} syndrome=0x{:x} PC=0x{:x} IPA=0x{:x} ESR_EL1=0x{:x}",
                            ec, syndrome, pc, ipa, esr,
                        );
                        print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                        return;
                    }
                }
            }

            hvf::HV_EXIT_REASON_VTIMER_ACTIVATED => {
                // HVF's vtimer fired — inject IRQ to guest and unmask for next time.
                timer_count += 1;
                unsafe {
                    // Mask vtimer (HVF requires this), inject IRQ, then unmask
                    // after the guest handles it (the EOI path will re-arm).
                    check_hv(hvf::hv_vcpu_set_vtimer_mask(vcpu, true), "mask vtimer");
                    check_hv(
                        hvf::hv_vcpu_set_pending_interrupt(
                            vcpu,
                            hvf::HV_INTERRUPT_TYPE_IRQ,
                            true,
                        ),
                        "inject timer IRQ",
                    );
                }
            }

            hvf::HV_EXIT_REASON_CANCELED => {
                // Forced exit from watchdog thread — just continue.
                // Unmask vtimer in case HVF masked it.
                unsafe {
                    let _ = hvf::hv_vcpu_set_vtimer_mask(vcpu, false);
                }
            }

            other => {
                let pc = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC) };
                eprintln!(
                    "Unexpected VM exit: reason={} PC=0x{:x}",
                    other, pc,
                );
                print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                return;
            }
        }
    }
}

fn handle_mmio(
    vcpu: u64,
    access: &MmioAccess,
    _guest_mem: *mut u8,
    uart: &Pl011,
    _vtimer: &mut VirtualTimer,
) {
    if uart.contains(access.addr) {
        let offset = access.addr - uart.base_addr;
        if access.is_write {
            let value = unsafe { hvf::vcpu_get_reg(vcpu, access.reg) };
            uart.write(offset, value, access.len);
        } else {
            let value = uart.read(offset, access.len);
            unsafe {
                check_hv(
                    hvf::hv_vcpu_set_reg(vcpu, access.reg, value),
                    "set reg from MMIO read",
                );
            }
        }
    } else {
        // Unknown MMIO region — log once and return 0 for reads
        static LOGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let prev = LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if prev < 20 {
            let pc = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC) };
            eprintln!(
                "MMIO {} to unmapped addr 0x{:x} (size={}, reg=x{}, PC=0x{:x})",
                if access.is_write { "write" } else { "read" },
                access.addr,
                access.len,
                access.reg,
                pc,
            );
        }
        if !access.is_write {
            unsafe {
                check_hv(
                    hvf::hv_vcpu_set_reg(vcpu, access.reg, 0),
                    "set reg from unknown MMIO",
                );
            }
        }
    }

    // Advance PC past the faulting instruction
    unsafe {
        let pc = hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC);
        check_hv(
            hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_PC, pc + 4),
            "advance PC past MMIO",
        );
    }
}

/// Handle trapped MSR/MRS (system register access, EC=0x18).
/// Used for timer register trapping.
fn handle_sys_reg_trap(vcpu: u64, syndrome: u64, vtimer: &mut VirtualTimer) {
    // Decode ISS for MSR/MRS:
    // bit 0: direction (0=write/MSR, 1=read/MRS)
    // bits 4:1: CRm
    // bits 8:5: Rt (register)
    // bits 11:9: CRn (but encoding is different)
    // bits 13:12: Op1 (but in ISS encoding)
    // bits 16:14: Op2
    // bits 19:17: Op0
    // bit 20: Op1 high bit
    //
    // Actually the ISS encoding for MSR/MRS traps is:
    // [24]: CV (condition valid)
    // [23:20]: COND
    // [19:17]: Op0
    // [16:14]: Op2
    // [13:10]: CRn
    // [9:5]: Rt
    // [4:1]: CRm
    // [0]: direction (1=read, 0=write)

    let direction = syndrome & 1; // 1 = read (MRS), 0 = write (MSR)
    let crm = (syndrome >> 1) & 0xf;
    let rt = ((syndrome >> 5) & 0x1f) as u32;
    let crn = (syndrome >> 10) & 0xf;
    let op1 = (syndrome >> 14) & 0x7;
    let op2 = (syndrome >> 17) & 0x7;
    let op0 = (syndrome >> 20) & 0x3;

    // Identify the register being accessed.
    // CNTVCT_EL0: op0=3, op1=3, CRn=14, CRm=0, op2=2
    // CNTPCT_EL0: op0=3, op1=3, CRn=14, CRm=0, op2=1
    // CNTV_CTL_EL0: op0=3, op1=3, CRn=14, CRm=3, op2=1
    // CNTV_CVAL_EL0: op0=3, op1=3, CRn=14, CRm=3, op2=2
    // CNTV_TVAL_EL0: op0=3, op1=3, CRn=14, CRm=3, op2=0
    // CNTFRQ_EL0: op0=3, op1=3, CRn=14, CRm=0, op2=0

    let is_timer = op0 == 3 && op1 == 3 && crn == 14;

    if is_timer {
        match (crm, op2, direction) {
            // CNTFRQ_EL0 read
            (0, 0, 1) => unsafe {
                check_hv(
                    hvf::hv_vcpu_set_reg(vcpu, rt, crate::vtimer::COUNTER_FREQ_HZ),
                    "set CNTFRQ",
                );
            },
            // CNTPCT_EL0 read (physical counter)
            (0, 1, 1) => {
                let val = vtimer.read_counter();
                unsafe {
                    check_hv(hvf::hv_vcpu_set_reg(vcpu, rt, val), "set CNTPCT");
                }
            }
            // CNTVCT_EL0 read (virtual counter)
            (0, 2, 1) => {
                let val = vtimer.read_counter();
                unsafe {
                    check_hv(hvf::hv_vcpu_set_reg(vcpu, rt, val), "set CNTVCT");
                }
            }
            // CNTV_TVAL_EL0 write
            (3, 0, 0) => {
                let val = unsafe { hvf::vcpu_get_reg(vcpu, rt) };
                vtimer.write_tval(val);
            }
            // CNTV_TVAL_EL0 read
            (3, 0, 1) => {
                let val = vtimer.counter.wrapping_sub(vtimer.read_cval()) as i32;
                unsafe {
                    check_hv(
                        hvf::hv_vcpu_set_reg(vcpu, rt, val as u64),
                        "set CNTV_TVAL",
                    );
                }
            }
            // CNTV_CTL_EL0 write
            (3, 1, 0) => {
                let val = unsafe { hvf::vcpu_get_reg(vcpu, rt) };
                vtimer.write_ctl(val);
                // Unmask HVF vtimer if guest enables the timer
                // (so we get VTIMER_ACTIVATED exits as a fallback)
                unsafe {
                    let _ = hvf::hv_vcpu_set_vtimer_mask(vcpu, (val & 2) != 0);
                }
            }
            // CNTV_CTL_EL0 read
            (3, 1, 1) => {
                let val = vtimer.read_ctl();
                unsafe {
                    check_hv(hvf::hv_vcpu_set_reg(vcpu, rt, val), "set CNTV_CTL");
                }
            }
            // CNTV_CVAL_EL0 write
            (3, 2, 0) => {
                let val = unsafe { hvf::vcpu_get_reg(vcpu, rt) };
                vtimer.write_cval(val);
            }
            // CNTV_CVAL_EL0 read
            (3, 2, 1) => {
                let val = vtimer.read_cval();
                unsafe {
                    check_hv(hvf::hv_vcpu_set_reg(vcpu, rt, val), "set CNTV_CVAL");
                }
            }
            _ => {
                let pc = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC) };
                eprintln!(
                    "Unhandled timer reg access: CRm={} op2={} dir={} PC=0x{:x}",
                    crm, op2, direction, pc
                );
            }
        }
    } else {
        let pc = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC) };
        eprintln!(
            "Trapped sys reg: op0={} op1={} CRn={} CRm={} op2={} dir={} Rt=x{} PC=0x{:x}",
            op0, op1, crn, crm, op2, direction, rt, pc
        );
        // Return 0 for reads of unknown sys regs
        if direction == 1 {
            unsafe {
                check_hv(
                    hvf::hv_vcpu_set_reg(vcpu, rt, 0),
                    "set reg for unknown sysreg",
                );
            }
        }
    }

    // Advance PC past the trapped instruction
    unsafe {
        let pc = hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC);
        check_hv(
            hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_PC, pc + 4),
            "advance PC past sysreg trap",
        );
    }
}

fn print_exit_stats(exits: u64, mmio: u64, hvc: u64, timer: u64, wfi: u64) {
    eprintln!("\nVM exit stats:");
    eprintln!("  total exits: {}", exits);
    eprintln!("  MMIO:        {}", mmio);
    eprintln!("  HVC/SMC:     {}", hvc);
    eprintln!("  timer:       {}", timer);
    eprintln!("  WFI:         {}", wfi);
    eprintln!("  other:       {}", exits - mmio - hvc - timer - wfi);
}
