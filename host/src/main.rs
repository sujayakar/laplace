use std::path::Path;
use std::ptr;
use std::time::Instant;

mod dtb;
mod elf;
mod hvf;
mod linux_boot;
mod pl011;
mod psci;
mod snapshot;
mod vtimer;

use convex_shared::{HC_CONSOLE, HC_DB_READ, HC_EXIT, HC_RANDOM, HC_READY, HC_TIME};
use convex_shared::{GUEST_BASE, GUEST_MEM_SIZE, MAILBOX_OFFSET, MAILBOX_SIZE};
use hvf::{
    check_hv, HvVcpuExit, HV_EXIT_REASON_EXCEPTION, HV_MEMORY_EXEC, HV_MEMORY_READ,
    HV_MEMORY_WRITE, HV_REG_CPSR, HV_REG_PC, HV_REG_X0, HV_REG_X1, HV_REG_X2,
    HV_SYS_REG_CPACR_EL1, HV_SYS_REG_SCTLR_EL1, HV_SYS_REG_SP_EL1,
};
use snapshot::{CpuState, Template};

use rand::RngCore;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

pub(crate) const PAGE_SIZE: usize = 16384;

/// Per-VM deterministic state.
struct VmState {
    virtual_time_ns: u64,
    rng: ChaCha8Rng,
}

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

/// Load the guest ELF into a freshly allocated memory region.
/// Returns (host_ptr, region_size).
fn load_guest_elf(guest_elf_path: &Path) -> (*mut u8, usize) {
    let elf_data = std::fs::read(guest_elf_path)
        .unwrap_or_else(|e| panic!("Failed to read guest ELF {}: {}", guest_elf_path.display(), e));
    let elf = elf::GuestElf::parse(&elf_data).expect("Failed to parse guest ELF");
    eprintln!(
        "Guest ELF: entry=0x{:x}, {} LOAD segments",
        elf.entry,
        elf.loads.len()
    );

    let region_size = page_align(GUEST_MEM_SIZE);
    let mem = alloc_pages(region_size);

    for load in &elf.loads {
        let offset = (load.vaddr - GUEST_BASE) as usize;
        assert!(
            offset + load.data.len() <= region_size,
            "LOAD segment exceeds guest memory"
        );
        unsafe {
            ptr::copy_nonoverlapping(load.data.as_ptr(), mem.add(offset), load.data.len());
        }
        eprintln!(
            "  Loaded: vaddr=0x{:x}, size={}, flags={}",
            load.vaddr,
            load.data.len(),
            load.flags_str()
        );
    }

    (mem, region_size)
}

/// Create a VM with memory mapped and a vCPU ready to run.
/// Returns (vcpu, exit_ptr). Caller must destroy the vcpu and VM.
fn create_vm_with_memory(
    mem: *mut u8,
    mem_size: usize,
    entry: u64,
) -> (u64, *const HvVcpuExit) {
    unsafe {
        check_hv(hvf::hv_vm_create(ptr::null()), "hv_vm_create");
        check_hv(
            hvf::hv_vm_map(
                mem,
                GUEST_BASE,
                mem_size,
                HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC,
            ),
            "hv_vm_map",
        );
    }

    let mut vcpu: u64 = 0;
    let mut exit_ptr: *const HvVcpuExit = ptr::null();
    unsafe {
        check_hv(
            hvf::hv_vcpu_create(&mut vcpu, &mut exit_ptr, ptr::null()),
            "hv_vcpu_create",
        );
        check_hv(hvf::hv_vcpu_set_reg(vcpu, HV_REG_PC, entry), "set PC");
        check_hv(
            hvf::hv_vcpu_set_reg(vcpu, HV_REG_CPSR, 0x3c5),
            "set CPSR",
        );
        check_hv(
            hvf::hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_SCTLR_EL1, 0x30d00800),
            "set SCTLR_EL1",
        );
        // Enable SIMD/FP at EL1: CPACR_EL1.FPEN = 0b11
        check_hv(
            hvf::hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_CPACR_EL1, 3 << 20),
            "set CPACR_EL1",
        );
        let stack_top = GUEST_BASE + mem_size as u64;
        check_hv(
            hvf::hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_SP_EL1, stack_top),
            "set SP_EL1",
        );
    }

    (vcpu, exit_ptr)
}

