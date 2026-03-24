use std::path::Path;

mod dtb;
mod elf;
mod hypervisor;
mod linux_boot;
mod pl011;
mod psci;
mod snapshot;
#[cfg(target_os = "linux")]
mod uart8250;
mod vtimer;

use std::ptr;

#[cfg(target_os = "macos")]
use std::time::Instant;

#[allow(unused_imports)]
use snapshot::{CpuState, Template};

#[cfg(target_os = "macos")]
use rand::RngCore;
#[cfg(target_os = "macos")]
use rand::SeedableRng;
#[cfg(target_os = "macos")]
use rand_chacha::ChaCha8Rng;

pub(crate) const PAGE_SIZE: usize = 16384;

/// Per-VM deterministic state (Phase 1 bare-metal, macOS only).
#[cfg(target_os = "macos")]
struct VmState {
    virtual_time_ns: u64,
    rng: ChaCha8Rng,
}

#[cfg(target_os = "macos")]
impl VmState {
    fn new(seed: u64) -> Self {
        let mut full_seed = [0u8; 32];
        full_seed[..8].copy_from_slice(&seed.to_le_bytes());
        VmState {
            virtual_time_ns: 1_700_000_000_000_000_000,
            rng: ChaCha8Rng::from_seed(full_seed),
        }
    }
}

