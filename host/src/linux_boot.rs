//! Linux kernel boot support.
//!
//! Loads an ARM64 Linux kernel Image + optional initramfs into guest memory,
//! generates a DTB, sets up the GIC, and boots the kernel.

use std::path::Path;
use std::ptr;

extern "C" {
    fn mach_absolute_time() -> u64;
}

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
const GUEST_RAM_SIZE: u64 = 512 * 1024 * 1024; // 512 MiB

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

/// Create a VM with EL2 enabled (for timer trapping) if the platform supports it.
fn create_vm_with_el2() {
    unsafe {
        let mut el2_supported = false;
        check_hv(
            hvf::hv_vm_config_get_el2_supported(&mut el2_supported),
            "hv_vm_config_get_el2_supported",
        );

        if el2_supported {
            let config = hvf::hv_vm_config_create();
            assert!(!config.is_null(), "hv_vm_config_create returned null");
            check_hv(
                hvf::hv_vm_config_set_el2_enabled(config, true),
                "hv_vm_config_set_el2_enabled",
            );
            check_hv(hvf::hv_vm_create(config as *const _), "hv_vm_create (EL2)");
            eprintln!("VM created with EL2 enabled (timer trapping available)");
        } else {
            check_hv(hvf::hv_vm_create(ptr::null()), "hv_vm_create");
            eprintln!("VM created without EL2 (timer trapping not available)");
        }
    }
}

// ── Timer instruction patching ────────────────────────────────────────────────
//
// On M1 (no EL2), we can't trap CNTVCT_EL0 reads via CNTHCTL_EL2.
// Instead, we binary-patch the kernel image: replace every MRS/MSR to
// timer registers with an HVC instruction. Both are 4 bytes, and the
// HVC immediate encodes the operation type + target register.
//
// The HVC handler in the vCPU loop detects imm >= 0x100 and dispatches
// to vtimer.rs, which fully controls all timer state.

// HVC immediate encoding for patched timer instructions:
// 0x100 + Rt: read virtual/physical counter → Xrt
// 0x140 + Rt: read counter frequency → Xrt
// 0x180 + Rt: read CNTV_CTL → Xrt
// 0x1C0 + Rt: write Xrt → CNTV_CTL
// 0x200 + Rt: read CNTV_CVAL → Xrt
// 0x240 + Rt: write Xrt → CNTV_CVAL
// 0x280 + Rt: read CNTV_TVAL → Xrt
// 0x2C0 + Rt: write Xrt → CNTV_TVAL
const HVC_COUNTER_READ: u16 = 0x100;
const HVC_FREQ_READ: u16 = 0x140;
const HVC_CTL_READ: u16 = 0x180;
const HVC_CTL_WRITE: u16 = 0x1C0;
const HVC_CVAL_READ: u16 = 0x200;
const HVC_CVAL_WRITE: u16 = 0x240;
const HVC_TVAL_READ: u16 = 0x280;
const HVC_TVAL_WRITE: u16 = 0x2C0;

/// Encode an HVC #imm16 instruction.
fn encode_hvc(imm: u16) -> u32 {
    0xD400_0002 | ((imm as u32) << 5)
}

unsafe fn read_insn(ptr: *const u8, offset: usize) -> u32 {
    // ARM instructions in Image are always little-endian and 4-byte aligned.
    // We're on a LE host (aarch64-apple-darwin) so native read works.
    ptr::read(ptr.add(offset) as *const u32)
}

unsafe fn write_insn(ptr: *mut u8, offset: usize, insn: u32) {
    ptr::write(ptr.add(offset) as *mut u32, insn);
}