/// Destroy the vCPU and VM.
fn destroy_vm(vcpu: u64) {
    unsafe {
        check_hv(hvf::hv_vcpu_destroy(vcpu), "hv_vcpu_destroy");
        check_hv(hvf::hv_vm_destroy(), "hv_vm_destroy");
    }
}

/// Run the vCPU until HC_EXIT or HC_READY, dispatching hypercalls.
/// Returns the hypercall ID that terminated the loop (HC_EXIT or HC_READY)
/// and the exit code (x1 for HC_EXIT, 0 for HC_READY).
fn run_vcpu_loop(
    vcpu: u64,
    exit_ptr: *const HvVcpuExit,
    vm_state: &mut VmState,
    guest_mem: *mut u8,
    mem_size: usize,
) -> (u64, u64) {
    run_vcpu_loop_inner(vcpu, exit_ptr, vm_state, guest_mem, mem_size, false)
}

fn run_vcpu_loop_inner(
    vcpu: u64,
    exit_ptr: *const HvVcpuExit,
    vm_state: &mut VmState,
    guest_mem: *mut u8,
    mem_size: usize,
    quiet: bool,
) -> (u64, u64) {
    loop {
        unsafe {
            check_hv(hvf::hv_vcpu_run(vcpu), "hv_vcpu_run");
        }

        let exit = unsafe { &*exit_ptr };
        match exit.reason {
            HV_EXIT_REASON_EXCEPTION => {
                let ec = (exit.exception.syndrome >> 26) & 0x3f;
                if ec != 0x16 {
                    let pc = unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_PC) };
                    let lr = unsafe { hvf::vcpu_get_reg(vcpu, 30) }; // x30 = LR
                    let sp = unsafe { hvf::vcpu_get_sys_reg(vcpu, HV_SYS_REG_SP_EL1) };
                    let cpacr = unsafe { hvf::vcpu_get_sys_reg(vcpu, HV_SYS_REG_CPACR_EL1) };
                    panic!(
                        "Unexpected exception: EC=0x{:x}, syndrome=0x{:x}, PC=0x{:x}, IPA=0x{:x}, LR=0x{:x}, SP=0x{:x}, CPACR=0x{:x}",
                        ec, exit.exception.syndrome, pc, exit.exception.physical_address, lr, sp, cpacr
                    );
                }

                let x0 = unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_X0) };

                match x0 {
                    HC_CONSOLE => {
                        let ptr_gpa = unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_X1) };
                        let len = unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_X2) } as usize;

                        if !quiet {
                            if let Some(p) =
                                gpa_to_host_ptr(ptr_gpa, len, guest_mem, GUEST_BASE, mem_size)
                            {
                                let bytes = unsafe { std::slice::from_raw_parts(p, len) };
                                use std::io::Write;
                                std::io::stdout().write_all(bytes).ok();
                            } else {
                                eprintln!("HC_CONSOLE: invalid GPA 0x{:x} len={}", ptr_gpa, len);
                            }
                        }
                        unsafe {
                            check_hv(hvf::hv_vcpu_set_reg(vcpu, HV_REG_X0, 0), "set x0");
                        }
                    }

                    HC_TIME => unsafe {
                        check_hv(
                            hvf::hv_vcpu_set_reg(vcpu, HV_REG_X0, vm_state.virtual_time_ns),
                            "set x0 time",
                        );
                    },

                    HC_RANDOM => {
                        let value = vm_state.rng.next_u64();
                        unsafe {
                            check_hv(
                                hvf::hv_vcpu_set_reg(vcpu, HV_REG_X0, value),
                                "set x0 random",
                            );
                        }
                    }

                    HC_DB_READ => {
                        let req_len =
                            unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_X1) } as usize;
                        let mailbox_ptr = unsafe {
                            guest_mem.add(MAILBOX_OFFSET as usize)
                        };

                        // Read request from mailbox
                        let request = if req_len <= MAILBOX_SIZE {
                            unsafe {
                                std::slice::from_raw_parts(mailbox_ptr, req_len)
                            }
                        } else {
                            b"" as &[u8]
                        };
                        let request_str =
                            std::str::from_utf8(request).unwrap_or("");

                        if !quiet {
                            eprintln!("HC_DB_READ: {:?}", request_str);
                        }

                        // Generate stub response based on collection name
                        let response = match request_str {
                            "users" => r#"[{"id":1,"name":"Alice"},{"id":2,"name":"Bob"}]"#,
                            "posts" => r#"[{"id":1,"title":"Hello World","author":"Alice"}]"#,
                            _ => "[]",
                        };

                        // Write response to mailbox
                        let resp_bytes = response.as_bytes();
                        let resp_len = resp_bytes.len().min(MAILBOX_SIZE - 1);
                        unsafe {
                            ptr::copy_nonoverlapping(
                                resp_bytes.as_ptr(),
                                mailbox_ptr,
                                resp_len,
                            );
                            // Null-terminate
                            *mailbox_ptr.add(resp_len) = 0;
                            check_hv(
                                hvf::hv_vcpu_set_reg(vcpu, HV_REG_X0, resp_len as u64),
                                "set x0 db_read",
                            );
                        }
                    }

                    HC_READY => {
                        eprintln!("Guest signaled HC_READY (snapshot point)");
                        return (HC_READY, 0);
                    }

                    HC_EXIT => {
                        let exit_code = unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_X1) };
                        return (HC_EXIT, exit_code);
                    }

                    0xDE => {
                        // HC_EXCEPTION: guest EL1 exception handler fired
                        let esr = unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_X1) };
                        let far = unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_X2) };
                        let elr = unsafe { hvf::vcpu_get_reg(vcpu, hvf::HV_REG_X3) };
                        let ec = (esr >> 26) & 0x3f;
                        panic!(
                            "Guest EL1 exception: EC=0x{:x} ESR=0x{:x} FAR=0x{:x} ELR=0x{:x}",
                            ec, esr, far, elr
                        );
                    }

                    other => {
                        eprintln!("Unknown hypercall 0x{:x}", other);
                        unsafe {
                            check_hv(
                                hvf::hv_vcpu_set_reg(vcpu, HV_REG_X0, u64::MAX),
                                "set x0 err",
                            );
                        }
                    }
                }
            }

            other => {
                let pc = unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_PC) };
                let x0 = unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_X0) };
                let x1 = unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_X1) };
                let cpsr = unsafe { hvf::vcpu_get_reg(vcpu, HV_REG_CPSR) };
                let sp = unsafe { hvf::vcpu_get_sys_reg(vcpu, HV_SYS_REG_SP_EL1) };
                panic!("Unexpected VM exit: reason={}, PC=0x{:x}, x0=0x{:x}, x1=0x{:x}, CPSR=0x{:x}, SP=0x{:x}", other, pc, x0, x1, cpsr, sp);
            }
        }
    }
}