pub(crate) fn page_align(size: usize) -> usize {
    (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

pub(crate) fn alloc_pages(size: usize) -> *mut u8 {
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

// Phase 1 bare-metal guest code (macOS/HVF only).
// Uses raw HVF FFI for the bare-metal hypercall dispatch loop.
#[cfg(target_os = "macos")]
mod phase1_bare_metal {
use super::*;
use convex_shared::{HC_CONSOLE, HC_DB_READ, HC_EXIT, HC_RANDOM, HC_READY, HC_TIME};
use convex_shared::{GUEST_BASE, GUEST_MEM_SIZE, MAILBOX_OFFSET, MAILBOX_SIZE};
use crate::hypervisor::hvf::*;

/// Load the guest ELF into a freshly allocated memory region.
pub fn load_guest_elf(guest_elf_path: &Path) -> (*mut u8, usize) {
    let elf_data = std::fs::read(guest_elf_path)
        .unwrap_or_else(|e| panic!("Failed to read guest ELF {}: {}", guest_elf_path.display(), e));
    let elf = elf::GuestElf::parse(&elf_data).expect("Failed to parse guest ELF");
    eprintln!("Guest ELF: entry=0x{:x}, {} LOAD segments", elf.entry, elf.loads.len());

    let region_size = page_align(GUEST_MEM_SIZE);
    let mem = alloc_pages(region_size);

    for load in &elf.loads {
        let offset = (load.vaddr - GUEST_BASE) as usize;
        assert!(offset + load.data.len() <= region_size, "LOAD segment exceeds guest memory");
        unsafe { ptr::copy_nonoverlapping(load.data.as_ptr(), mem.add(offset), load.data.len()); }
        eprintln!("  Loaded: vaddr=0x{:x}, size={}, flags={}", load.vaddr, load.data.len(), load.flags_str());
    }
    (mem, region_size)
}

/// Create a VM with memory mapped and a vCPU ready to run (uses hypervisor abstraction).
fn create_vm_with_memory(mem: *mut u8, mem_size: usize, entry: u64)
    -> (hypervisor::VmHandle, hypervisor::VcpuHandle)
{
    use hypervisor::SysReg;
    let mut vm = hypervisor::VmHandle::create();
    vm.map_memory(mem, GUEST_BASE, mem_size, true);
    let vcpu = vm.create_vcpu();
    vcpu.set_reg(hypervisor::REG_PC, entry);
    vcpu.set_reg(hypervisor::REG_CPSR, 0x3c5);
    vcpu.set_sys_reg(SysReg::SCTLR_EL1, 0x30d00800);
    vcpu.set_sys_reg(SysReg::CPACR_EL1, 3 << 20);
    vcpu.set_sys_reg(SysReg::SP_EL1, GUEST_BASE + mem_size as u64);
    (vm, vcpu)
}

fn run_vcpu_loop(
    vcpu: &hypervisor::VcpuHandle,
    vm_state: &mut VmState,
    guest_mem: *mut u8,
    mem_size: usize,
) -> (u64, u64) {
    run_vcpu_loop_inner(vcpu, vm_state, guest_mem, mem_size, false)
}

/// Phase 1 bare-metal vCPU loop: dispatches hypercalls via raw HVF FFI.
fn run_vcpu_loop_inner(
    vcpu: &hypervisor::VcpuHandle,
    vm_state: &mut VmState,
    guest_mem: *mut u8,
    mem_size: usize,
    quiet: bool,
) -> (u64, u64) {
    // Get raw HVF handles for the tight loop
    let raw_vcpu = vcpu.raw_vcpu();
    let exit_ptr = vcpu.raw_exit_ptr();

    loop {
        unsafe { check_hv(hv_vcpu_run(raw_vcpu), "hv_vcpu_run"); }
        let exit = unsafe { &*exit_ptr };

        match exit.reason {
            HV_EXIT_REASON_EXCEPTION => {
                let ec = (exit.exception.syndrome >> 26) & 0x3f;
                if ec != 0x16 {
                    let pc = unsafe { vcpu_get_reg(raw_vcpu, HV_REG_PC) };
                    let lr = unsafe { vcpu_get_reg(raw_vcpu, 30) };
                    let sp = unsafe { vcpu_get_sys_reg(raw_vcpu, HV_SYS_REG_SP_EL1) };
                    let cpacr = unsafe { vcpu_get_sys_reg(raw_vcpu, HV_SYS_REG_CPACR_EL1) };
                    panic!(
                        "Unexpected exception: EC=0x{:x}, syndrome=0x{:x}, PC=0x{:x}, IPA=0x{:x}, LR=0x{:x}, SP=0x{:x}, CPACR=0x{:x}",
                        ec, exit.exception.syndrome, pc, exit.exception.physical_address, lr, sp, cpacr
                    );
                }

                let x0 = unsafe { vcpu_get_reg(raw_vcpu, HV_REG_X0) };
                match x0 {
                    HC_CONSOLE => {
                        let ptr_gpa = unsafe { vcpu_get_reg(raw_vcpu, HV_REG_X1) };
                        let len = unsafe { vcpu_get_reg(raw_vcpu, HV_REG_X2) } as usize;
                        if !quiet {
                            if let Some(p) = gpa_to_host_ptr(ptr_gpa, len, guest_mem, GUEST_BASE, mem_size) {
                                let bytes = unsafe { std::slice::from_raw_parts(p, len) };
                                use std::io::Write;
                                std::io::stdout().write_all(bytes).ok();
                            }
                        }
                        unsafe { check_hv(hv_vcpu_set_reg(raw_vcpu, HV_REG_X0, 0), "set x0"); }
                    }
                    HC_TIME => unsafe {
                        check_hv(hv_vcpu_set_reg(raw_vcpu, HV_REG_X0, vm_state.virtual_time_ns), "set x0 time");
                    },
                    HC_RANDOM => {
                        let value = vm_state.rng.next_u64();
                        unsafe { check_hv(hv_vcpu_set_reg(raw_vcpu, HV_REG_X0, value), "set x0 random"); }
                    }
                    HC_DB_READ => {
                        let req_len = unsafe { vcpu_get_reg(raw_vcpu, HV_REG_X1) } as usize;
                        let mailbox_ptr = unsafe { guest_mem.add(MAILBOX_OFFSET as usize) };
                        let request = if req_len <= MAILBOX_SIZE {
                            unsafe { std::slice::from_raw_parts(mailbox_ptr, req_len) }
                        } else { b"" as &[u8] };
                        let request_str = std::str::from_utf8(request).unwrap_or("");
                        if !quiet { eprintln!("HC_DB_READ: {:?}", request_str); }
                        let response = match request_str {
                            "users" => r#"[{"id":1,"name":"Alice"},{"id":2,"name":"Bob"}]"#,
                            "posts" => r#"[{"id":1,"title":"Hello World","author":"Alice"}]"#,
                            _ => "[]",
                        };
                        let resp_bytes = response.as_bytes();
                        let resp_len = resp_bytes.len().min(MAILBOX_SIZE - 1);
                        unsafe {
                            ptr::copy_nonoverlapping(resp_bytes.as_ptr(), mailbox_ptr, resp_len);
                            *mailbox_ptr.add(resp_len) = 0;
                            check_hv(hv_vcpu_set_reg(raw_vcpu, HV_REG_X0, resp_len as u64), "set x0 db_read");
                        }
                    }
                    HC_READY => { return (HC_READY, 0); }
                    HC_EXIT => {
                        let exit_code = unsafe { vcpu_get_reg(raw_vcpu, HV_REG_X1) };
                        return (HC_EXIT, exit_code);
                    }
                    0xDE => {
                        let esr = unsafe { vcpu_get_reg(raw_vcpu, HV_REG_X1) };
                        let far = unsafe { vcpu_get_reg(raw_vcpu, HV_REG_X2) };
                        let elr = unsafe { vcpu_get_reg(raw_vcpu, HV_REG_X3) };
                        panic!("Guest EL1 exception: EC=0x{:x} ESR=0x{:x} FAR=0x{:x} ELR=0x{:x}", (esr >> 26) & 0x3f, esr, far, elr);
                    }
                    other => {
                        eprintln!("Unknown hypercall 0x{:x}", other);
                        unsafe { check_hv(hv_vcpu_set_reg(raw_vcpu, HV_REG_X0, u64::MAX), "set x0 err"); }
                    }
                }
            }
            other => {
                let pc = unsafe { vcpu_get_reg(raw_vcpu, HV_REG_PC) };
                panic!("Unexpected VM exit: reason={}, PC=0x{:x}", other, pc);
            }
        }
    }
}

fn gpa_to_host_ptr(gpa: u64, len: usize, host_base: *mut u8, guest_base: u64, region_size: usize) -> Option<*const u8> {
    if gpa < guest_base { return None; }
    let offset = (gpa - guest_base) as usize;
    if offset + len > region_size { return None; }
    Some(unsafe { host_base.add(offset) })
}

// ── Commands ────────────────────────────────────────────────────────────────

pub fn cmd_snapshot(guest_elf_path: &Path, template_dir: &Path) {
    let (mem, mem_size) = load_guest_elf(guest_elf_path);
    let (_vm, vcpu) = create_vm_with_memory(mem, mem_size, GUEST_BASE);
    let mut vm_state = VmState::new(0);

    let (hc, _) = run_vcpu_loop(&vcpu, &mut vm_state, mem, mem_size);
    assert_eq!(hc, HC_READY, "Guest didn't reach HC_READY");

    let cpu_state = CpuState::capture(&vcpu);

    let mem_path = template_dir.join("guest.mem");
    std::fs::create_dir_all(template_dir).expect("create template dir");
    let mem_bytes = unsafe { std::slice::from_raw_parts(mem, mem_size) };
    std::fs::write(&mem_path, mem_bytes).expect("write guest.mem");

    let template = Template { cpu_state, mem_path: mem_path.clone(), mem_size, guest_base: GUEST_BASE };
    template.save(template_dir);

    drop(vcpu); // destroys vCPU
    // _vm dropped here — destroys VM
    unsafe { libc::munmap(mem as *mut libc::c_void, mem_size); }

    eprintln!("Template saved to {}/ (mem={} bytes)", template_dir.display(), mem_size);
}

pub fn cmd_fork(template_dir: &Path, seed: u64, mailbox_data: &[u8]) -> u64 {
    fork_inner(template_dir, seed, mailbox_data, false)
}

fn cmd_fork_quiet(template_dir: &Path, seed: u64, mailbox_data: &[u8]) -> u64 {
    fork_inner(template_dir, seed, mailbox_data, true)
}

fn fork_inner(template_dir: &Path, seed: u64, mailbox_data: &[u8], quiet: bool) -> u64 {
    let template = Template::load(template_dir);
    let mem = template.mmap_cow_memory();
    let mem_size = template.mem_size;

    let mailbox_host_offset = MAILBOX_OFFSET as usize;
    assert!(mailbox_data.len() < convex_shared::MAILBOX_SIZE);
    unsafe {
        ptr::copy_nonoverlapping(mailbox_data.as_ptr(), mem.add(mailbox_host_offset), mailbox_data.len());
        *mem.add(mailbox_host_offset + mailbox_data.len()) = 0;
    }

    let mut vm = hypervisor::VmHandle::create();
    vm.map_memory(mem, GUEST_BASE, mem_size, true);
    let vcpu = vm.create_vcpu();
    template.cpu_state.restore(&vcpu);

    let mut vm_state = VmState::new(seed);
    let (hc, exit_code) = run_vcpu_loop_inner(&vcpu, &mut vm_state, mem, mem_size, quiet);
    assert_eq!(hc, HC_EXIT, "Forked VM didn't exit cleanly");

    drop(vcpu);
    drop(vm);
    unsafe { libc::munmap(mem as *mut libc::c_void, mem_size); }
    exit_code
}

/// Benchmark fork latency. Suppresses guest stdout.
fn cmd_bench(template_dir: &Path, iterations: usize, js_code: &str) {
    let js_bytes = js_code.as_bytes();
    let label = if js_bytes.is_empty() {
        "fork+run (empty)"
    } else {
        "fork+eval JS"
    };
    eprintln!("Benchmarking {} {} iterations...", iterations, label);
    if !js_bytes.is_empty() {
        eprintln!("  JS: {}", js_code);
    }

    // Warmup
    cmd_fork_quiet(template_dir, 0, js_bytes);

    let mut fork_times = Vec::with_capacity(iterations);

    for i in 0..iterations {
        let t_start = Instant::now();
        let exit_code = cmd_fork_quiet(template_dir, i as u64, js_bytes);
        let elapsed = t_start.elapsed();
        fork_times.push(elapsed);
        assert_eq!(exit_code, 0, "Unexpected exit code on iteration {}", i);
    }

    fork_times.sort();
    let p50 = fork_times[iterations / 2];
    let p99 = fork_times[iterations * 99 / 100];
    let total: std::time::Duration = fork_times.iter().sum();
    let avg = total / iterations as u32;

    eprintln!("\n{} latency ({} iterations):", label, iterations);
    eprintln!("  p50:  {:?}", p50);
    eprintln!("  p99:  {:?}", p99);
    eprintln!("  avg:  {:?}", avg);
    eprintln!("  min:  {:?}", fork_times[0]);
    eprintln!("  max:  {:?}", fork_times[iterations - 1]);
    eprintln!(
        "  throughput: {:.0} exec/sec",
        iterations as f64 / total.as_secs_f64()
    );

    // Memory info: template size on disk + CoW overhead
    if let Ok(mem_meta) = std::fs::metadata(template_dir.join("guest.mem")) {
        let mem_mb = mem_meta.len() as f64 / (1024.0 * 1024.0);
        eprintln!("\nMemory:");
        eprintln!("  template size: {:.1} MiB (guest.mem)", mem_mb);
        eprintln!(
            "  per-fork CoW:  ~0 MiB (MAP_PRIVATE, pages copied on write only)"
        );
    }
}

/// Direct run (no snapshot/fork) — the M0/M1 mode.
pub fn cmd_run(guest_elf_path: &Path, seed: u64) {
    let (mem, mem_size) = load_guest_elf(guest_elf_path);
    let (_vm, vcpu) = create_vm_with_memory(mem, mem_size, GUEST_BASE);
    let mut vm_state = VmState::new(seed);

    eprintln!("VM created (seed={}). Running guest...\n", seed);

    let (hc, exit_code) = run_vcpu_loop(&vcpu, &mut vm_state, mem, mem_size);

    match hc {
        HC_EXIT => eprintln!("\nGuest exited with code {}", exit_code),
        HC_READY => eprintln!("\nGuest reached HC_READY (use 'snapshot' command to save)"),
        _ => unreachable!(),
    }

    drop(vcpu);
    // _vm dropped here
    unsafe { libc::munmap(mem as *mut libc::c_void, mem_size); }
}
} // mod phase1_bare_metal

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage:");
        eprintln!("  convex-hypervisor run [--seed N] <guest.elf>");
        eprintln!("  convex-hypervisor snapshot <guest.elf> <template-dir>");
        eprintln!("  convex-hypervisor fork [--seed N] [--js 'code'] <template-dir>");
        eprintln!("  convex-hypervisor bench [--iterations N] [--js 'code'] <template-dir>");
        eprintln!("  convex-hypervisor boot-linux <kernel-image> [--initrd <initrd>] [--quiet]");
        eprintln!("  convex-hypervisor snapshot-linux <kernel-image> --initrd <initrd> [--quiet] <template-dir>");
        eprintln!("  convex-hypervisor fork-linux [--msg 'text' | --js-file path] <template-dir>");
        std::process::exit(1);
    }

    match args[1].as_str() {
        "boot-linux" => {
            let (kernel_path, initrd_path, quiet) = parse_boot_linux_args(&args[2..]);
            linux_boot::cmd_boot_linux(
                Path::new(&kernel_path),
                initrd_path.as_deref().map(Path::new),
                quiet,
            );
        }
        "snapshot-linux" => {
            let (kernel_path, initrd_path, template_dir, quiet) = parse_snapshot_linux_args(&args[2..]);
            linux_boot::cmd_snapshot_linux(
                Path::new(&kernel_path),
                initrd_path.as_deref().map(Path::new),
                Path::new(&template_dir),
                quiet,
            );
        }
        "fork-linux" => {
            let (msg, template_dir) = parse_fork_linux_args(&args[2..]);
            linux_boot::cmd_fork_linux(Path::new(&template_dir), msg.as_bytes());
        }
        #[cfg(target_os = "macos")]
        "run" => {
            let (seed, guest_path) = parse_run_args(&args[2..]);
            phase1_bare_metal::cmd_run(Path::new(&guest_path), seed);
        }
        #[cfg(target_os = "macos")]
        "snapshot" => {
            let guest_path = args.get(2).expect("missing guest ELF path");
            let template_dir = args.get(3).expect("missing template dir");
            phase1_bare_metal::cmd_snapshot(Path::new(guest_path), Path::new(template_dir));
        }
        #[cfg(target_os = "macos")]
        "fork" => {
            let (seed, js_code, template_dir) = parse_fork_args(&args[2..]);
            let exit_code = phase1_bare_metal::cmd_fork(Path::new(&template_dir), seed, js_code.as_bytes());
            eprintln!("Guest exited with code {}", exit_code);
        }
        #[cfg(target_os = "macos")]
        "bench" => {
            let (iterations, js_code, template_dir) = parse_bench_args(&args[2..]);
            phase1_bare_metal::cmd_bench(Path::new(&template_dir), iterations, &js_code);
        }
        _ => {
            eprintln!("Unknown command: {}", args[1]);
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
fn parse_run_args(args: &[String]) -> (u64, String) {
    let mut seed: u64 = 42;
    let mut guest_path = "guest/target/aarch64-unknown-none/release/convex-guest".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                seed = args
                    .get(i + 1)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| {
                        eprintln!("--seed requires a u64 value");
                        std::process::exit(1);
                    });
                i += 2;
            }
            other => {
                guest_path = other.to_string();
                i += 1;
            }
        }
    }
    (seed, guest_path)
}

