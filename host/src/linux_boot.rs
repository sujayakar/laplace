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

use crate::{alloc_pages, page_align};

/// Guest memory layout for Linux boot.
/// We place RAM at a standard base address and use the top of RAM for DTB.
const GUEST_RAM_BASE: u64 = 0x4000_0000;
const GUEST_RAM_SIZE: u64 = 256 * 1024 * 1024; // 256 MiB

/// Kernel is loaded at RAM_BASE + 0x80000 (standard ARM64 Image offset)
const KERNEL_OFFSET: u64 = 0x8_0000;

/// DTB is placed near the top of RAM (last 2 MiB)
const DTB_MAX_SIZE: usize = 2 * 1024 * 1024;

/// Result of loading a kernel + initrd + DTB into guest memory.
struct LoadedKernel {
    mem: *mut u8,
    ram_size: usize,
    kernel_entry: u64,
    dtb_addr: u64,
}

/// Load kernel image, optional initrd, and generated DTB into a freshly
/// allocated guest memory region.
fn load_kernel_and_initrd(kernel_path: &Path, initrd_path: Option<&Path>) -> LoadedKernel {
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

    // Read the kernel's image_size from the ARM64 Image header (offset 0x10).
    // This includes BSS and is larger than the file — we must not place the
    // initrd within this region or the kernel's BSS zeroing will overwrite it.
    let kernel_image_size = if kernel_data.len() >= 0x18 {
        u64::from_le_bytes(kernel_data[0x10..0x18].try_into().unwrap()) as usize
    } else {
        kernel_data.len()
    };
    eprintln!(
        "Kernel loaded at GPA 0x{:x} (file={}, image_size={})",
        kernel_entry,
        kernel_data.len(),
        kernel_image_size,
    );

    // Load initrd after kernel image_size (page-aligned), not after file size
    let (initrd_start, initrd_end) = if let Some(ref initrd) = initrd_data {
        let initrd_offset = page_align(kernel_load_offset + kernel_image_size);
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

    LoadedKernel {
        mem,
        ram_size,
        kernel_entry,
        dtb_addr,
    }
}

/// Spawn a watchdog thread that periodically forces VM exits so we can
/// check status and inject timer interrupts.
fn spawn_watchdog(vcpu: u64, duration_secs: u32) -> std::thread::JoinHandle<()> {
    let iterations = (duration_secs as u64) * 10; // 100ms per iteration
    std::thread::spawn(move || {
        for _ in 0..iterations {
            std::thread::sleep(std::time::Duration::from_millis(100));
            unsafe {
                let mut vcpus = [vcpu];
                hvf::hv_vcpus_exit(vcpus.as_mut_ptr(), 1);
            }
        }
    })
}

/// Mailbox: 64 KiB at a fixed GPA **outside** the RAM region.
/// Placed below RAM so the kernel doesn't include it in its memory map,
/// allowing userspace to access it via /dev/mem without STRICT_DEVMEM blocking.
/// Must match MAILBOX_GPA in init/src/main.rs.
pub const LINUX_MAILBOX_GPA: u64 = 0x3FFF_0000;
pub const LINUX_MAILBOX_SIZE: usize = 64 * 1024;

/// Allocate and map the mailbox page into the VM at LINUX_MAILBOX_GPA.
/// Returns the host pointer to the mailbox memory.
fn setup_mailbox() -> *mut u8 {
    let mailbox_mem = alloc_pages(LINUX_MAILBOX_SIZE);
    unsafe {
        check_hv(
            hvf::hv_vm_map(
                mailbox_mem,
                LINUX_MAILBOX_GPA,
                LINUX_MAILBOX_SIZE,
                hvf::HV_MEMORY_READ | hvf::HV_MEMORY_WRITE,
            ),
            "hv_vm_map mailbox",
        );
    }
    mailbox_mem
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
    let loaded = load_kernel_and_initrd(kernel_path, initrd_path);
    let LoadedKernel { mem, ram_size, kernel_entry, dtb_addr } = loaded;

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

    // Create and configure GIC + mailbox
    setup_gic();
    let _mailbox_mem = setup_mailbox();

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

    eprintln!("Starting Linux kernel...\n");

    let watchdog = spawn_watchdog(vcpu, 10);

    // Run the vCPU loop
    let result = run_linux_vcpu_loop(vcpu, exit_ptr, mem, ram_size, &uart, &mut vtimer, None);
    let _ = watchdog.join();

    match result {
        VmExitReason::Ready => eprintln!("VM reached HC_READY (use snapshot-linux to save)"),
        VmExitReason::Exit(code) => eprintln!("VM exited with code {}", code),
        VmExitReason::SystemOff => eprintln!("VM powered off"),
        VmExitReason::Error(e) => eprintln!("VM error: {}", e),
    }

    // Cleanup
    unsafe {
        check_hv(hvf::hv_vcpu_destroy(vcpu), "hv_vcpu_destroy");
        check_hv(hvf::hv_vm_destroy(), "hv_vm_destroy");
        libc::munmap(mem as *mut libc::c_void, ram_size);
    }
}

// ── Snapshot/Fork ─────────────────────────────────────────────────────────────

use crate::snapshot::{CpuState, Template};

/// Save GIC state to a byte vector.
fn save_gic_state() -> Vec<u8> {
    unsafe {
        let state = hvf::hv_gic_state_create();
        assert!(!state.is_null(), "hv_gic_state_create returned null");

        let mut size: usize = 0;
        check_hv(
            hvf::hv_gic_state_get_size(state, &mut size),
            "hv_gic_state_get_size",
        );

        let mut data = vec![0u8; size];
        check_hv(
            hvf::hv_gic_state_get_data(state, data.as_mut_ptr()),
            "hv_gic_state_get_data",
        );

        eprintln!("GIC state: {} bytes", size);
        data
    }
}

/// Boot Linux, run to HC_READY, snapshot, then destroy the VM.
pub fn cmd_snapshot_linux(
    kernel_path: &Path,
    initrd_path: Option<&Path>,
    template_dir: &Path,
) {
    let loaded = load_kernel_and_initrd(kernel_path, initrd_path);
    let LoadedKernel { mem, ram_size, kernel_entry, dtb_addr } = loaded;

    // Create VM, GIC, vCPU
    unsafe { check_hv(hvf::hv_vm_create(ptr::null()), "hv_vm_create"); }
    unsafe {
        check_hv(hvf::hv_vm_map(mem, GUEST_RAM_BASE, ram_size,
            hvf::HV_MEMORY_READ | hvf::HV_MEMORY_WRITE | hvf::HV_MEMORY_EXEC), "hv_vm_map");
    }
    setup_gic();
    let mailbox_mem = setup_mailbox();

    let mut vcpu: u64 = 0;
    let mut exit_ptr: *const hvf::HvVcpuExit = ptr::null();
    unsafe {
        check_hv(hvf::hv_vcpu_create(&mut vcpu, &mut exit_ptr, ptr::null()), "hv_vcpu_create");
    }
    setup_cpu_for_linux(vcpu, kernel_entry, dtb_addr);

    let uart = Pl011::new(dtb::UART_BASE);
    let mut vtimer = VirtualTimer::new();

    eprintln!("Booting Linux to snapshot point...\n");

    let watchdog = spawn_watchdog(vcpu, 30);

    let result = run_linux_vcpu_loop(
        vcpu, exit_ptr, mem, ram_size, &uart, &mut vtimer,
        Some(mailbox_mem as *const u8),
    );
    let _ = watchdog.join();

    match result {
        VmExitReason::Ready => eprintln!("Init signaled READY"),
        VmExitReason::SystemOff => panic!("VM halted before signaling READY"),
        VmExitReason::Exit(c) => panic!("VM exited with code {} before READY", c),
        VmExitReason::Error(e) => panic!("VM error before READY: {}", e),
    }

    // Save CPU state
    let cpu_state = unsafe { CpuState::capture(vcpu) };

    // Save GIC state
    let gic_state = save_gic_state();

    // Write template
    std::fs::create_dir_all(template_dir).expect("create template dir");

    // Guest memory
    let mem_path = template_dir.join("guest.mem");
    let mem_bytes = unsafe { std::slice::from_raw_parts(mem, ram_size) };
    std::fs::write(&mem_path, mem_bytes).expect("write guest.mem");

    // CPU state
    let template = Template {
        cpu_state,
        mem_path: mem_path.clone(),
        mem_size: ram_size,
        guest_base: GUEST_RAM_BASE,
    };
    template.save(template_dir);

    // GIC state
    std::fs::write(template_dir.join("gic.state"), &gic_state).expect("write gic.state");

    // Mailbox memory (separate from guest RAM since it's at a different GPA)
    let mailbox_bytes = unsafe { std::slice::from_raw_parts(mailbox_mem, LINUX_MAILBOX_SIZE) };
    std::fs::write(template_dir.join("mailbox.mem"), mailbox_bytes).expect("write mailbox.mem");

    // Cleanup
    unsafe {
        check_hv(hvf::hv_vcpu_destroy(vcpu), "hv_vcpu_destroy");
        check_hv(hvf::hv_vm_destroy(), "hv_vm_destroy");
        libc::munmap(mem as *mut libc::c_void, ram_size);
    }

    eprintln!(
        "Template saved to {}/ (mem={:.1} MiB, gic={} bytes)",
        template_dir.display(),
        ram_size as f64 / (1024.0 * 1024.0),
        gic_state.len(),
    );
}

/// Fork from a Linux VM template: CoW mmap, restore state, write mailbox, run.
pub fn cmd_fork_linux(template_dir: &Path, mailbox_data: &[u8]) {
    let template = Template::load(template_dir);
    let mem = template.mmap_cow_memory();
    let ram_size = template.mem_size;

    // Allocate mailbox memory and write per-fork data
    let mailbox_mem = alloc_pages(LINUX_MAILBOX_SIZE);
    assert!(mailbox_data.len() < LINUX_MAILBOX_SIZE, "mailbox data too large");
    unsafe {
        ptr::copy_nonoverlapping(mailbox_data.as_ptr(), mailbox_mem, mailbox_data.len());
        *mailbox_mem.add(mailbox_data.len()) = 0; // null-terminate
    }

    // Create VM
    unsafe { check_hv(hvf::hv_vm_create(ptr::null()), "hv_vm_create fork"); }
    unsafe {
        check_hv(hvf::hv_vm_map(mem, GUEST_RAM_BASE, ram_size,
            hvf::HV_MEMORY_READ | hvf::HV_MEMORY_WRITE | hvf::HV_MEMORY_EXEC), "hv_vm_map fork");
    }

    // Map mailbox at its own GPA
    unsafe {
        check_hv(hvf::hv_vm_map(mailbox_mem, LINUX_MAILBOX_GPA, LINUX_MAILBOX_SIZE,
            hvf::HV_MEMORY_READ | hvf::HV_MEMORY_WRITE), "hv_vm_map mailbox fork");
    }

    // Set up GIC and restore its state
    setup_gic();
    let gic_state = std::fs::read(template_dir.join("gic.state")).expect("read gic.state");
    unsafe {
        check_hv(
            hvf::hv_gic_set_state(gic_state.as_ptr(), gic_state.len()),
            "hv_gic_set_state",
        );
    }

    // Create vCPU and restore CPU state
    let mut vcpu: u64 = 0;
    let mut exit_ptr: *const hvf::HvVcpuExit = ptr::null();
    unsafe {
        check_hv(hvf::hv_vcpu_create(&mut vcpu, &mut exit_ptr, ptr::null()), "hv_vcpu_create fork");
        check_hv(hvf::hv_vcpu_set_sys_reg(vcpu, hvf::HV_SYS_REG_MPIDR_EL1, 0x8000_0000), "set MPIDR");
        template.cpu_state.restore(vcpu);
        check_hv(hvf::hv_vcpu_set_vtimer_mask(vcpu, false), "unmask vtimer fork");
    }

    let uart = Pl011::new(dtb::UART_BASE);
    let mut vtimer = VirtualTimer::new();

    let watchdog = spawn_watchdog(vcpu, 10);

    // Run to completion
    let result = run_linux_vcpu_loop(vcpu, exit_ptr, mem, ram_size, &uart, &mut vtimer, None);
    let _ = watchdog.join();

    match result {
        VmExitReason::Exit(code) => {
            if code != 0 {
                eprintln!("VM exited with code {}", code);
            }
        }
        VmExitReason::SystemOff => {}
        VmExitReason::Ready => eprintln!("Unexpected HC_READY in forked VM"),
        VmExitReason::Error(e) => eprintln!("VM error: {}", e),
    }

    // Cleanup
    unsafe {
        check_hv(hvf::hv_vcpu_destroy(vcpu), "hv_vcpu_destroy");
        check_hv(hvf::hv_vm_destroy(), "hv_vm_destroy");
        libc::munmap(mem as *mut libc::c_void, ram_size);
        libc::munmap(mailbox_mem as *mut libc::c_void, LINUX_MAILBOX_SIZE);
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

/// Why the vCPU loop terminated.
pub enum VmExitReason {
    /// Guest called HC_READY (snapshot point)
    Ready,
    /// Guest called HC_EXIT with an exit code
    Exit(u64),
    /// PSCI SYSTEM_OFF or SYSTEM_RESET
    SystemOff,
    /// Too many exits or unexpected error
    Error(String),
}

fn run_linux_vcpu_loop(
    vcpu: u64,
    exit_ptr: *const hvf::HvVcpuExit,
    _guest_mem: *mut u8,
    _mem_size: usize,
    uart: &Pl011,
    vtimer: &mut VirtualTimer,
    mailbox_ptr: Option<*const u8>,
) -> VmExitReason {
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
            return VmExitReason::Error("too many exits".into());
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
                    // HVC (0x16) or SMC (0x17) from AArch64
                    0x16 | 0x17 => {
                        hvc_count += 1;
                        let is_smc = ec == 0x17;
                        let x0 = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_X0) };
                        let x1 = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_X1) };

                        // Check our hypercall IDs first
                        if x0 == convex_shared::HC_READY {
                            eprintln!("Guest signaled HC_READY (snapshot point)");
                            print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                            return VmExitReason::Ready;
                        } else if x0 == convex_shared::HC_EXIT {
                            let exit_code = x1;
                            print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                            return VmExitReason::Exit(exit_code);
                        } else if let Some(result) = psci::handle_psci(x0 as u32, x1) {
                            match result {
                                psci::PsciResult::Return(val) => unsafe {
                                    check_hv(
                                        hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_X0, val),
                                        "set x0 psci",
                                    );
                                },
                                psci::PsciResult::SystemOff => {
                                    eprintln!("\nPSCI SYSTEM_OFF");
                                    print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                                    return VmExitReason::SystemOff;
                                }
                                psci::PsciResult::SystemReset => {
                                    eprintln!("\nPSCI SYSTEM_RESET");
                                    print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                                    return VmExitReason::SystemOff;
                                }
                            }
                        } else {
                            // Unknown SMCCC call — return NOT_SUPPORTED (-1 as i32,
                            // sign-extended). Most SMCCC callers check for this.
                            static UNKNOWN_HVC_LOGGED: std::sync::atomic::AtomicU64 =
                                std::sync::atomic::AtomicU64::new(0);
                            let prev = UNKNOWN_HVC_LOGGED
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            if prev < 10 {
                                eprintln!("Unknown {}: x0=0x{:x}", if is_smc { "SMC" } else { "HVC" }, x0);
                            }
                            unsafe {
                                check_hv(
                                    hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_X0, (-1i32) as u64),
                                    "set x0 err",
                                );
                            }
                        }

                        // SMC traps don't auto-advance PC; HVC does
                        if is_smc {
                            unsafe {
                                let pc = hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC);
                                check_hv(
                                    hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_PC, pc + 4),
                                    "advance PC past SMC",
                                );
                            }
                        }
                    }

                    // Data abort from lower EL (MMIO)
                    0x24 => {
                        mmio_count += 1;
                        if let Some(access) = decode_data_abort(syndrome, ipa) {
                            handle_mmio(vcpu, &access, uart);
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
                        return VmExitReason::Error(format!("unexpected exception EC=0x{:x}", ec));
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
                // Forced exit from watchdog thread.
                // Check if init has written READY to the mailbox.
                if let Some(mbox) = mailbox_ptr {
                    let content = unsafe { std::slice::from_raw_parts(mbox, 12) };
                    if content == b"CONVEX_READY" {
                        eprintln!("Init signaled READY via mailbox");
                        print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                        return VmExitReason::Ready;
                    }
                }
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
                return VmExitReason::Error(format!("unexpected VM exit reason={}", other));
            }
        }
    }
}

