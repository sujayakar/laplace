//! Linux kernel boot support.
//!
//! Loads an ARM64 Linux kernel Image + optional initramfs into guest memory,
//! generates a DTB, sets up the GIC, and boots the kernel.

use std::path::Path;
use std::ptr;

use crate::dtb;
use crate::hypervisor::{self, MmioAccess, SysReg, VcpuExit, VcpuHandle, VmHandle};
use crate::pl011::Pl011;
use crate::psci;
use crate::vtimer::VirtualTimer;

use crate::{alloc_pages, page_align};

/// Sentinel written by init to the inbox when the VM is ready for snapshot.
/// Must match init/src/main.rs. Used in the Canceled exit handler during snapshot.
const CONVEX_READY: &[u8] = b"CONVEX_READY";

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
    /// Kernel file size in bytes (used for timer patching).
    kernel_file_size: usize,
}

// ── Timer instruction patching ────────────────────────────────────────────────
//
// When CNTHCTL_EL2 is unavailable (pKVM, M1 HVF without EL2), we binary-patch
// the kernel image to replace timer MRS/MSR instructions with trap instructions.
//
// Two patching modes:
// - **HVC mode** (HVF): Replace with HVC #imm. HVF forwards all HVCs to
//   userspace where our vtimer handles them.
// - **MMIO mode** (KVM): Replace with LDR/STR Xt, [X18, #offset]. KVM exits
//   to userspace on MMIO access. X18 (platform register) is pre-loaded with
//   the MMIO timer device base address before the VM resumes.
//
// Both modes are 4 bytes per instruction, preserving code layout.

// ── HVC patching (HVF) ──

// HVC immediate encoding for patched timer instructions:
#[cfg(target_os = "macos")]
const HVC_COUNTER_READ: u16 = 0x100;
#[cfg(target_os = "macos")]
const HVC_FREQ_READ: u16 = 0x140;
#[cfg(target_os = "macos")]
const HVC_CTL_READ: u16 = 0x180;
#[cfg(target_os = "macos")]
const HVC_CTL_WRITE: u16 = 0x1C0;
#[cfg(target_os = "macos")]
const HVC_CVAL_READ: u16 = 0x200;
#[cfg(target_os = "macos")]
const HVC_CVAL_WRITE: u16 = 0x240;
#[cfg(target_os = "macos")]
const HVC_TVAL_READ: u16 = 0x280;
#[cfg(target_os = "macos")]
const HVC_TVAL_WRITE: u16 = 0x2C0;

#[cfg(target_os = "macos")]
fn encode_hvc(imm: u16) -> u32 {
    0xD400_0002 | ((imm as u32) << 5)
}

// ── BRK patching (KVM) ──
//
// On KVM where CNTHCTL_EL2 is unavailable (pKVM), we replace timer instructions
// with BRK #imm16. With KVM_SET_GUEST_DEBUG enabled, BRK exits to userspace
// as KVM_EXIT_DEBUG. The immediate encodes operation type + register, same
// scheme as HVC patching on HVF.
//
// BRK #imm16 encoding: 0xD4200000 | (imm16 << 5)

/// Encode BRK #imm16 instruction.
fn encode_brk(imm: u16) -> u32 {
    0xD420_0000 | ((imm as u32) << 5)
}

// BRK immediate encoding. Uses 0xE1xx-0xE2xx range to avoid collision with
// kernel-reserved BRK immediates (BUG=0x800, KASAN=0x9xx, FAULT=0x100, etc.).
const BRK_COUNTER_READ: u16 = 0xE100;
const BRK_FREQ_READ: u16 = 0xE140;
const BRK_CTL_READ: u16 = 0xE180;
const BRK_CTL_WRITE: u16 = 0xE1C0;
const BRK_CVAL_READ: u16 = 0xE200;
const BRK_CVAL_WRITE: u16 = 0xE240;
const BRK_TVAL_READ: u16 = 0xE280;
const BRK_TVAL_WRITE: u16 = 0xE2C0;

// ── Common patching infrastructure ──

unsafe fn read_insn(ptr: *const u8, offset: usize) -> u32 {
    ptr::read(ptr.add(offset) as *const u32)
}

unsafe fn write_insn(ptr: *mut u8, offset: usize, insn: u32) {
    ptr::write(ptr.add(offset) as *mut u32, insn);
}

/// Which trap mechanism to use for timer patching.
enum TimerPatchMode {
    /// Replace with HVC #imm (HVF — exits to userspace).
    #[cfg(target_os = "macos")]
    Hvc,
    /// Replace with BRK #imm16 (KVM — debug exit with KVM_SET_GUEST_DEBUG).
    Brk,
}