#[cfg(target_os = "macos")]
fn parse_fork_args(args: &[String]) -> (u64, String, String) {
    let mut seed: u64 = 42;
    let mut js_code = String::new();
    let mut template_dir = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--seed" => {
                seed = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(42);
                i += 2;
            }
            "--js" => {
                js_code = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            other => {
                template_dir = other.to_string();
                i += 1;
            }
        }
    }
    (seed, js_code, template_dir)
}

#[cfg(target_os = "macos")]
fn parse_bench_args(args: &[String]) -> (usize, String, String) {
    let mut iterations: usize = 1000;
    let mut js_code = String::new();
    let mut template_dir = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--iterations" => {
                iterations = args.get(i + 1).and_then(|s| s.parse().ok()).unwrap_or(1000);
                i += 2;
            }
            "--js" => {
                js_code = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            other => {
                template_dir = other.to_string();
                i += 1;
            }
        }
    }
    (iterations, js_code, template_dir)
}

fn parse_boot_linux_args(args: &[String]) -> (String, Option<String>, bool) {
    let mut kernel_path = String::new();
    let mut initrd_path: Option<String> = None;
    let mut quiet = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--initrd" => {
                initrd_path = args.get(i + 1).cloned();
                i += 2;
            }
            "--quiet" => {
                quiet = true;
                i += 1;
            }
            other => {
                kernel_path = other.to_string();
                i += 1;
            }
        }
    }
    if kernel_path.is_empty() {
        eprintln!("boot-linux requires a kernel image path");
        std::process::exit(1);
    }
    (kernel_path, initrd_path, quiet)
}