/// Patch all timer register accesses in the loaded kernel image.
/// Replaces MRS/MSR instructions with HVC calls so the hypervisor
/// fully controls the timer for deterministic execution.
fn patch_timer_reads(mem: *mut u8, kernel_offset: usize, kernel_file_size: usize) -> usize {
    let mut patched = 0;
    let kernel_start = unsafe { mem.add(kernel_offset) };

    for i in (0..kernel_file_size).step_by(4) {
        let insn = unsafe { read_insn(kernel_start, i) };
        let rt = insn & 0x1F;

        // Check for timer-related MRS/MSR instructions.
        // MRS (read): bit 21 = 1, base = 0xd53be000
        // MSR (write): bit 21 = 0, base = 0xd51be000
        let replacement = match insn & 0xFFFF_FFE0 {
            // Counter reads (MRS)
            0xd53b_e040 => encode_hvc(HVC_COUNTER_READ + rt as u16),  // CNTVCT_EL0
            0xd53b_e020 => encode_hvc(HVC_COUNTER_READ + rt as u16),  // CNTPCT_EL0
            0xd53b_e000 => encode_hvc(HVC_FREQ_READ + rt as u16),     // CNTFRQ_EL0
            // Virtual timer control (MRS reads)
            0xd53b_e320 => encode_hvc(HVC_CTL_READ + rt as u16),      // CNTV_CTL_EL0 read
            0xd53b_e340 => encode_hvc(HVC_CVAL_READ + rt as u16),     // CNTV_CVAL_EL0 read
            0xd53b_e300 => encode_hvc(HVC_TVAL_READ + rt as u16),     // CNTV_TVAL_EL0 read
            // Virtual timer control (MSR writes)
            0xd51b_e320 => encode_hvc(HVC_CTL_WRITE + rt as u16),     // CNTV_CTL_EL0 write
            0xd51b_e340 => encode_hvc(HVC_CVAL_WRITE + rt as u16),    // CNTV_CVAL_EL0 write
            0xd51b_e300 => encode_hvc(HVC_TVAL_WRITE + rt as u16),    // CNTV_TVAL_EL0 write
            _ => continue,
        };

        unsafe { write_insn(kernel_start as *mut u8, i, replacement); }
        patched += 1;
    }
    patched
}