fn handle_mmio(
    vcpu: u64,
    access: &MmioAccess,
    uart: &Pl011,
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
    // ISS encoding for MSR/MRS traps (EC=0x18):
    // [19:17]: Op0, [16:14]: Op2, [13:10]: CRn, [9:5]: Rt, [4:1]: CRm, [0]: direction
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
            // CNTV_TVAL_EL0 read: remaining ticks = cval - counter
            (3, 0, 1) => {
                let val = vtimer.read_cval().wrapping_sub(vtimer.counter) as i32;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a data abort syndrome value for testing.
    /// EC=0x24 is the exception class (already shifted out by caller).
    fn make_data_abort_syndrome(
        isv: bool,
        sas: u32,  // 0=byte, 1=hw, 2=word, 3=dw
        sse: bool,
        srt: u32,  // register index
        wnr: bool, // write=true
    ) -> u64 {
        let mut s: u64 = 0;
        if isv { s |= 1 << 24; }
        s |= ((sas as u64) & 3) << 22;
        if sse { s |= 1 << 21; }
        s |= ((srt as u64) & 0x1f) << 16;
        if wnr { s |= 1 << 6; }
        s
    }

    #[test]
    fn decode_word_write() {
        let syndrome = make_data_abort_syndrome(true, 2, false, 5, true);
        let access = decode_data_abort(syndrome, 0x0900_0000).unwrap();
        assert!(access.is_write);
        assert_eq!(access.len, 4);
        assert_eq!(access.reg, 5);
        assert_eq!(access.addr, 0x0900_0000);
    }

    #[test]
    fn decode_byte_read() {
        let syndrome = make_data_abort_syndrome(true, 0, false, 10, false);
        let access = decode_data_abort(syndrome, 0x0900_0018).unwrap();
        assert!(!access.is_write);
        assert_eq!(access.len, 1);
        assert_eq!(access.reg, 10);
    }

    #[test]
    fn decode_dword_access() {
        let syndrome = make_data_abort_syndrome(true, 3, false, 0, false);
        let access = decode_data_abort(syndrome, 0x1000).unwrap();
        assert_eq!(access.len, 8);
    }

    #[test]
    fn decode_without_isv_returns_none() {
        let syndrome = make_data_abort_syndrome(false, 2, false, 5, true);
        assert!(decode_data_abort(syndrome, 0x0900_0000).is_none());
    }
}