fn parse_snapshot_linux_args(args: &[String]) -> (String, Option<String>, String, bool) {
    let mut kernel_path = String::new();
    let mut initrd_path: Option<String> = None;
    let mut template_dir = String::new();
    let mut quiet = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--initrd" => {
                initrd_path = args.get(i + 1).cloned();
                i += 2;
            }
            "--quiet" => {
                quiet = true;
                i += 1;
            }
            other => {
                if kernel_path.is_empty() {
                    kernel_path = other.to_string();
                } else {
                    template_dir = other.to_string();
                }
                i += 1;
            }
        }
    }
    if kernel_path.is_empty() || template_dir.is_empty() {
        eprintln!("snapshot-linux requires: <kernel-image> --initrd <initrd> <template-dir>");
        std::process::exit(1);
    }
    (kernel_path, initrd_path, template_dir, quiet)
}

fn parse_fork_linux_args(args: &[String]) -> (String, String) {
    let mut msg = String::new();
    let mut template_dir = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--msg" => {
                msg = args.get(i + 1).cloned().unwrap_or_default();
                i += 2;
            }
            "--js-file" => {
                let path = args.get(i + 1).cloned().unwrap_or_default();
                msg = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("failed to read {}: {}", path, e));
                i += 2;
            }
            other => {
                template_dir = other.to_string();
                i += 1;
            }
        }
    }
    if template_dir.is_empty() {
        eprintln!("fork-linux requires: [--msg 'text' | --js-file path] <template-dir>");
        std::process::exit(1);
    }
    (msg, template_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_align() {
        assert_eq!(page_align(0), 0);
        assert_eq!(page_align(1), PAGE_SIZE);
        assert_eq!(page_align(PAGE_SIZE), PAGE_SIZE);
        assert_eq!(page_align(PAGE_SIZE + 1), PAGE_SIZE * 2);
    }

    #[test]
    fn test_elf_parse_rejects_bad_input() {
        assert!(elf::GuestElf::parse(&[]).is_err());
        assert!(elf::GuestElf::parse(&[0; 64]).is_err());
        assert!(elf::GuestElf::parse(b"\x7fELF").is_err());
    }

    #[test]
    fn test_cpu_state_roundtrip() {
        use hypervisor::{SimdReg, SNAPSHOT_SYS_REGS};

        let state = CpuState {
            gpr: {
                let mut g = [0u64; 35];
                for i in 0..35 {
                    g[i] = (i as u64) * 0x1111;
                }
                g
            },
            sys_regs: {
                let len = SNAPSHOT_SYS_REGS.len();
                (0..len).map(|i| (i as u64) * 0x1111 + 0xAAAA).collect()
            },
            simd: {
                let mut s = [SimdReg::default(); 32];
                s[0] = SimdReg([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
                s
            },
        };

        let bytes = state.to_bytes();
        let restored = CpuState::from_bytes(&bytes);

        assert_eq!(state.gpr, restored.gpr);
        assert_eq!(state.sys_regs, restored.sys_regs);
        assert_eq!(state.simd, restored.simd);
    }
}