/// Patch all timer register accesses in the loaded kernel image.
fn patch_timer_reads(
    mem: *mut u8,
    kernel_offset: usize,
    kernel_file_size: usize,
    mode: &TimerPatchMode,
) -> usize {
    let mut patched = 0;
    let kernel_start = unsafe { mem.add(kernel_offset) };

    for i in (0..kernel_file_size).step_by(4) {
        let insn = unsafe { read_insn(kernel_start, i) };
        let rt = insn & 0x1F;

        let replacement = match insn & 0xFFFF_FFE0 {
            // Counter reads (MRS)
            0xd53b_e040 | // CNTVCT_EL0
            0xd53b_e020   // CNTPCT_EL0
            => match mode {
                #[cfg(target_os = "macos")]
                TimerPatchMode::Hvc => encode_hvc(HVC_COUNTER_READ + rt as u16),
                TimerPatchMode::Brk => encode_brk(BRK_COUNTER_READ + rt as u16),
            },
            0xd53b_e000 => match mode { // CNTFRQ_EL0
                #[cfg(target_os = "macos")]
                TimerPatchMode::Hvc => encode_hvc(HVC_FREQ_READ + rt as u16),
                TimerPatchMode::Brk => encode_brk(BRK_FREQ_READ + rt as u16),
            },
            // Virtual timer control (MRS reads)
            0xd53b_e320 => match mode { // CNTV_CTL_EL0 read
                #[cfg(target_os = "macos")]
                TimerPatchMode::Hvc => encode_hvc(HVC_CTL_READ + rt as u16),
                TimerPatchMode::Brk => encode_brk(BRK_CTL_READ + rt as u16),
            },
            0xd53b_e340 => match mode { // CNTV_CVAL_EL0 read
                #[cfg(target_os = "macos")]
                TimerPatchMode::Hvc => encode_hvc(HVC_CVAL_READ + rt as u16),
                TimerPatchMode::Brk => encode_brk(BRK_CVAL_READ + rt as u16),
            },
            0xd53b_e300 => match mode { // CNTV_TVAL_EL0 read
                #[cfg(target_os = "macos")]
                TimerPatchMode::Hvc => encode_hvc(HVC_TVAL_READ + rt as u16),
                TimerPatchMode::Brk => encode_brk(BRK_TVAL_READ + rt as u16),
            },
            // Virtual timer control (MSR writes)
            0xd51b_e320 => match mode { // CNTV_CTL_EL0 write
                #[cfg(target_os = "macos")]
                TimerPatchMode::Hvc => encode_hvc(HVC_CTL_WRITE + rt as u16),
                TimerPatchMode::Brk => encode_brk(BRK_CTL_WRITE + rt as u16),
            },
            0xd51b_e340 => match mode { // CNTV_CVAL_EL0 write
                #[cfg(target_os = "macos")]
                TimerPatchMode::Hvc => encode_hvc(HVC_CVAL_WRITE + rt as u16),
                TimerPatchMode::Brk => encode_brk(BRK_CVAL_WRITE + rt as u16),
            },
            0xd51b_e300 => match mode { // CNTV_TVAL_EL0 write
                #[cfg(target_os = "macos")]
                TimerPatchMode::Hvc => encode_hvc(HVC_TVAL_WRITE + rt as u16),
                TimerPatchMode::Brk => encode_brk(BRK_TVAL_WRITE + rt as u16),
            },
            _ => continue,
        };

        unsafe {
            write_insn(kernel_start as *mut u8, i, replacement);
        }
        patched += 1;
    }
    patched
}

/// Handle a BRK debug exit for patched timer instructions (KVM).
/// The BRK immediate encodes the operation type + register, same as HVC.
/// Returns true if this was a patched timer BRK.
/// Handle a BRK debug exit for patched timer instructions (KVM).
/// The HSR (exception syndrome register) contains the BRK immediate in bits 15:0.
/// The immediate encodes the operation type + register, same scheme as HVC.
/// Returns true if this was a patched timer BRK.
fn handle_patched_timer_brk(vcpu: &VcpuHandle, hsr: u32, vtimer: &mut VirtualTimer) -> bool {
    // ESR for BRK: EC=0x3C (bits 31:26), ISS = imm16 (bits 15:0)
    let imm = (hsr & 0xFFFF) as u16;
    if imm < BRK_COUNTER_READ {
        return false;
    }

    let kind = imm & 0xFFC0; // upper bits select operation
    let rt = (imm & 0x1F) as u32;

    match kind {
        0xE100 => {
            let val = vtimer.read_counter();
            vcpu.set_reg(rt, val);
            if vtimer.check_pending() {
                vcpu.set_pending_interrupt(true);
            }
        }
        0xE140 => {
            vcpu.set_reg(rt, crate::vtimer::COUNTER_FREQ_HZ);
        }
        0xE180 => {
            vcpu.set_reg(rt, vtimer.read_ctl());
        }
        0xE1C0 => {
            vtimer.write_ctl(vcpu.get_reg(rt));
        }
        0xE200 => {
            vcpu.set_reg(rt, vtimer.read_cval());
        }
        0xE240 => {
            vtimer.write_cval(vcpu.get_reg(rt));
        }
        0xE280 => {
            let val = vtimer.read_cval().wrapping_sub(vtimer.counter) as i32 as i64 as u64;
            vcpu.set_reg(rt, val);
        }
        0xE2C0 => {
            vtimer.write_tval(vcpu.get_reg(rt));
        }
        _ => return false,
    }

    // Advance PC past the BRK (KVM_EXIT_DEBUG doesn't auto-advance)
    let pc = vcpu.get_reg(hypervisor::REG_PC);
    vcpu.set_reg(hypervisor::REG_PC, pc + 4);
    true
}

/// Check if an HVC immediate is a patched timer read, and handle it.
/// Only used on HVF where timer instructions are patched to HVC.
#[cfg(target_os = "macos")]
fn handle_patched_timer_hvc(vcpu: &VcpuHandle, syndrome: u64, vtimer: &mut VirtualTimer) -> bool {
    let imm = (syndrome & 0xFFFF) as u16;
    if imm < HVC_COUNTER_READ {
        return false;
    }
    let kind = imm & 0xFFC0;
    let rt = (imm & 0x1F) as u32;

    match kind {
        0x100 => {
            let val = vtimer.read_counter();
            vcpu.set_reg(rt, val);
            if vtimer.check_pending() {
                vcpu.set_pending_interrupt(true);
            }
        }
        0x140 => {
            vcpu.set_reg(rt, crate::vtimer::COUNTER_FREQ_HZ);
        }
        0x180 => {
            let val = vtimer.read_ctl();
            vcpu.set_reg(rt, val);
        }
        0x1C0 => {
            let val = vcpu.get_reg(rt);
            vtimer.write_ctl(val);
        }
        0x200 => {
            let val = vtimer.read_cval();
            vcpu.set_reg(rt, val);
        }
        0x240 => {
            let val = vcpu.get_reg(rt);
            vtimer.write_cval(val);
        }
        0x280 => {
            let val = vtimer.read_cval().wrapping_sub(vtimer.counter) as i32 as i64 as u64;
            vcpu.set_reg(rt, val);
        }
        0x2C0 => {
            let val = vcpu.get_reg(rt);
            vtimer.write_tval(val);
        }
        _ => return false,
    }
    true
}