fn gpa_to_host_ptr(
    gpa: u64,
    len: usize,
    host_base: *mut u8,
    guest_base: u64,
    region_size: usize,
) -> Option<*const u8> {
    if gpa < guest_base {
        return None;
    }
    let offset = (gpa - guest_base) as usize;
    if offset + len > region_size {
        return None;
    }
    Some(unsafe { host_base.add(offset) })
}

// ── Commands ────────────────────────────────────────────────────────────────

/// Create a template: boot guest, run to HC_READY, snapshot.
fn cmd_snapshot(guest_elf_path: &Path, template_dir: &Path) {
    let (mem, mem_size) = load_guest_elf(guest_elf_path);

    let (vcpu, exit_ptr) = create_vm_with_memory(mem, mem_size, GUEST_BASE);
    let mut vm_state = VmState::new(0); // seed doesn't matter for snapshot phase

    let (hc, _) = run_vcpu_loop(vcpu, exit_ptr, &mut vm_state, mem, mem_size);
    assert_eq!(hc, HC_READY, "Guest didn't reach HC_READY");

    // Capture CPU state
    let cpu_state = unsafe { CpuState::capture(vcpu) };

    // Write guest memory to file
    let mem_path = template_dir.join("guest.mem");
    std::fs::create_dir_all(template_dir).expect("create template dir");
    let mem_bytes = unsafe { std::slice::from_raw_parts(mem, mem_size) };
    std::fs::write(&mem_path, mem_bytes).expect("write guest.mem");

    let template = Template {
        cpu_state,
        mem_path: mem_path.clone(),
        mem_size,
        guest_base: GUEST_BASE,
    };
    template.save(template_dir);

    destroy_vm(vcpu);
    unsafe { libc::munmap(mem as *mut libc::c_void, mem_size); }

    eprintln!(
        "Template saved to {}/ (mem={} bytes, state={} bytes)",
        template_dir.display(),
        mem_size,
        std::fs::metadata(template_dir.join("cpu.state"))
            .map(|m| m.len())
            .unwrap_or(0)
    );
}