/// Check if an HVC immediate is a patched timer read, and handle it.
/// Returns true if handled, false if this is a regular HVC.
fn handle_patched_timer_hvc(
    vcpu: u64,
    syndrome: u64,
    vtimer: &mut VirtualTimer,
) -> bool {
    let imm = (syndrome & 0xFFFF) as u16;
    if imm < HVC_COUNTER_READ {
        return false; // Not a patched timer HVC
    }
    let kind = imm & 0xFFC0; // top bits select operation
    let rt = (imm & 0x1F) as u32;

    match kind {
        0x100 => {
            // Counter read (CNTVCT / CNTPCT)
            let val = vtimer.read_counter();
            unsafe { check_hv(hvf::hv_vcpu_set_reg(vcpu, rt, val), "timer counter read"); }
            if vtimer.check_pending() {
                unsafe {
                    check_hv(
                        hvf::hv_vcpu_set_pending_interrupt(vcpu, hvf::HV_INTERRUPT_TYPE_IRQ, true),
                        "inject timer IRQ",
                    );
                }
            }
        }
        0x140 => {
            // Frequency read (CNTFRQ)
            unsafe {
                check_hv(hvf::hv_vcpu_set_reg(vcpu, rt, crate::vtimer::COUNTER_FREQ_HZ), "freq");
            }
        }
        0x180 => {
            // CNTV_CTL read
            let val = vtimer.read_ctl();
            unsafe { check_hv(hvf::hv_vcpu_set_reg(vcpu, rt, val), "ctl read"); }
        }
        0x1C0 => {
            // CNTV_CTL write
            let val = unsafe { hvf::vcpu_get_reg(vcpu, rt) };
            vtimer.write_ctl(val);
        }
        0x200 => {
            // CNTV_CVAL read
            let val = vtimer.read_cval();
            unsafe { check_hv(hvf::hv_vcpu_set_reg(vcpu, rt, val), "cval read"); }
        }
        0x240 => {
            // CNTV_CVAL write
            let val = unsafe { hvf::vcpu_get_reg(vcpu, rt) };
            vtimer.write_cval(val);
        }
        0x280 => {
            // CNTV_TVAL read
            // TVAL = CVAL - counter, sign-extended to 64 bits per ARM spec
            let val = vtimer.read_cval().wrapping_sub(vtimer.counter) as i32 as i64 as u64;
            unsafe { check_hv(hvf::hv_vcpu_set_reg(vcpu, rt, val), "tval read"); }
        }
        0x2C0 => {
            // CNTV_TVAL write
            let val = unsafe { hvf::vcpu_get_reg(vcpu, rt) };
            vtimer.write_tval(val);
        }
        _ => return false,
    }
    true
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
    // Binary-patch timer reads for deterministic time.
    // Disabled by default: each patched read becomes an HVC exit (~1-2μs),
    // making boot ~30x slower. Enable for determinism testing.
    // On M4+ or Linux/KVM, use CNTHCTL_EL2 trapping instead (zero overhead).
    let patched = if std::env::var("CONVEX_PATCH_TIMER").is_ok() {
        patch_timer_reads(mem, kernel_load_offset, kernel_data.len())
    } else {
        0
    };
    eprintln!(
        "Kernel loaded at GPA 0x{:x} (file={}, image_size={}, patched {} timer reads)",
        kernel_entry,
        kernel_data.len(),
        kernel_image_size,
        patched,
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
    create_vm_with_el2();

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
    create_vm_with_el2();
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

    // Save ICC (GIC CPU interface) registers
    let mut icc_data = Vec::new();
    for &reg_id in hvf::ICC_REGS {
        let mut val: u64 = 0;
        unsafe {
            check_hv(hvf::hv_gic_get_icc_reg(vcpu, reg_id, &mut val), "get ICC reg");
        }
        icc_data.extend_from_slice(&val.to_le_bytes());
    }
    std::fs::write(template_dir.join("icc.state"), &icc_data).expect("write icc.state");
    eprintln!("ICC state: {} bytes ({} registers)", icc_data.len(), hvf::ICC_REGS.len());

    // Save vtimer offset and current mach_absolute_time for timer continuity
    let vtimer_offset = unsafe {
        let mut offset: u64 = 0;
        check_hv(hvf::hv_vcpu_get_vtimer_offset(vcpu, &mut offset), "get vtimer offset");
        offset
    };
    let mach_time = unsafe { mach_absolute_time() };
    // The guest counter was: mach_time - vtimer_offset
    let guest_counter_at_snapshot = mach_time - vtimer_offset;
    let timer_meta = format!("vtimer_offset={}\nmach_time={}\nguest_counter={}\n",
        vtimer_offset, mach_time, guest_counter_at_snapshot);
    std::fs::write(template_dir.join("timer.meta"), &timer_meta).expect("write timer.meta");
    eprintln!("Timer: offset={} guest_counter={}", vtimer_offset, guest_counter_at_snapshot);

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
    create_vm_with_el2();
    unsafe {
        check_hv(hvf::hv_vm_map(mem, GUEST_RAM_BASE, ram_size,
            hvf::HV_MEMORY_READ | hvf::HV_MEMORY_WRITE | hvf::HV_MEMORY_EXEC), "hv_vm_map fork");
    }

    // Map mailbox at its own GPA
    unsafe {
        check_hv(hvf::hv_vm_map(mailbox_mem, LINUX_MAILBOX_GPA, LINUX_MAILBOX_SIZE,
            hvf::HV_MEMORY_READ | hvf::HV_MEMORY_WRITE), "hv_vm_map mailbox fork");
    }

    // Create GIC (must exist before vCPU and before set_state)
    setup_gic();

    // Create vCPU (must exist before GIC set_state per Apple docs)
    let mut vcpu: u64 = 0;
    let mut exit_ptr: *const hvf::HvVcpuExit = ptr::null();
    unsafe {
        check_hv(hvf::hv_vcpu_create(&mut vcpu, &mut exit_ptr, ptr::null()), "hv_vcpu_create fork");
        check_hv(hvf::hv_vcpu_set_sys_reg(vcpu, hvf::HV_SYS_REG_MPIDR_EL1, 0x8000_0000), "set MPIDR");
    }

    // Restore GIC device state AFTER gic_create + vcpu_create (Apple requirement)
    let gic_state_path = template_dir.join("gic.state");
    if gic_state_path.exists() {
        let gic_state = std::fs::read(&gic_state_path).expect("read gic.state");
        unsafe {
            check_hv(
                hvf::hv_gic_set_state(gic_state.as_ptr(), gic_state.len()),
                "hv_gic_set_state",
            );
        }
    }

    // Restore ICC (GIC CPU interface) registers
    let icc_path = template_dir.join("icc.state");
    if icc_path.exists() {
        let icc_data = std::fs::read(&icc_path).expect("read icc.state");
        let mut off = 0;
        for &reg_id in hvf::ICC_REGS {
            if off + 8 <= icc_data.len() {
                let val = u64::from_le_bytes(icc_data[off..off + 8].try_into().unwrap());
                unsafe {
                    check_hv(hvf::hv_gic_set_icc_reg(vcpu, reg_id, val), "set ICC reg");
                }
                off += 8;
            }
        }
    }

    unsafe {
        // Step 1: Set vtimer offset FIRST (so CNTVCT has correct base)
        let timer_meta_path = template_dir.join("timer.meta");
        if timer_meta_path.exists() {
            let meta = std::fs::read_to_string(&timer_meta_path).expect("read timer.meta");
            let mut guest_counter: u64 = 0;
            for line in meta.lines() {
                if let Some(v) = line.strip_prefix("guest_counter=") {
                    guest_counter = v.parse().unwrap_or(0);
                }
            }
            let now = mach_absolute_time();
            let new_offset = now - guest_counter;
            check_hv(hvf::hv_vcpu_set_vtimer_offset(vcpu, new_offset), "set vtimer offset");
        }

        // Step 2: Restore all CPU registers (including CNTV_CVAL, CNTV_CTL, CNTP_*)
        template.cpu_state.restore(vcpu);

        // Step 3: Unmask vtimer
        check_hv(hvf::hv_vcpu_set_vtimer_mask(vcpu, false), "unmask vtimer fork");
    }

    let uart = Pl011::new(dtb::UART_BASE);
    let mut vtimer = VirtualTimer::new();

    // Watchdog forces periodic VM exits. On each exit we unmask the vtimer
    // and ensure a timer interrupt fires, keeping the kernel scheduler alive.
    // V8 init can take 10+ seconds with thread creation.
    let watchdog = spawn_watchdog(vcpu, 300);

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

        // Try to set CNTHCTL_EL2 to trap timer register accesses for
        // deterministic virtual time. If this fails (HVF doesn't support it
        // without EL2 enabled), fall back to non-deterministic real-time.
        //
        // CNTHCTL_EL2 bits:
        //   bit 0 (EL1PCTEN): 0 = trap physical counter reads at EL0/EL1
        //   bit 1 (EL1PCEN): 0 = trap physical timer control at EL0/EL1
        // Setting to 0 traps all physical timer/counter accesses.
        let cnthctl_ret = hvf::hv_vcpu_set_sys_reg(
            vcpu,
            hvf::HV_SYS_REG_CNTHCTL_EL2,
            0, // trap everything
        );
        if cnthctl_ret == hvf::HV_SUCCESS {
            eprintln!("Timer trapping enabled via CNTHCTL_EL2");
        } else {
            eprintln!(
                "CNTHCTL_EL2 not available (ret=0x{:x}), using real-time timer",
                cnthctl_ret as u32
            );
        }

        // Don't mask the vtimer — let HVF deliver vtimer interrupts
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
    let mut _sysreg_count: u64 = 0;
    let mut canceled_count: u64 = 0;
    let start_time = std::time::Instant::now();
    let mut last_log = start_time;

    loop {
        // Log every 2 seconds of wall time OR every 100K exits
        let now = std::time::Instant::now();
        let should_log = (exit_count > 0 && exit_count % 100_000 == 0)
            || (now.duration_since(last_log).as_secs() >= 2 && exit_count > 0);
        if should_log {
            let pc = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC) };
            let cpsr = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_CPSR) };
            let el = (cpsr >> 2) & 3;
            let sp = unsafe { hvf::vcpu_get_sys_reg(vcpu, hvf::HV_SYS_REG_SP_EL1) };
            let elapsed = now.duration_since(start_time);
            eprintln!(
                "[{:.1}s, {} exits] EL{} PC=0x{:x} SP=0x{:x} mmio={} hvc={} timer={} canceled={}",
                elapsed.as_secs_f64(), exit_count, el, pc, sp,
                mmio_count, hvc_count, timer_count, canceled_count,
            );
            last_log = now;
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
        // QEMU-style hvf_sync_vtimer: if vtimer was masked (after a
        // VTIMER_ACTIVATED exit), check if the guest EOI'd the timer
        // interrupt (CNTV_CTL.ISTATUS cleared). If so, unmask.
        unsafe {
            let ctl = hvf::vcpu_get_sys_reg(vcpu, hvf::HV_SYS_REG_CNTV_CTL_EL0);
            let enabled = (ctl & 1) != 0;
            let imask = (ctl & 2) != 0;
            let istatus = (ctl & 4) != 0;
            // Unmask if: timer not asserting (ISTATUS cleared or masked)
            if !(enabled && !imask && istatus) {
                let _ = hvf::hv_vcpu_set_vtimer_mask(vcpu, false);
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

                        // Check for patched timer reads first (HVC #0x100+)
                        if handle_patched_timer_hvc(vcpu, syndrome, vtimer) {
                            // Timer read handled — continue execution
                        } else if x0 == convex_shared::HC_READY {
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
                        _sysreg_count += 1;
                        handle_sys_reg_trap(vcpu, syndrome);
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
                canceled_count += 1;
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
                // Force a timer interrupt for the kernel scheduler.
                // The vtimer PPI (27) is how the kernel receives timer ticks.
                // We can't directly inject a PPI via hv_gic_set_spi (that's for
                // shared peripheral interrupts). Instead, use the pending
                // interrupt mechanism + unmask vtimer to trigger a tick.
                unsafe {
                    // Set an expired timer deadline and unmask
                    check_hv(
                        hvf::hv_vcpu_set_sys_reg(vcpu, hvf::HV_SYS_REG_CNTV_CVAL_EL0, 0),
                        "set CNTV_CVAL expired",
                    );
                    check_hv(
                        hvf::hv_vcpu_set_sys_reg(vcpu, hvf::HV_SYS_REG_CNTV_CTL_EL0, 1), // enable, unmask
                        "set CNTV_CTL enabled",
                    );
                    check_hv(hvf::hv_vcpu_set_vtimer_mask(vcpu, false), "unmask vtimer");
                    // Also directly pend an IRQ — belt and suspenders
                    check_hv(
                        hvf::hv_vcpu_set_pending_interrupt(vcpu, hvf::HV_INTERRUPT_TYPE_IRQ, true),
                        "pend IRQ",
                    );
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
/// Timer registers are handled via binary patching (HVC), so this only
/// handles non-timer sysreg traps.
fn handle_sys_reg_trap(vcpu: u64, syndrome: u64) {
    let direction = syndrome & 1; // 1 = read (MRS), 0 = write (MSR)
    let rt = ((syndrome >> 5) & 0x1f) as u32;
    let pc = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_PC) };

    eprintln!(
        "Trapped sys reg: ISS=0x{:x} dir={} Rt=x{} PC=0x{:x}",
        syndrome & 0x1FFFFF, direction, rt, pc
    );

    // Return 0 for reads of unknown sys regs
    if direction == 1 {
        unsafe {
            check_hv(hvf::hv_vcpu_set_reg(vcpu, rt, 0), "set reg for unknown sysreg");
        }
    }

    // Advance PC past the trapped instruction
    unsafe {
        check_hv(hvf::hv_vcpu_set_reg(vcpu, hvf::HV_REG_PC, pc + 4), "advance PC past sysreg");
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

    #[test]
    fn encode_hvc_encoding() {
        // HVC #0 = 0xD4000002
        assert_eq!(encode_hvc(0), 0xD4000002);
        // HVC #0x100 = 0xD4002002 (0x100 << 5 = 0x2000)
        assert_eq!(encode_hvc(0x100), 0xD4000002 | (0x100 << 5));
        // HVC #0x105 = counter read into X5
        let insn = encode_hvc(HVC_COUNTER_READ + 5);
        assert_eq!(insn & 0xFFE0001F, 0xD4000002); // HVC base
        assert_eq!((insn >> 5) & 0xFFFF, (HVC_COUNTER_READ + 5) as u32); // imm
    }

    #[test]
    fn patch_timer_reads_replaces_cntvct() {
        // Create a small buffer with a MRS X0, CNTVCT_EL0 instruction
        let mut buf = vec![0u8; 16];
        let mrs_x0_cntvct: u32 = 0xd53be040; // MRS X0, CNTVCT_EL0
        let nop: u32 = 0xd503201f;
        unsafe {
            ptr::write(buf.as_mut_ptr() as *mut u32, mrs_x0_cntvct);
            ptr::write(buf.as_mut_ptr().add(4) as *mut u32, nop);
            ptr::write(buf.as_mut_ptr().add(8) as *mut u32, 0xd53be041); // MRS X1, CNTVCT
            ptr::write(buf.as_mut_ptr().add(12) as *mut u32, nop);
        }

        let count = patch_timer_reads(buf.as_mut_ptr(), 0, 16);
        assert_eq!(count, 2); // two CNTVCT reads patched

        // Verify the first instruction was replaced with HVC #0x100
        let patched0 = unsafe { ptr::read(buf.as_ptr() as *const u32) };
        assert_eq!(patched0, encode_hvc(HVC_COUNTER_READ + 0)); // X0

        // Second should be HVC #0x101 (X1)
        let patched1 = unsafe { ptr::read(buf.as_ptr().add(8) as *const u32) };
        assert_eq!(patched1, encode_hvc(HVC_COUNTER_READ + 1)); // X1

        // NOP instructions should be unchanged
        let nop0 = unsafe { ptr::read(buf.as_ptr().add(4) as *const u32) };
        assert_eq!(nop0, nop);
    }

    #[test]
    fn patch_timer_reads_handles_msr_writes() {
        let mut buf = vec![0u8; 8];
        let msr_x2_cval: u32 = 0xd51be342; // MSR CNTV_CVAL_EL0, X2
        unsafe {
            ptr::write(buf.as_mut_ptr() as *mut u32, msr_x2_cval);
            ptr::write(buf.as_mut_ptr().add(4) as *mut u32, 0xd503201f); // NOP
        }

        let count = patch_timer_reads(buf.as_mut_ptr(), 0, 8);
        assert_eq!(count, 1);

        let patched = unsafe { ptr::read(buf.as_ptr() as *const u32) };
        assert_eq!(patched, encode_hvc(HVC_CVAL_WRITE + 2)); // write X2 to CVAL
    }
}