/// Load kernel image, optional initrd, and generated DTB into a freshly
/// allocated guest memory region.
fn load_kernel_and_initrd(
    kernel_path: &Path,
    initrd_path: Option<&Path>,
    quiet: bool,
) -> LoadedKernel {
    let kernel_data =
        std::fs::read(kernel_path).unwrap_or_else(|e| panic!("Failed to read kernel: {}", e));
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

    let kernel_image_size = if kernel_data.len() >= 0x18 {
        u64::from_le_bytes(kernel_data[0x10..0x18].try_into().unwrap()) as usize
    } else {
        kernel_data.len()
    };

    let kernel_file_size = kernel_data.len();
    eprintln!(
        "Kernel loaded at GPA 0x{:x} (file={}, image_size={})",
        kernel_entry, kernel_file_size, kernel_image_size,
    );

    // Load initrd after kernel image_size (page-aligned)
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
    let dtb_data = dtb::build_dtb(
        GUEST_RAM_BASE,
        GUEST_RAM_SIZE,
        initrd_start,
        initrd_end,
        quiet,
    );
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
        kernel_file_size,
    }
}

/// Install a no-op SIGUSR1 handler so we can use it to interrupt KVM_RUN.
/// Must be called before spawning any watchdog threads.
#[cfg(target_os = "linux")]
fn install_sigusr1_handler() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = noop_signal_handler as *const () as usize;
            sa.sa_flags = 0; // no SA_RESTART: let KVM_RUN return EINTR
            libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
        }
    });
}

/// Spawn a watchdog thread that periodically forces VM exits so we can
/// check for the READY sentinel during snapshot boot.
/// On KVM, sends a signal to the vCPU thread to cause KVM_RUN to return EINTR.
/// On HVF, calls hv_vcpus_exit.
fn spawn_watchdog(
    _vcpu: &VcpuHandle,
    duration_secs: u32,
) -> (
    std::thread::JoinHandle<()>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop.clone();
    let iterations = (duration_secs as u64) * 10;

    #[cfg(target_os = "linux")]
    install_sigusr1_handler();
    #[cfg(target_os = "linux")]
    let vcpu_tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };

    let handle = std::thread::spawn(move || {
        for _ in 0..iterations {
            if stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            #[cfg(target_os = "linux")]
            unsafe {
                libc::syscall(libc::SYS_tgkill, libc::getpid(), vcpu_tid, libc::SIGUSR1);
            }
            // On macOS, hv_vcpus_exit would be called here via vcpu.force_exit(),
            // but we don't have a Send reference to the vcpu from another thread.
            // The HVF backend's original code used the raw vcpu handle directly.
            // TODO: Make force_exit thread-safe on HVF.
        }
    });
    (handle, stop)
}

#[cfg(target_os = "linux")]
extern "C" fn noop_signal_handler(_sig: libc::c_int) {}

/// Like spawn_watchdog but with 1ms sleep intervals for low-latency forks.
fn spawn_watchdog_fast(
    _vcpu: &VcpuHandle,
    duration_secs: u32,
) -> (
    std::thread::JoinHandle<()>,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    #[cfg(target_os = "linux")]
    install_sigusr1_handler();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_clone = stop.clone();
    let iterations = (duration_secs as u64) * 1000;
    #[cfg(target_os = "linux")]
    let vcpu_tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };

    let handle = std::thread::spawn(move || {
        for _ in 0..iterations {
            if stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
            #[cfg(target_os = "linux")]
            unsafe {
                libc::syscall(libc::SYS_tgkill, libc::getpid(), vcpu_tid, libc::SIGUSR1);
            }
        }
    });
    (handle, stop)
}

/// Shared region at a fixed GPA outside the RAM region.
#[allow(dead_code)]
pub const LINUX_INBOX_GPA: u64 = 0x3F00_0000;
pub const LINUX_INBOX_SIZE: usize = 8 * 1024 * 1024;
#[allow(dead_code)]
pub const LINUX_OUTBOX_GPA: u64 = 0x3F80_0000;
#[allow(dead_code)]
pub const LINUX_OUTBOX_SIZE: usize = 8 * 1024 * 1024;
pub const LINUX_SHARED_GPA: u64 = 0x3F00_0000;
pub const LINUX_SHARED_SIZE: usize = 16 * 1024 * 1024;

/// Allocate and map the shared region (inbox + outbox) into the VM.
fn setup_shared_region(vm: &mut VmHandle) -> *mut u8 {
    let shared_mem = alloc_pages(LINUX_SHARED_SIZE);
    vm.map_memory(shared_mem, LINUX_SHARED_GPA, LINUX_SHARED_SIZE, false);
    shared_mem
}

// Note: No MMIO hole needed for BRK-based timer patching — BRK traps via
// KVM_EXIT_DEBUG regardless of memory mapping.