fn cmd_fork(template_dir: &Path, seed: u64, mailbox_data: &[u8]) -> u64 {
    fork_inner(template_dir, seed, mailbox_data, false)
}

fn cmd_fork_quiet(template_dir: &Path, seed: u64, mailbox_data: &[u8]) -> u64 {
    fork_inner(template_dir, seed, mailbox_data, true)
}

/// Fork from a template: mmap CoW memory, restore CPU state, run to completion.
fn fork_inner(template_dir: &Path, seed: u64, mailbox_data: &[u8], quiet: bool) -> u64 {
    let template = Template::load(template_dir);
    let mem = template.mmap_cow_memory();
    let mem_size = template.mem_size;

    // Write mailbox data into the CoW memory
    let mailbox_host_offset = MAILBOX_OFFSET as usize;
    assert!(
        mailbox_data.len() < convex_shared::MAILBOX_SIZE,
        "mailbox data too large ({} > {})",
        mailbox_data.len(),
        convex_shared::MAILBOX_SIZE
    );
    unsafe {
        ptr::copy_nonoverlapping(
            mailbox_data.as_ptr(),
            mem.add(mailbox_host_offset),
            mailbox_data.len(),
        );
        // Null-terminate
        *mem.add(mailbox_host_offset + mailbox_data.len()) = 0;
    }

    // Create VM and restore CPU state
    unsafe {
        check_hv(hvf::hv_vm_create(ptr::null()), "hv_vm_create fork");
        check_hv(
            hvf::hv_vm_map(
                mem,
                GUEST_BASE,
                mem_size,
                HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC,
            ),
            "hv_vm_map fork",
        );
    }

    let mut vcpu: u64 = 0;
    let mut exit_ptr: *const HvVcpuExit = ptr::null();
    unsafe {
        check_hv(
            hvf::hv_vcpu_create(&mut vcpu, &mut exit_ptr, ptr::null()),
            "hv_vcpu_create fork",
        );
        template.cpu_state.restore(vcpu);
    }

    // Run to completion
    let mut vm_state = VmState::new(seed);
    let (hc, exit_code) = run_vcpu_loop_inner(vcpu, exit_ptr, &mut vm_state, mem, mem_size, quiet);
    assert_eq!(hc, HC_EXIT, "Forked VM didn't exit cleanly");

    destroy_vm(vcpu);
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
fn cmd_run(guest_elf_path: &Path, seed: u64) {
    let (mem, mem_size) = load_guest_elf(guest_elf_path);
    let (vcpu, exit_ptr) = create_vm_with_memory(mem, mem_size, GUEST_BASE);
    let mut vm_state = VmState::new(seed);

    eprintln!("VM created (seed={}). Running guest...\n", seed);

    let (hc, exit_code) = run_vcpu_loop(vcpu, exit_ptr, &mut vm_state, mem, mem_size);

    match hc {
        HC_EXIT => eprintln!("\nGuest exited with code {}", exit_code),
        HC_READY => eprintln!("\nGuest reached HC_READY (use 'snapshot' command to save)"),
        _ => unreachable!(),
    }

    destroy_vm(vcpu);
    unsafe { libc::munmap(mem as *mut libc::c_void, mem_size); }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage:");
        eprintln!("  convex-hypervisor run [--seed N] <guest.elf>");
        eprintln!("  convex-hypervisor snapshot <guest.elf> <template-dir>");
        eprintln!("  convex-hypervisor fork [--seed N] [--js 'code'] <template-dir>");
        eprintln!("  convex-hypervisor bench [--iterations N] [--js 'code'] <template-dir>");
        eprintln!("  convex-hypervisor boot-linux <kernel-image> [--initrd <initrd>]");
        eprintln!("  convex-hypervisor snapshot-linux <kernel-image> --initrd <initrd> <template-dir>");
        eprintln!("  convex-hypervisor fork-linux [--msg 'text'] <template-dir>");
        std::process::exit(1);
    }

    match args[1].as_str() {
        "boot-linux" => {
            let (kernel_path, initrd_path) = parse_boot_linux_args(&args[2..]);
            linux_boot::cmd_boot_linux(
                Path::new(&kernel_path),
                initrd_path.as_deref().map(Path::new),
            );
        }
        "snapshot-linux" => {
            let (kernel_path, initrd_path, template_dir) = parse_snapshot_linux_args(&args[2..]);
            linux_boot::cmd_snapshot_linux(
                Path::new(&kernel_path),
                initrd_path.as_deref().map(Path::new),
                Path::new(&template_dir),
            );
        }
        "fork-linux" => {
            let (msg, template_dir) = parse_fork_linux_args(&args[2..]);
            linux_boot::cmd_fork_linux(Path::new(&template_dir), msg.as_bytes());
        }
        "run" => {
            let (seed, guest_path) = parse_run_args(&args[2..]);
            cmd_run(Path::new(&guest_path), seed);
        }
        "snapshot" => {
            let guest_path = args.get(2).expect("missing guest ELF path");
            let template_dir = args.get(3).expect("missing template dir");
            cmd_snapshot(Path::new(guest_path), Path::new(template_dir));
        }
        "fork" => {
            let (seed, js_code, template_dir) = parse_fork_args(&args[2..]);
            let exit_code = cmd_fork(Path::new(&template_dir), seed, js_code.as_bytes());
            eprintln!("Guest exited with code {}", exit_code);
        }
        "bench" => {
            let (iterations, js_code, template_dir) = parse_bench_args(&args[2..]);
            cmd_bench(Path::new(&template_dir), iterations, &js_code);
        }
        // Legacy: if first positional arg is a file path, treat as `run`
        _ => {
            let (seed, guest_path) = parse_run_args(&args[1..]);
            cmd_run(Path::new(&guest_path), seed);
        }
    }
}

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