/// Set up CPU state for Linux boot. Returns true if hardware timer
/// trapping is available (CNTHCTL_EL2), false if binary patching is needed.
fn setup_cpu_for_linux(vcpu: &VcpuHandle, kernel_entry: u64, dtb_addr: u64) -> bool {
    // PC = kernel entry point
    vcpu.set_reg(hypervisor::REG_PC, kernel_entry);

    // CPSR = EL1h with all interrupts masked
    vcpu.set_reg(hypervisor::REG_CPSR, 0x3c5);

    // x0 = DTB address (Linux boot protocol)
    vcpu.set_reg(hypervisor::REG_X0, dtb_addr);

    // x1, x2, x3 = 0 (reserved)
    vcpu.set_reg(hypervisor::REG_X1, 0);
    vcpu.set_reg(hypervisor::REG_X2, 0);
    vcpu.set_reg(hypervisor::REG_X3, 0);

    // SCTLR_EL1: MMU off, caches off
    vcpu.set_sys_reg(SysReg::SCTLR_EL1, 0x30d00800);

    // Enable SIMD/FP at EL1: CPACR_EL1.FPEN = 0b11
    vcpu.set_sys_reg(SysReg::CPACR_EL1, 3 << 20);

    // Set MPIDR_EL1 for CPU 0
    vcpu.set_sys_reg(SysReg::MPIDR_EL1, 0x8000_0000);

    // Try to set CNTHCTL_EL2 to trap timer register accesses.
    // On KVM with full EL2 support, this enables deterministic timer.
    // On pKVM (Asahi Linux), this register is not writable — fall back
    // to binary patching via CONVEX_PATCH_TIMER env var.
    let has_timer_trapping = match vcpu.try_set_sys_reg(SysReg::CNTHCTL_EL2, 0) {
        Ok(()) => {
            eprintln!("Timer trapping enabled via CNTHCTL_EL2");
            true
        }
        Err(e) => {
            eprintln!(
                "CNTHCTL_EL2 not available ({}) — will use binary patching for determinism",
                e
            );
            false
        }
    };

    // KVM manages the vtimer directly — no manual mask/unmask needed
    vcpu.set_vtimer_mask(false);
    has_timer_trapping
}

/// Why the vCPU loop terminated.
pub enum VmExitReason {
    Ready,
    Exit(u64),
    SystemOff,
    Error(String),
}