fn parse_boot_linux_args(args: &[String]) -> (String, Option<String>) {
    let mut kernel_path = String::new();
    let mut initrd_path: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--initrd" => {
                initrd_path = args.get(i + 1).cloned();
                i += 2;
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
    (kernel_path, initrd_path)
}

fn parse_snapshot_linux_args(args: &[String]) -> (String, Option<String>, String) {
    let mut kernel_path = String::new();
    let mut initrd_path: Option<String> = None;
    let mut template_dir = String::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--initrd" => {
                initrd_path = args.get(i + 1).cloned();
                i += 2;
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
    (kernel_path, initrd_path, template_dir)
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
            other => {
                template_dir = other.to_string();
                i += 1;
            }
        }
    }
    if template_dir.is_empty() {
        eprintln!("fork-linux requires: [--msg 'text'] <template-dir>");
        std::process::exit(1);
    }
    (msg, template_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gpa_to_host_ptr_valid() {
        let buf = [0u8; 4096];
        let host_base = buf.as_ptr() as *mut u8;
        let guest_base: u64 = 0x4000_0000;

        let p = gpa_to_host_ptr(guest_base, 1, host_base, guest_base, 4096);
        assert!(p.is_some());
        assert_eq!(p.unwrap(), host_base as *const u8);

        let p = gpa_to_host_ptr(guest_base + 100, 10, host_base, guest_base, 4096);
        assert!(p.is_some());

        let p = gpa_to_host_ptr(guest_base + 4096, 0, host_base, guest_base, 4096);
        assert!(p.is_some());
    }

    #[test]
    fn test_gpa_to_host_ptr_invalid() {
        let buf = [0u8; 4096];
        let host_base = buf.as_ptr() as *mut u8;
        let guest_base: u64 = 0x4000_0000;

        assert!(gpa_to_host_ptr(guest_base - 1, 1, host_base, guest_base, 4096).is_none());
        assert!(gpa_to_host_ptr(guest_base + 4096, 1, host_base, guest_base, 4096).is_none());
        assert!(gpa_to_host_ptr(guest_base + 4090, 10, host_base, guest_base, 4096).is_none());
    }

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
    fn test_hvf_exit_struct_layout() {
        assert_eq!(std::mem::size_of::<HvVcpuExit>(), 4 + 4 + 24);
        assert_eq!(std::mem::align_of::<HvVcpuExit>(), 8);
    }

    #[test]
    fn test_vm_state_determinism() {
        let mut s1 = VmState::new(42);
        let mut s2 = VmState::new(42);
        let mut s3 = VmState::new(99);

        for _ in 0..10 {
            assert_eq!(s1.rng.next_u64(), s2.rng.next_u64());
        }
        assert_eq!(s1.virtual_time_ns, s2.virtual_time_ns);

        let v1 = VmState::new(42).rng.next_u64();
        let v3 = s3.rng.next_u64();
        assert_ne!(v1, v3);
    }

    #[test]
    fn test_elf_parse_guest_binary() {
        let guest_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("guest/target/aarch64-unknown-none/release/convex-guest");
        if let Ok(data) = std::fs::read(&guest_path) {
            let elf = elf::GuestElf::parse(&data).expect("Failed to parse guest ELF");
            assert_eq!(elf.entry, 0x4000_0000);
            assert!(!elf.loads.is_empty());
            for load in &elf.loads {
                assert!(load.vaddr >= 0x4000_0000);
            }
        }
    }

    fn run_host_cmd(args: &[&str]) -> String {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap();
        let output = std::process::Command::new(workspace_root.join("target/debug/convex-hypervisor"))
            .args(args)
            .output()
            .expect("failed to run host binary");
        assert!(output.status.success(), "host failed: {:?}\nstderr: {}", output.status, String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).expect("non-utf8")
    }

    #[test]
    #[ignore = "requires pre-built+signed binaries: run `just snapshot` first"]
    fn test_fork_determinism() {
        let template_dir = "/tmp/hvf-template";
        if !std::path::Path::new(template_dir).join("cpu.state").exists() {
            panic!("Template not found. Run `just snapshot` first.");
        }

        let js = "console.log('hello ' + (1 + 2))";
        let out1 = run_host_cmd(&["fork", "--seed", "42", "--js", js, template_dir]);
        let out2 = run_host_cmd(&["fork", "--seed", "42", "--js", js, template_dir]);
        assert_eq!(out1, out2, "Same seed must produce identical output");

        let js_rand = "console.log(Math.random())";
        let out3 = run_host_cmd(&["fork", "--seed", "42", "--js", js_rand, template_dir]);
        let out4 = run_host_cmd(&["fork", "--seed", "99", "--js", js_rand, template_dir]);
        assert_ne!(out3, out4, "Different seed must produce different random output");
    }

    #[test]
    #[ignore = "requires pre-built+signed binaries: run `just snapshot` first"]
    fn test_js_console_log() {
        let template_dir = "/tmp/hvf-template";
        if !std::path::Path::new(template_dir).join("cpu.state").exists() {
            panic!("Template not found. Run `just snapshot` first.");
        }

        let out = run_host_cmd(&["fork", "--seed", "42", "--js", "console.log('hello ' + (1 + 2))", template_dir]);
        assert_eq!(out.trim(), "hello 3");
    }

    #[test]
    #[ignore = "requires pre-built+signed binaries: run `just snapshot` first"]
    fn test_js_date_now() {
        let template_dir = "/tmp/hvf-template";
        if !std::path::Path::new(template_dir).join("cpu.state").exists() {
            panic!("Template not found. Run `just snapshot` first.");
        }

        let out = run_host_cmd(&["fork", "--seed", "42", "--js", "console.log(Date.now())", template_dir]);
        // Virtual time is 1700000000000000000 ns = 1700000000000 ms
        assert_eq!(out.trim(), "1700000000000");
    }

    #[test]
    #[ignore = "requires pre-built+signed binaries: run `just snapshot` first"]
    fn test_js_empty_code() {
        let template_dir = "/tmp/hvf-template";
        if !std::path::Path::new(template_dir).join("cpu.state").exists() {
            panic!("Template not found. Run `just snapshot` first.");
        }

        let out = run_host_cmd(&["fork", "--seed", "42", template_dir]);
        assert_eq!(out.trim(), "No JS code in mailbox.");
    }

    #[test]
    #[ignore = "requires pre-built+signed binaries: run `just snapshot` first"]
    fn test_db_query_users() {
        let template_dir = "/tmp/hvf-template";
        if !std::path::Path::new(template_dir).join("cpu.state").exists() {
            panic!("Template not found. Run `just snapshot` first.");
        }

        let js = r#"var u = db.query("users"); console.log(JSON.stringify(u))"#;
        let out = run_host_cmd(&["fork", "--seed", "42", "--js", js, template_dir]);
        assert_eq!(
            out.trim(),
            r#"[{"id":1,"name":"Alice"},{"id":2,"name":"Bob"}]"#
        );
    }

    #[test]
    #[ignore = "requires pre-built+signed binaries: run `just snapshot` first"]
    fn test_db_query_unknown_collection() {
        let template_dir = "/tmp/hvf-template";
        if !std::path::Path::new(template_dir).join("cpu.state").exists() {
            panic!("Template not found. Run `just snapshot` first.");
        }

        let js = r#"var x = db.query("nonexistent"); console.log(x.length)"#;
        let out = run_host_cmd(&["fork", "--seed", "42", "--js", js, template_dir]);
        assert_eq!(out.trim(), "0");
    }

    #[test]
    #[ignore = "requires pre-built+signed binaries: run `just snapshot` first"]
    fn test_db_query_determinism() {
        let template_dir = "/tmp/hvf-template";
        if !std::path::Path::new(template_dir).join("cpu.state").exists() {
            panic!("Template not found. Run `just snapshot` first.");
        }

        // db.query + Math.random should be deterministic with same seed
        let js = r#"var u = db.query("users"); console.log(u[0].name, Math.random())"#;
        let out1 = run_host_cmd(&["fork", "--seed", "42", "--js", js, template_dir]);
        let out2 = run_host_cmd(&["fork", "--seed", "42", "--js", js, template_dir]);
        assert_eq!(out1, out2, "Same seed must produce identical db+random output");
    }

    #[test]
    fn test_cpu_state_roundtrip() {
        let state = CpuState {
            gpr: {
                let mut g = [0u64; 35];
                for i in 0..35 {
                    g[i] = (i as u64) * 0x1111;
                }
                g
            },
            sys_regs: vec![0xAAAA, 0xBBBB, 0xCCCC],
            simd: {
                let mut s = [[0u8; 16]; 32];
                s[0] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
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