fn run_linux_vcpu_loop(
    vcpu: &mut VcpuHandle,
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
    let mut canceled_count: u64 = 0;
    let start_time = std::time::Instant::now();
    let mut last_log = start_time;

    loop {
        let now = std::time::Instant::now();
        let should_log = (exit_count > 0 && exit_count % 100_000 == 0)
            || (now.duration_since(last_log).as_secs() >= 2 && exit_count > 0);
        if should_log {
            let pc = vcpu.get_reg(hypervisor::REG_PC);
            let cpsr = vcpu.get_reg(hypervisor::REG_CPSR);
            let el = (cpsr >> 2) & 3;
            let elapsed = now.duration_since(start_time);
            eprintln!(
                "[{:.1}s, {} exits] EL{} PC=0x{:x} mmio={} hvc={} timer={} canceled={}",
                elapsed.as_secs_f64(),
                exit_count,
                el,
                pc,
                mmio_count,
                hvc_count,
                timer_count,
                canceled_count,
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
            vcpu.set_pending_interrupt(true);
        }

        let exit = vcpu.run();
        exit_count += 1;

        match exit {
            VcpuExit::Hvc { syndrome, is_smc } => {
                let _ = syndrome; // used on macOS for HVC timer patching
                hvc_count += 1;
                let x0 = vcpu.get_reg(hypervisor::REG_X0);
                let x1 = vcpu.get_reg(hypervisor::REG_X1);

                // On HVF, check for patched timer HVCs (imm >= 0x100)
                #[cfg(target_os = "macos")]
                let timer_handled = handle_patched_timer_hvc(vcpu, syndrome, vtimer);
                #[cfg(target_os = "linux")]
                let timer_handled = false; // KVM uses MMIO patching, not HVC

                if timer_handled {
                    // Timer read handled via HVC patching
                } else if x0 == convex_shared::HC_READY {
                    eprintln!("Guest signaled HC_READY (snapshot point)");
                    print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                    return VmExitReason::Ready;
                } else if x0 == convex_shared::HC_EXIT {
                    print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                    return VmExitReason::Exit(x1);
                } else if let Some(result) = psci::handle_psci(x0 as u32, x1) {
                    match result {
                        psci::PsciResult::Return(val) => {
                            vcpu.set_reg(hypervisor::REG_X0, val);
                        }
                        psci::PsciResult::SystemOff => {
                            eprintln!("\nPSCI SYSTEM_OFF");
                            print_exit_stats(
                                exit_count,
                                mmio_count,
                                hvc_count,
                                timer_count,
                                wfi_count,
                            );
                            return VmExitReason::SystemOff;
                        }
                        psci::PsciResult::SystemReset => {
                            eprintln!("\nPSCI SYSTEM_RESET");
                            print_exit_stats(
                                exit_count,
                                mmio_count,
                                hvc_count,
                                timer_count,
                                wfi_count,
                            );
                            return VmExitReason::SystemOff;
                        }
                    }
                } else {
                    static UNKNOWN_HVC_LOGGED: std::sync::atomic::AtomicU64 =
                        std::sync::atomic::AtomicU64::new(0);
                    let prev =
                        UNKNOWN_HVC_LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if prev < 10 {
                        eprintln!(
                            "Unknown {}: x0=0x{:x}",
                            if is_smc { "SMC" } else { "HVC" },
                            x0
                        );
                    }
                    vcpu.set_reg(hypervisor::REG_X0, (-1i32) as u64);
                }

                // SMC traps don't auto-advance PC on HVF; KVM handles this in-kernel.
                // On KVM, both HVC and SMC auto-advance PC.
                #[cfg(target_os = "macos")]
                if is_smc {
                    let pc = vcpu.get_reg(hypervisor::REG_PC);
                    vcpu.set_reg(hypervisor::REG_PC, pc + 4);
                }
            }

            VcpuExit::Mmio(access) => {
                mmio_count += 1;
                handle_mmio(vcpu, &access, uart);
            }

            VcpuExit::Debug { hsr } => {
                // BRK instruction from patched timer code (KVM with guest debug).
                // The HSR contains the BRK immediate which encodes the timer op.
                if handle_patched_timer_brk(vcpu, hsr, vtimer) {
                    timer_count += 1;
                } else {
                    let pc = vcpu.get_reg(hypervisor::REG_PC);
                    eprintln!("Unexpected debug exit at PC=0x{:x} HSR=0x{:x}", pc, hsr);
                    vcpu.set_reg(hypervisor::REG_PC, pc + 4);
                }
            }

            VcpuExit::UndecodableMmio { syndrome, ipa } => {
                let pc = vcpu.get_reg(hypervisor::REG_PC);
                eprintln!(
                    "Undecodable data abort: syndrome=0x{:x} IPA=0x{:x} PC=0x{:x}",
                    syndrome, ipa, pc
                );
                // Skip the faulting instruction
                vcpu.set_reg(hypervisor::REG_PC, pc + 4);
            }

            VcpuExit::Wfi => {
                wfi_count += 1;
                let inject = vtimer.handle_wfi();
                if inject {
                    timer_count += 1;
                    vcpu.set_pending_interrupt(true);
                }
            }

            VcpuExit::VtimerActivated => {
                // HVF-only: vtimer fired, inject IRQ
                timer_count += 1;
                vcpu.set_vtimer_mask(true);
                vcpu.set_pending_interrupt(true);
            }

            VcpuExit::Canceled => {
                canceled_count += 1;
                // Check if init has written READY to the inbox
                if let Some(mbox) = mailbox_ptr {
                    let content = unsafe { std::slice::from_raw_parts(mbox, CONVEX_READY.len()) };
                    if content == CONVEX_READY {
                        eprintln!("Init signaled READY via inbox");
                        print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                        return VmExitReason::Ready;
                    }
                }
            }

            VcpuExit::SysRegTrap { syndrome } => {
                handle_sys_reg_trap(vcpu, syndrome);
            }

            VcpuExit::SystemEvent { event_type } => {
                // KVM system events (shutdown, reset)
                const KVM_SYSTEM_EVENT_SHUTDOWN: u32 = 1;
                const KVM_SYSTEM_EVENT_RESET: u32 = 2;
                match event_type {
                    KVM_SYSTEM_EVENT_SHUTDOWN => {
                        eprintln!("\nKVM SYSTEM_EVENT_SHUTDOWN");
                        print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                        return VmExitReason::SystemOff;
                    }
                    KVM_SYSTEM_EVENT_RESET => {
                        eprintln!("\nKVM SYSTEM_EVENT_RESET");
                        print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                        return VmExitReason::SystemOff;
                    }
                    other => {
                        eprintln!("Unknown KVM system event: {}", other);
                    }
                }
            }

            VcpuExit::Unknown(reason) => {
                let pc = vcpu.get_reg(hypervisor::REG_PC);
                eprintln!("Unexpected VM exit: reason={} PC=0x{:x}", reason, pc);
                print_exit_stats(exit_count, mmio_count, hvc_count, timer_count, wfi_count);
                return VmExitReason::Error(format!("unexpected VM exit reason={}", reason));
            }
        }
    }
}

fn handle_mmio(vcpu: &mut VcpuHandle, access: &MmioAccess, uart: &Pl011) {
    if uart.contains(access.addr) {
        let offset = access.addr - uart.base_addr;
        if access.is_write {
            uart.write(offset, access.data, access.len);
        } else {
            let value = uart.read(offset, access.len);
            let bytes = value.to_le_bytes();
            vcpu.complete_mmio_read(&bytes[..access.len]);
        }
    } else {
        // Unknown MMIO region — return 0 for reads
        static LOGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let prev = LOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if prev < 20 {
            let pc = vcpu.get_reg(hypervisor::REG_PC);
            eprintln!(
                "MMIO {} to unmapped addr 0x{:x} (size={}, PC=0x{:x})",
                if access.is_write { "write" } else { "read" },
                access.addr,
                access.len,
                pc,
            );
        }
        if !access.is_write {
            let zero = [0u8; 8];
            vcpu.complete_mmio_read(&zero[..access.len]);
        }
    }

    // PC advancement:
    // - HVF: done inside hypervisor/hvf.rs run() when decoding the data abort
    // - KVM: done automatically by the kernel
    // No manual PC advance needed here.
}

fn handle_sys_reg_trap(vcpu: &VcpuHandle, syndrome: u64) {
    let direction = syndrome & 1; // 1 = read (MRS), 0 = write (MSR)
    let rt = ((syndrome >> 5) & 0x1f) as u32;
    let pc = vcpu.get_reg(hypervisor::REG_PC);

    eprintln!(
        "Trapped sys reg: ISS=0x{:x} dir={} Rt=x{} PC=0x{:x}",
        syndrome & 0x1FFFFF,
        direction,
        rt,
        pc
    );

    if direction == 1 {
        vcpu.set_reg(rt, 0);
    }

    // Advance PC on HVF (KVM does it automatically)
    #[cfg(target_os = "macos")]
    vcpu.set_reg(hypervisor::REG_PC, pc + 4);
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

// ── Public commands ──────────────────────────────────────────────────────────

/// Boot a Linux kernel.
pub fn cmd_boot_linux(kernel_path: &Path, initrd_path: Option<&Path>, quiet: bool) {
    let loaded = load_kernel_and_initrd(kernel_path, initrd_path, quiet);
    let LoadedKernel {
        mem,
        ram_size,
        kernel_entry,
        dtb_addr,
        ..
    } = loaded;

    let mut vm = VmHandle::create();
    vm.map_memory(mem, GUEST_RAM_BASE, ram_size, true);
    let _shared_mem = setup_shared_region(&mut vm);

    let mut vcpu = vm.create_vcpu();
    let _gic = vm.create_gic(dtb::GICD_BASE, dtb::GICR_BASE);
    let _has_timer_trapping = setup_cpu_for_linux(&vcpu, kernel_entry, dtb_addr);

    let uart = Pl011::new(dtb::UART_BASE);
    let mut vtimer = VirtualTimer::new();

    eprintln!("Starting Linux kernel...\n");

    let (watchdog, watchdog_stop) = spawn_watchdog(&vcpu, 10);
    let result = run_linux_vcpu_loop(&mut vcpu, mem, ram_size, &uart, &mut vtimer, None);
    watchdog_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = watchdog.join();

    match result {
        VmExitReason::Ready => eprintln!("VM reached HC_READY (use snapshot-linux to save)"),
        VmExitReason::Exit(code) => eprintln!("VM exited with code {}", code),
        VmExitReason::SystemOff => eprintln!("VM powered off"),
        VmExitReason::Error(e) => eprintln!("VM error: {}", e),
    }

    unsafe {
        libc::munmap(mem as *mut libc::c_void, ram_size);
    }
}

// ── Snapshot/Fork ────────────────────────────────────────────────────────────

use crate::snapshot::{CpuState, Template};

pub fn cmd_snapshot_linux(
    kernel_path: &Path,
    initrd_path: Option<&Path>,
    template_dir: &Path,
    quiet: bool,
) {
    let loaded = load_kernel_and_initrd(kernel_path, initrd_path, quiet);
    let LoadedKernel {
        mem,
        ram_size,
        kernel_entry,
        dtb_addr,
        kernel_file_size,
    } = loaded;

    let mut vm = VmHandle::create();
    vm.map_memory(mem, GUEST_RAM_BASE, ram_size, true);
    let shared_mem = setup_shared_region(&mut vm);

    let mut vcpu = vm.create_vcpu();
    let _gic = vm.create_gic(dtb::GICD_BASE, dtb::GICR_BASE);
    let has_timer_trapping = setup_cpu_for_linux(&vcpu, kernel_entry, dtb_addr);

    let uart = Pl011::new(dtb::UART_BASE);
    let mut vtimer = VirtualTimer::new();

    eprintln!("Booting Linux to snapshot point...\n");

    let (watchdog, watchdog_stop) = spawn_watchdog(&vcpu, 30);
    let result = run_linux_vcpu_loop(
        &mut vcpu,
        mem,
        ram_size,
        &uart,
        &mut vtimer,
        Some(shared_mem as *const u8),
    );
    watchdog_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = watchdog.join();

    match result {
        VmExitReason::Ready => eprintln!("Init signaled READY"),
        VmExitReason::SystemOff => panic!("VM halted before signaling READY"),
        VmExitReason::Exit(c) => panic!("VM exited with code {} before READY", c),
        VmExitReason::Error(e) => panic!("VM error before READY: {}", e),
    }

    // Create template directory early (timer metadata is saved before CPU state)
    std::fs::create_dir_all(template_dir).expect("create template dir");

    // Timer determinism for fork:
    // - If CNTHCTL_EL2 trapping works: deterministic (no patching needed)
    // - HVF without EL2: patch with HVC instructions (exits to userspace)
    // - KVM without CNTHCTL_EL2 (pKVM): patch with MMIO LDR/STR [X18, #off]
    //   (KVM_EXIT_MMIO exits to userspace). Must set X18 = TIMER_MMIO_GPA.
    if !has_timer_trapping {
        let kernel_load_offset = KERNEL_OFFSET as usize;
        #[cfg(target_os = "macos")]
        let mode = TimerPatchMode::Hvc;
        #[cfg(target_os = "linux")]
        let mode = TimerPatchMode::Brk;
        let patched = patch_timer_reads(mem, kernel_load_offset, kernel_file_size, &mode);
        eprintln!(
            "Patched {} timer instructions in snapshot ({} mode)",
            patched,
            match &mode {
                #[cfg(target_os = "macos")]
                TimerPatchMode::Hvc => "HVC",
                TimerPatchMode::Brk => "BRK",
            }
        );

        // Note: no VA-to-GPA offset needed — the BRK immediate is extracted
        // from the HSR (exception syndrome) in the KVM_EXIT_DEBUG info.
    }

    // Save CPU state
    let cpu_state = CpuState::capture(&vcpu);

    let mem_path = template_dir.join("guest.mem");
    let mem_bytes = unsafe { std::slice::from_raw_parts(mem, ram_size) };
    std::fs::write(&mem_path, mem_bytes).expect("write guest.mem");

    let template = Template::new(cpu_state, mem_path.clone(), ram_size, GUEST_RAM_BASE);
    template.save(template_dir);

    // Save whether timer was patched (fork needs to create MMIO hole)
    let timer_patched = !has_timer_trapping;
    std::fs::write(
        template_dir.join("timer_patched"),
        if timer_patched { "brk" } else { "none" },
    )
    .expect("write timer_patched");

    // TODO: Save GIC state, ICC regs, vtimer state for KVM

    let shared_bytes = unsafe { std::slice::from_raw_parts(shared_mem, LINUX_SHARED_SIZE) };
    std::fs::write(template_dir.join("shared.mem"), shared_bytes).expect("write shared.mem");

    unsafe {
        libc::munmap(mem as *mut libc::c_void, ram_size);
    }

    eprintln!(
        "Template saved to {}/ (mem={:.1} MiB)",
        template_dir.display(),
        ram_size as f64 / (1024.0 * 1024.0),
    );
}

pub fn cmd_fork_linux(template_dir: &Path, inbox_data: &[u8]) {
    let t0 = std::time::Instant::now();
    let template = Template::load(template_dir);
    let t_load = t0.elapsed();
    let mem = template.mmap_cow_memory();
    let t_mmap = t0.elapsed();
    let ram_size = template.mem_size;

    // Check if the template was built with BRK timer patching
    let timer_patched =
        std::fs::read_to_string(template_dir.join("timer_patched")).unwrap_or_default();
    let needs_guest_debug = timer_patched.trim() == "brk";

    let shared_mem = alloc_pages(LINUX_SHARED_SIZE);
    assert!(inbox_data.len() < LINUX_INBOX_SIZE, "inbox data too large");
    unsafe {
        ptr::copy_nonoverlapping(inbox_data.as_ptr(), shared_mem, inbox_data.len());
        *shared_mem.add(inbox_data.len()) = 0;
    }

    let mut vm = VmHandle::create();
    let t_vm = t0.elapsed();
    vm.map_memory(mem, GUEST_RAM_BASE, ram_size, true);
    vm.map_memory(shared_mem, LINUX_SHARED_GPA, LINUX_SHARED_SIZE, false);
    let t_map = t0.elapsed();

    let mut vcpu = vm.create_vcpu();
    let _gic = vm.create_gic(dtb::GICD_BASE, dtb::GICR_BASE);
    let t_vcpu_gic = t0.elapsed();

    vcpu.set_sys_reg(SysReg::MPIDR_EL1, 0x8000_0000);
    if needs_guest_debug {
        vcpu.enable_guest_debug();
    }

    // TODO: Restore GIC state, ICC regs, vtimer state
    template.cpu_state.restore(&vcpu);
    let t_restore = t0.elapsed();

    let uart = Pl011::new(dtb::UART_BASE);
    let mut vtimer = VirtualTimer::new();

    let (watchdog, watchdog_stop) = spawn_watchdog_fast(&vcpu, 300);
    let result = run_linux_vcpu_loop(&mut vcpu, mem, ram_size, &uart, &mut vtimer, None);
    let t_run = t0.elapsed();
    watchdog_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = watchdog.join();

    eprintln!("Fork timing: load={:.1}ms mmap={:.1}ms vm={:.1}ms map={:.1}ms vcpu+gic={:.1}ms restore={:.1}ms run={:.1}ms total={:.1}ms",
        t_load.as_secs_f64() * 1000.0,
        (t_mmap - t_load).as_secs_f64() * 1000.0,
        (t_vm - t_mmap).as_secs_f64() * 1000.0,
        (t_map - t_vm).as_secs_f64() * 1000.0,
        (t_vcpu_gic - t_map).as_secs_f64() * 1000.0,
        (t_restore - t_vcpu_gic).as_secs_f64() * 1000.0,
        (t_run - t_restore).as_secs_f64() * 1000.0,
        t_run.as_secs_f64() * 1000.0,
    );

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

    // Read outbox
    let outbox_ptr = unsafe { shared_mem.add(LINUX_INBOX_SIZE) };
    let mut outbox_len = 0usize;
    unsafe {
        while outbox_len < LINUX_OUTBOX_SIZE && *outbox_ptr.add(outbox_len) != 0 {
            outbox_len += 1;
        }
    }
    if outbox_len > 0 {
        let outbox_bytes = unsafe { std::slice::from_raw_parts(outbox_ptr, outbox_len) };
        use std::io::Write;
        std::io::stdout().write_all(outbox_bytes).ok();
        if outbox_bytes.last() != Some(&b'\n') {
            std::io::stdout().write_all(b"\n").ok();
        }
        std::io::stdout().flush().ok();
    }

    unsafe {
        libc::munmap(mem as *mut libc::c_void, ram_size);
        libc::munmap(shared_mem as *mut libc::c_void, LINUX_SHARED_SIZE);
    }
}

/// Serve mode: pre-load the template, then handle multiple requests in a loop.
/// Avoids process startup overhead by reusing the VM fd across invocations.
/// Each invocation re-creates vCPU + GIC and re-mmaps memory (CoW).
/// Reads JS from stdin lines, prints output to stdout.
pub fn cmd_serve_linux(template_dir: &Path) {
    let t0 = std::time::Instant::now();

    let template = Template::load(template_dir);
    let ram_size = template.mem_size;

    let timer_patched =
        std::fs::read_to_string(template_dir.join("timer_patched")).unwrap_or_default();
    let needs_guest_debug = timer_patched.trim() == "brk";

    let setup_time = t0.elapsed();
    eprintln!(
        "Serve: template loaded in {:.1}ms. Reading JS from stdin...",
        setup_time.as_secs_f64() * 1000.0
    );

    use std::io::BufRead;
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let js = match line {
            Ok(l) if !l.is_empty() => l,
            Ok(_) => continue,
            Err(_) => break,
        };

        let t_start = std::time::Instant::now();

        // Re-mmap guest memory as CoW for this invocation
        let mem = template.mmap_cow_memory();

        // Fresh shared region for this invocation
        let shared_mem = alloc_pages(LINUX_SHARED_SIZE);
        let inbox_data = js.as_bytes();
        assert!(inbox_data.len() < LINUX_INBOX_SIZE);
        unsafe {
            ptr::copy_nonoverlapping(inbox_data.as_ptr(), shared_mem, inbox_data.len());
            *shared_mem.add(inbox_data.len()) = 0;
        }

        // Create fresh VM + vCPU + GIC for this invocation
        let mut vm = VmHandle::create();
        vm.map_memory(mem, GUEST_RAM_BASE, ram_size, true);
        vm.map_memory(shared_mem, LINUX_SHARED_GPA, LINUX_SHARED_SIZE, false);

        let mut vcpu = vm.create_vcpu();
        let _gic = vm.create_gic(dtb::GICD_BASE, dtb::GICR_BASE);
        vcpu.set_sys_reg(SysReg::MPIDR_EL1, 0x8000_0000);
        if needs_guest_debug {
            vcpu.enable_guest_debug();
        }
        template.cpu_state.restore(&vcpu);

        let uart = Pl011::new(dtb::UART_BASE);
        let mut vtimer = VirtualTimer::new();
        let _result = run_linux_vcpu_loop(&mut vcpu, mem, ram_size, &uart, &mut vtimer, None);
        let t_done = t_start.elapsed();

        // Read outbox
        let outbox_ptr = unsafe { shared_mem.add(LINUX_INBOX_SIZE) };
        let mut outbox_len = 0usize;
        unsafe {
            while outbox_len < LINUX_OUTBOX_SIZE && *outbox_ptr.add(outbox_len) != 0 {
                outbox_len += 1;
            }
        }
        if outbox_len > 0 {
            let outbox_bytes = unsafe { std::slice::from_raw_parts(outbox_ptr, outbox_len) };
            use std::io::Write;
            std::io::stdout().write_all(outbox_bytes).ok();
            if outbox_bytes.last() != Some(&b'\n') {
                std::io::stdout().write_all(b"\n").ok();
            }
            std::io::stdout().flush().ok();
        }

        eprintln!("  fork: {:.1}ms", t_done.as_secs_f64() * 1000.0);

        // Cleanup
        drop(vcpu);
        drop(vm);
        unsafe {
            libc::munmap(mem as *mut libc::c_void, ram_size);
            libc::munmap(shared_mem as *mut libc::c_void, LINUX_SHARED_SIZE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(dead_code)]
    fn make_data_abort_syndrome(isv: bool, sas: u32, sse: bool, srt: u32, wnr: bool) -> u64 {
        let mut s: u64 = 0;
        if isv {
            s |= 1 << 24;
        }
        s |= ((sas as u64) & 3) << 22;
        if sse {
            s |= 1 << 21;
        }
        s |= ((srt as u64) & 0x1f) << 16;
        if wnr {
            s |= 1 << 6;
        }
        s
    }

    #[test]
    fn patch_timer_reads_brk_replaces_cntvct() {
        let mut buf = vec![0u8; 16];
        let mrs_x0_cntvct: u32 = 0xd53be040;
        let nop: u32 = 0xd503201f;
        unsafe {
            ptr::write(buf.as_mut_ptr() as *mut u32, mrs_x0_cntvct);
            ptr::write(buf.as_mut_ptr().add(4) as *mut u32, nop);
            ptr::write(buf.as_mut_ptr().add(8) as *mut u32, 0xd53be041); // MRS X1, CNTVCT
            ptr::write(buf.as_mut_ptr().add(12) as *mut u32, nop);
        }

        let count = patch_timer_reads(buf.as_mut_ptr(), 0, 16, &TimerPatchMode::Brk);
        assert_eq!(count, 2);

        // First should be BRK #0x100 (counter read, X0)
        let patched0 = unsafe { ptr::read(buf.as_ptr() as *const u32) };
        assert_eq!(patched0, encode_brk(BRK_COUNTER_READ + 0));

        // Second should be BRK #0x101 (counter read, X1)
        let patched1 = unsafe { ptr::read(buf.as_ptr().add(8) as *const u32) };
        assert_eq!(patched1, encode_brk(BRK_COUNTER_READ + 1));

        // NOP should be unchanged
        let nop0 = unsafe { ptr::read(buf.as_ptr().add(4) as *const u32) };
        assert_eq!(nop0, nop);
    }

    #[test]
    fn patch_timer_reads_brk_handles_msr_writes() {
        let mut buf = vec![0u8; 8];
        let msr_x2_cval: u32 = 0xd51be342; // MSR CNTV_CVAL_EL0, X2
        unsafe {
            ptr::write(buf.as_mut_ptr() as *mut u32, msr_x2_cval);
            ptr::write(buf.as_mut_ptr().add(4) as *mut u32, 0xd503201f);
        }

        let count = patch_timer_reads(buf.as_mut_ptr(), 0, 8, &TimerPatchMode::Brk);
        assert_eq!(count, 1);

        // Should be BRK #0x242 (CVAL write, X2)
        let patched = unsafe { ptr::read(buf.as_ptr() as *const u32) };
        assert_eq!(patched, encode_brk(BRK_CVAL_WRITE + 2));
    }

    #[test]
    fn encode_brk_encoding() {
        // BRK #0 = 0xD4200000
        assert_eq!(encode_brk(0), 0xD4200000);
        // BRK #0x100 = 0xD4200000 | (0x100 << 5)
        assert_eq!(encode_brk(0x100), 0xD4200000 | (0x100 << 5));
    }
}
