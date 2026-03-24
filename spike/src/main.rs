//! HVF spike test — validates core assumptions about Hypervisor.framework on aarch64 macOS.
//!
//! Tests:
//! 1. Basic HVC trap: guest executes HVC #0, host sees exception exit with EC=0x16
//! 2. CoW fork semantics: MAP_PRIVATE file mappings give independent copies per VM
//! 3. VM creation latency benchmark
//! 4. Concurrent VM limits

use std::io::Write;
use std::ptr;
use std::time::Instant;

// ── HVF FFI bindings (hand-written, arm64 only) ─────────────────────────────

type HvReturn = i32;
type HvVcpu = u64;
type HvIpa = u64;
type HvMemoryFlags = u64;

const HV_SUCCESS: HvReturn = 0;

const HV_MEMORY_READ: HvMemoryFlags = 1 << 0;
const HV_MEMORY_WRITE: HvMemoryFlags = 1 << 1;
const HV_MEMORY_EXEC: HvMemoryFlags = 1 << 2;

// Exit reasons
const HV_EXIT_REASON_CANCELED: u32 = 0;
const HV_EXIT_REASON_EXCEPTION: u32 = 1;
const HV_EXIT_REASON_VTIMER_ACTIVATED: u32 = 2;
const HV_EXIT_REASON_UNKNOWN: u32 = 3;

// Register IDs (sequential enum starting at 0)
const HV_REG_X0: u32 = 0;
const HV_REG_X1: u32 = 1;
const HV_REG_X2: u32 = 2;
const HV_REG_PC: u32 = 31; // After x0-x30
const HV_REG_CPSR: u32 = 34; // After x0-x30, PC, FPCR, FPSR

// System register IDs (from hv_vcpu_types.h)
const HV_SYS_REG_SCTLR_EL1: u16 = 0xc080;
const HV_SYS_REG_SP_EL1: u16 = 0xe208;

// Exit exception info
#[repr(C)]
struct HvVcpuExitException {
    syndrome: u64,
    virtual_address: u64,
    physical_address: u64,
}

#[repr(C)]
struct HvVcpuExit {
    reason: u32,
    exception: HvVcpuExitException,
}

// Opaque config types — we pass NULL
type HvVmConfig = *const std::ffi::c_void;
type HvVcpuConfig = *const std::ffi::c_void;

#[link(name = "Hypervisor", kind = "framework")]
extern "C" {
    fn hv_vm_create(config: HvVmConfig) -> HvReturn;
    fn hv_vm_destroy() -> HvReturn;
    fn hv_vm_map(addr: *mut u8, ipa: HvIpa, size: usize, flags: HvMemoryFlags) -> HvReturn;
    fn hv_vm_unmap(ipa: HvIpa, size: usize) -> HvReturn;
    fn hv_vcpu_create(
        vcpu: *mut HvVcpu,
        exit: *mut *const HvVcpuExit,
        config: HvVcpuConfig,
    ) -> HvReturn;
    fn hv_vcpu_destroy(vcpu: HvVcpu) -> HvReturn;
    fn hv_vcpu_run(vcpu: HvVcpu) -> HvReturn;
    fn hv_vcpu_get_reg(vcpu: HvVcpu, reg: u32, value: *mut u64) -> HvReturn;
    fn hv_vcpu_set_reg(vcpu: HvVcpu, reg: u32, value: u64) -> HvReturn;
    fn hv_vcpu_get_sys_reg(vcpu: HvVcpu, reg: u16, value: *mut u64) -> HvReturn;
    fn hv_vcpu_set_sys_reg(vcpu: HvVcpu, reg: u16, value: u64) -> HvReturn;
}

fn hv_result_name(ret: HvReturn) -> &'static str {
    match ret {
        0 => "HV_SUCCESS",
        // The error codes have high bits set: 0xfae94001 etc.
        // But as i32 they wrap. Let's just show the hex.
        _ => "HV_ERROR",
    }
}

fn check_hv(ret: HvReturn, context: &str) {
    if ret != HV_SUCCESS {
        panic!(
            "{}: returned 0x{:x} ({})",
            context,
            ret as u32,
            hv_result_name(ret)
        );
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

const PAGE_SIZE: usize = 16384; // Apple Silicon uses 16 KiB pages

/// Allocate page-aligned anonymous memory
fn alloc_guest_mem(size: usize) -> *mut u8 {
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

/// Create a vCPU, set up minimal bare-metal EL1 state, return (vcpu_id, exit_ptr)
fn create_vcpu_with_state(
    code_base_ipa: u64,
    entry_offset: u64,
    stack_top_ipa: u64,
) -> (HvVcpu, *const HvVcpuExit) {
    let mut vcpu: HvVcpu = 0;
    let mut exit_ptr: *const HvVcpuExit = ptr::null();

    unsafe {
        check_hv(
            hv_vcpu_create(&mut vcpu, &mut exit_ptr, ptr::null()),
            "hv_vcpu_create",
        );

        // PC = entry point
        check_hv(
            hv_vcpu_set_reg(vcpu, HV_REG_PC, code_base_ipa + entry_offset),
            "set PC",
        );

        // SP_EL1 = top of stack (stack grows downward)
        check_hv(
            hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_SP_EL1, stack_top_ipa),
            "set SP_EL1",
        );

        // CPSR = EL1h (SPSel=1), all DAIF masked (no interrupts)
        // EL1h = 0b0101 = 0x5, DAIF bits [9:6] all set = 0x3c0
        // Combined: 0x3c5 (but usually written as 0x3c4 for EL1h)
        // Actually: PSTATE M[3:0] = 0b0101 for EL1h = 5, DAIF = 0xf << 6 = 0x3c0
        // 0x3c0 | 0x5 = 0x3c5
        check_hv(hv_vcpu_set_reg(vcpu, HV_REG_CPSR, 0x3c4), "set CPSR");

        // SCTLR_EL1: MMU off, caches off
        check_hv(
            hv_vcpu_set_sys_reg(vcpu, HV_SYS_REG_SCTLR_EL1, 0x30d00800),
            "set SCTLR_EL1",
        );
    }

    (vcpu, exit_ptr)
}

// ── Test 1: Basic HVC trap ──────────────────────────────────────────────────

fn test_basic_hvc_trap() {
    println!("=== Test 1: Basic HVC trap ===");

    unsafe {
        check_hv(hv_vm_create(ptr::null()), "hv_vm_create");
    }

    // Allocate one page for code
    let code_size = PAGE_SIZE;
    let code_mem = alloc_guest_mem(code_size);
    let code_ipa: u64 = 0x4000_0000;

    // Write guest "program":
    // 0x00: MOV X0, #0x42       → movz x0, #0x42          → 0xd2800840
    // 0x04: HVC #0              → 0xd4000002
    // 0x08: MOV X0, #0xFF       → movz x0, #0xff          → 0xd2801fe0  (HC_EXIT)
    // 0x0c: HVC #0              → 0xd4000002
    let guest_code: [u32; 4] = [
        0xd2800840, // movz x0, #0x42
        0xd4000002, // hvc #0
        0xd2801fe0, // movz x0, #0xff  (HC_EXIT = 0xff)
        0xd4000002, // hvc #0
    ];

    unsafe {
        ptr::copy_nonoverlapping(
            guest_code.as_ptr() as *const u8,
            code_mem,
            guest_code.len() * 4,
        );
    }

    // Map code into guest
    unsafe {
        check_hv(
            hv_vm_map(
                code_mem,
                code_ipa,
                code_size as usize,
                HV_MEMORY_READ | HV_MEMORY_EXEC,
            ),
            "hv_vm_map code",
        );
    }

    // Allocate and map a stack page
    let stack_size = PAGE_SIZE;
    let stack_mem = alloc_guest_mem(stack_size);
    let stack_ipa: u64 = 0x4001_0000;

    unsafe {
        check_hv(
            hv_vm_map(
                stack_mem,
                stack_ipa,
                stack_size as usize,
                HV_MEMORY_READ | HV_MEMORY_WRITE,
            ),
            "hv_vm_map stack",
        );
    }

    let (vcpu, exit_ptr) = create_vcpu_with_state(code_ipa, 0, stack_ipa + stack_size as u64);

    // Run the vCPU
    unsafe {
        check_hv(hv_vcpu_run(vcpu), "hv_vcpu_run");

        let exit = &*exit_ptr;
        println!(
            "  Exit reason: {} (expected {}=EXCEPTION)",
            exit.reason, HV_EXIT_REASON_EXCEPTION
        );
        assert_eq!(
            exit.reason, HV_EXIT_REASON_EXCEPTION,
            "Expected EXCEPTION exit"
        );

        // ESR_EL2 syndrome: EC is bits [31:26]
        let syndrome = exit.exception.syndrome;
        let ec = (syndrome >> 26) & 0x3f;
        println!("  ESR syndrome: 0x{:08x}", syndrome);
        println!(
            "  Exception Class (EC): 0x{:02x} (expected 0x16=HVC from AArch64)",
            ec
        );
        assert_eq!(ec, 0x16, "Expected EC=0x16 (HVC from AArch64)");

        // Check x0 has our hypercall ID
        let mut x0: u64 = 0;
        check_hv(hv_vcpu_get_reg(vcpu, HV_REG_X0, &mut x0), "get x0");
        println!("  x0 (hypercall ID): 0x{:x} (expected 0x42)", x0);
        assert_eq!(x0, 0x42, "Expected x0=0x42");

        // Check PC — HVF sets PC to the instruction AFTER the HVC (ELR_EL2 = HVC + 4)
        // So we do NOT need to manually advance PC.
        let mut pc: u64 = 0;
        check_hv(hv_vcpu_get_reg(vcpu, HV_REG_PC, &mut pc), "get PC");
        println!(
            "  PC at exit: 0x{:x} (should be HVC+4 = 0x{:x})",
            pc,
            code_ipa + 8
        );
        assert_eq!(pc, code_ipa + 8, "PC should already point past HVC");

        // Set return value in x0
        check_hv(hv_vcpu_set_reg(vcpu, HV_REG_X0, 0xBEEF), "set x0 return");

        // Run again — should hit the second HVC (HC_EXIT)
        check_hv(hv_vcpu_run(vcpu), "hv_vcpu_run #2");
        let exit2 = &*exit_ptr;
        assert_eq!(exit2.reason, HV_EXIT_REASON_EXCEPTION);
        let ec2 = (exit2.exception.syndrome >> 26) & 0x3f;
        assert_eq!(ec2, 0x16);

        let mut x0_2: u64 = 0;
        check_hv(hv_vcpu_get_reg(vcpu, HV_REG_X0, &mut x0_2), "get x0 #2");
        println!("  Second HVC x0: 0x{:x} (expected 0xff=HC_EXIT)", x0_2);
        assert_eq!(x0_2, 0xFF);

        // Clean up
        check_hv(hv_vcpu_destroy(vcpu), "hv_vcpu_destroy");
        check_hv(hv_vm_destroy(), "hv_vm_destroy");
    }

    // Unmap host memory
    unsafe {
        libc::munmap(code_mem as *mut libc::c_void, code_size);
        libc::munmap(stack_mem as *mut libc::c_void, stack_size);
    }

    println!("  ✓ Basic HVC trap works!\n");
}

// ── Test 2: CoW fork semantics ──────────────────────────────────────────────

fn test_cow_fork() {
    println!("=== Test 2: CoW fork semantics (MAP_PRIVATE) ===");

    // Create a temp file with known content
    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join("hvf_cow_test.bin");
    let file_size = PAGE_SIZE * 4; // 4 pages

    {
        let mut f = std::fs::File::create(&tmp_path).expect("create temp file");
        let mut buf = vec![0xAA_u8; file_size];
        // Write a known pattern at a specific offset (page 2, offset 0)
        let marker_offset = PAGE_SIZE * 2;
        buf[marker_offset..marker_offset + 8]
            .copy_from_slice(&0xDEAD_BEEF_CAFE_BABEu64.to_le_bytes());
        f.write_all(&buf).expect("write temp file");
    }

    // Guest program: read from a known offset in the data region, write a new
    // value there, then HVC to report what it read.
    //
    // The data region will be mapped at GPA 0x5000_0000.
    // The marker is at offset PAGE_SIZE*2 = 0x8000 (for 16K pages) within the data region.
    // So the marker GPA = 0x5000_0000 + 0x8000 = 0x5000_8000.
    //
    // Guest code:
    //   LDR X1, =0x5000_8000      // address of marker
    //   LDR X2, [X1]              // read current value → x2
    //   MOV X3, #0xBEEF
    //   STR X3, [X1]              // write new value
    //   MOV X0, #0x01             // HC_CONSOLE (just to signal "done reading")
    //   HVC #0
    //
    // We'll encode this as raw instructions. For the address load, use MOVZ+MOVK.

    let data_ipa: u64 = 0x5000_0000;
    let marker_offset = PAGE_SIZE * 2;
    let marker_ipa: u64 = data_ipa + marker_offset as u64;

    // Encode: load marker_ipa into X1
    // MOVZ X1, #(marker_ipa & 0xFFFF), LSL #0
    // MOVK X1, #((marker_ipa >> 16) & 0xFFFF), LSL #16
    // MOVK X1, #((marker_ipa >> 32) & 0xFFFF), LSL #32
    let marker_lo = (marker_ipa & 0xFFFF) as u32;
    let marker_hi = ((marker_ipa >> 16) & 0xFFFF) as u32;
    // marker_ipa = 0x5000_8000, so bits [32:48] = 0
    let guest_code: [u32; 7] = [
        0xd2800001 | (marker_lo << 5), // MOVZ X1, #marker_lo
        0xf2a00001 | (marker_hi << 5), // MOVK X1, #marker_hi, LSL #16
        0xf9400022,                    // LDR X2, [X1]
        0xd2800003 | (0xBEEF << 5),    // MOVZ X3, #0xBEEF
        0xf9000023,                    // STR X3, [X1]
        0xd2800000 | (1 << 5),         // MOVZ X0, #1  (HC_CONSOLE)
        0xd4000002,                    // HVC #0
    ];

    let code_size = PAGE_SIZE;
    let code_mem = alloc_guest_mem(code_size);
    let code_ipa: u64 = 0x4000_0000;

    unsafe {
        ptr::copy_nonoverlapping(
            guest_code.as_ptr() as *const u8,
            code_mem,
            guest_code.len() * 4,
        );
    }

    let stack_size = PAGE_SIZE;
    let stack_mem = alloc_guest_mem(stack_size);
    let stack_ipa: u64 = 0x4001_0000;

    // ─── VM 1: mmap the file MAP_PRIVATE, run guest that reads+writes ────

    let fd = unsafe {
        let c_path = std::ffi::CString::new(tmp_path.to_str().unwrap()).unwrap();
        libc::open(c_path.as_ptr(), libc::O_RDONLY)
    };
    assert!(fd >= 0, "open temp file failed");

    let data_mem_1 = unsafe {
        let ptr = libc::mmap(
            ptr::null_mut(),
            file_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
            fd,
            0,
        );
        assert_ne!(ptr, libc::MAP_FAILED, "mmap MAP_PRIVATE #1 failed");
        ptr as *mut u8
    };

    // Verify the marker is present in the mapping
    let marker_val = unsafe {
        let p = data_mem_1.add(marker_offset) as *const u64;
        *p
    };
    println!(
        "  Marker value in mmap #1 before guest: 0x{:016x} (expected 0xDEADBEEFCAFEBABE)",
        marker_val
    );
    assert_eq!(marker_val, 0xDEAD_BEEF_CAFE_BABE);

    // Create VM 1
    unsafe {
        check_hv(hv_vm_create(ptr::null()), "hv_vm_create #1");
    }

    unsafe {
        check_hv(
            hv_vm_map(
                code_mem,
                code_ipa,
                code_size,
                HV_MEMORY_READ | HV_MEMORY_EXEC,
            ),
            "hv_vm_map code #1",
        );
        check_hv(
            hv_vm_map(
                stack_mem,
                stack_ipa,
                stack_size,
                HV_MEMORY_READ | HV_MEMORY_WRITE,
            ),
            "hv_vm_map stack #1",
        );
        check_hv(
            hv_vm_map(
                data_mem_1,
                data_ipa,
                file_size,
                HV_MEMORY_READ | HV_MEMORY_WRITE,
            ),
            "hv_vm_map data #1",
        );
    }

    let (vcpu1, exit_ptr1) = create_vcpu_with_state(code_ipa, 0, stack_ipa + stack_size as u64);

    unsafe {
        check_hv(hv_vcpu_run(vcpu1), "hv_vcpu_run VM1");
        let exit = &*exit_ptr1;
        assert_eq!(
            exit.reason, HV_EXIT_REASON_EXCEPTION,
            "VM1: expected EXCEPTION"
        );
        let ec = (exit.exception.syndrome >> 26) & 0x3f;
        assert_eq!(ec, 0x16, "VM1: expected HVC");

        // x2 should have the original marker value
        let mut x2: u64 = 0;
        check_hv(hv_vcpu_get_reg(vcpu1, HV_REG_X2, &mut x2), "get x2 VM1");
        println!(
            "  VM1 read marker (x2): 0x{:016x} (expected 0xDEADBEEFCAFEBABE)",
            x2
        );
        assert_eq!(x2, 0xDEAD_BEEF_CAFE_BABE);

        // Verify the write landed in host mapping (CoW page now)
        let written_val = *(data_mem_1.add(marker_offset) as *const u64);
        println!("  VM1 wrote 0xBEEF, host mmap reads: 0x{:x}", written_val);
        assert_eq!(written_val, 0xBEEF);

        check_hv(hv_vcpu_destroy(vcpu1), "hv_vcpu_destroy #1");
        check_hv(hv_vm_destroy(), "hv_vm_destroy #1");
    }

    // ─── VM 2: fresh MAP_PRIVATE of the same file ────

    let data_mem_2 = unsafe {
        let ptr = libc::mmap(
            ptr::null_mut(),
            file_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
            fd,
            0,
        );
        assert_ne!(ptr, libc::MAP_FAILED, "mmap MAP_PRIVATE #2 failed");
        ptr as *mut u8
    };

    // VM2's mapping should NOT see VM1's write
    let marker_val_2 = unsafe { *(data_mem_2.add(marker_offset) as *const u64) };
    println!(
        "  VM2 mmap marker (before guest): 0x{:016x} (expected original 0xDEADBEEFCAFEBABE)",
        marker_val_2
    );
    assert_eq!(
        marker_val_2, 0xDEAD_BEEF_CAFE_BABE,
        "CoW isolation failed: VM2 sees VM1's write!"
    );

    unsafe {
        check_hv(hv_vm_create(ptr::null()), "hv_vm_create #2");
    }

    unsafe {
        check_hv(
            hv_vm_map(
                code_mem,
                code_ipa,
                code_size,
                HV_MEMORY_READ | HV_MEMORY_EXEC,
            ),
            "hv_vm_map code #2",
        );
        check_hv(
            hv_vm_map(
                stack_mem,
                stack_ipa,
                stack_size,
                HV_MEMORY_READ | HV_MEMORY_WRITE,
            ),
            "hv_vm_map stack #2",
        );
        check_hv(
            hv_vm_map(
                data_mem_2,
                data_ipa,
                file_size,
                HV_MEMORY_READ | HV_MEMORY_WRITE,
            ),
            "hv_vm_map data #2",
        );
    }

    let (vcpu2, exit_ptr2) = create_vcpu_with_state(code_ipa, 0, stack_ipa + stack_size as u64);

    unsafe {
        check_hv(hv_vcpu_run(vcpu2), "hv_vcpu_run VM2");
        let exit = &*exit_ptr2;
        assert_eq!(exit.reason, HV_EXIT_REASON_EXCEPTION);

        let mut x2: u64 = 0;
        check_hv(hv_vcpu_get_reg(vcpu2, HV_REG_X2, &mut x2), "get x2 VM2");
        println!(
            "  VM2 read marker (x2): 0x{:016x} (expected original, NOT 0xBEEF)",
            x2
        );
        assert_eq!(x2, 0xDEAD_BEEF_CAFE_BABE, "CoW isolation failed in guest!");

        check_hv(hv_vcpu_destroy(vcpu2), "hv_vcpu_destroy #2");
        check_hv(hv_vm_destroy(), "hv_vm_destroy #2");
    }

    // ─── Verify original file is unmodified ────
    let file_contents = std::fs::read(&tmp_path).expect("read temp file");
    let file_marker = u64::from_le_bytes(
        file_contents[marker_offset..marker_offset + 8]
            .try_into()
            .unwrap(),
    );
    println!(
        "  Original file marker: 0x{:016x} (expected 0xDEADBEEFCAFEBABE)",
        file_marker
    );
    assert_eq!(
        file_marker, 0xDEAD_BEEF_CAFE_BABE,
        "MAP_PRIVATE leaked writes to file!"
    );

    // Cleanup
    unsafe {
        libc::munmap(data_mem_1 as *mut libc::c_void, file_size);
        libc::munmap(data_mem_2 as *mut libc::c_void, file_size);
        libc::munmap(code_mem as *mut libc::c_void, code_size);
        libc::munmap(stack_mem as *mut libc::c_void, stack_size);
        libc::close(fd);
    }
    let _ = std::fs::remove_file(&tmp_path);

    println!("  ✓ CoW fork semantics work!\n");
}

// ── Test 3: VM creation latency benchmark ───────────────────────────────────

fn test_vm_creation_latency() {
    println!("=== Test 3: VM creation latency benchmark ===");

    let iterations = 1000;
    let mut create_times = Vec::with_capacity(iterations);
    let mut total_times = Vec::with_capacity(iterations);

    // Pre-allocate code + stack memory (shared across iterations)
    let code_size = PAGE_SIZE;
    let code_mem = alloc_guest_mem(code_size);
    let code_ipa: u64 = 0x4000_0000;

    // Write a single HVC #0 instruction
    let hvc_insn: u32 = 0xd4000002;
    unsafe {
        ptr::copy_nonoverlapping(&hvc_insn as *const u32 as *const u8, code_mem, 4);
    }

    let stack_size = PAGE_SIZE;
    let stack_mem = alloc_guest_mem(stack_size);
    let stack_ipa: u64 = 0x4001_0000;

    for i in 0..iterations {
        let t_start = Instant::now();

        unsafe {
            check_hv(hv_vm_create(ptr::null()), &format!("vm_create iter {}", i));
        }

        let t_vm_created = Instant::now();

        unsafe {
            check_hv(
                hv_vm_map(
                    code_mem,
                    code_ipa,
                    code_size,
                    HV_MEMORY_READ | HV_MEMORY_EXEC,
                ),
                "map code",
            );
            check_hv(
                hv_vm_map(
                    stack_mem,
                    stack_ipa,
                    stack_size,
                    HV_MEMORY_READ | HV_MEMORY_WRITE,
                ),
                "map stack",
            );
        }

        let (vcpu, _exit_ptr) = create_vcpu_with_state(code_ipa, 0, stack_ipa + stack_size as u64);

        let t_total = Instant::now();

        create_times.push(t_vm_created - t_start);
        total_times.push(t_total - t_start);

        unsafe {
            check_hv(hv_vcpu_destroy(vcpu), "vcpu destroy");
            check_hv(hv_vm_destroy(), "vm destroy");
        }
    }

    // Sort for percentile calculation
    create_times.sort();
    total_times.sort();

    let create_p50 = create_times[iterations / 2];
    let create_p99 = create_times[iterations * 99 / 100];
    let total_p50 = total_times[iterations / 2];
    let total_p99 = total_times[iterations * 99 / 100];

    println!("  hv_vm_create only:");
    println!("    p50: {:?}", create_p50);
    println!("    p99: {:?}", create_p99);
    println!("    min: {:?}", create_times[0]);
    println!("    max: {:?}", create_times[iterations - 1]);
    println!("  Full setup (vm_create + vm_map + vcpu_create + reg setup):");
    println!("    p50: {:?}", total_p50);
    println!("    p99: {:?}", total_p99);
    println!("    min: {:?}", total_times[0]);
    println!("    max: {:?}", total_times[iterations - 1]);

    let target_us = 500;
    if total_p50.as_micros() > target_us {
        println!(
            "  ⚠ WARNING: p50 total setup ({:?}) exceeds {}μs target!",
            total_p50, target_us
        );
    } else {
        println!(
            "  ✓ p50 total setup ({:?}) is under {}μs target",
            total_p50, target_us
        );
    }

    unsafe {
        libc::munmap(code_mem as *mut libc::c_void, code_size);
        libc::munmap(stack_mem as *mut libc::c_void, stack_size);
    }

    println!();
}

// ── Test 4: Concurrent VM limits ────────────────────────────────────────────

fn test_concurrent_vm_limits() {
    println!("=== Test 4: Concurrent VM limits ===");

    // Test 4a: Sequential VM create/destroy cycles
    println!("  Testing sequential VM create/destroy cycles...");
    let cycle_count = 100;
    let start = Instant::now();
    for i in 0..cycle_count {
        let ret = unsafe { hv_vm_create(ptr::null()) };
        if ret != HV_SUCCESS {
            println!("  VM create failed at iteration {}: 0x{:x}", i, ret as u32);
            break;
        }
        unsafe {
            check_hv(hv_vm_destroy(), &format!("vm_destroy cycle {}", i));
        }
    }
    let elapsed = start.elapsed();
    println!(
        "  {} VM create/destroy cycles in {:?} ({:.1}μs/cycle)",
        cycle_count,
        elapsed,
        elapsed.as_micros() as f64 / cycle_count as f64
    );

    // Test 4b: Confirm only one VM per process
    println!("\n  Testing single-VM-per-process constraint...");
    unsafe {
        check_hv(hv_vm_create(ptr::null()), "hv_vm_create main");
    }

    let ret2 = unsafe { hv_vm_create(ptr::null()) };
    println!(
        "  Second hv_vm_create returned: 0x{:x} (expected non-zero/HV_BUSY)",
        ret2 as u32
    );
    if ret2 != HV_SUCCESS {
        println!("  ✓ Confirmed: only one VM per process");
    } else {
        println!("  ⚠ Unexpected: second VM creation succeeded!");
        unsafe {
            let _ = hv_vm_destroy();
        }
    }

    // Test 4c: Max vCPUs within the single VM (one per thread, using park/unpark)
    println!("\n  Testing max vCPU count across threads...");

    let max_to_try: usize = 500;
    let max_vcpu_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let first_err = std::sync::Arc::new(std::sync::Mutex::new(None::<(usize, i32)>));

    let mut thread_handles = Vec::new();
    let (tx, rx) = std::sync::mpsc::channel::<bool>();

    for i in 0..max_to_try {
        let mc = max_vcpu_count.clone();
        let fe = first_err.clone();
        let tx = tx.clone();

        let h = std::thread::spawn(move || {
            let mut vcpu: HvVcpu = 0;
            let mut exit_ptr: *const HvVcpuExit = ptr::null();

            let ret = unsafe { hv_vcpu_create(&mut vcpu, &mut exit_ptr, ptr::null()) };

            if ret == HV_SUCCESS {
                mc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tx.send(true).ok();
                // Park until main thread unparks us for cleanup
                std::thread::park();
                unsafe {
                    let _ = hv_vcpu_destroy(vcpu);
                }
            } else {
                let mut guard = fe.lock().unwrap();
                if guard.is_none() {
                    *guard = Some((i, ret));
                }
                tx.send(false).ok();
            }
        });

        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(true) => {
                thread_handles.push(h);
            }
            Ok(false) => {
                let _ = h.join();
                break;
            }
            Err(_) => {
                println!("  Timeout waiting for vCPU {} creation", i);
                break;
            }
        }
    }

    let total_vcpus = max_vcpu_count.load(std::sync::atomic::Ordering::SeqCst);
    let err_info = first_err.lock().unwrap().clone();
    println!("  Max vCPUs created concurrently: {}", total_vcpus);
    if let Some((idx, code)) = err_info {
        println!("  First failure at vCPU {}: error 0x{:x}", idx, code as u32);
    }

    // Unpark all threads to clean up
    for h in thread_handles {
        h.thread().unpark();
        let _ = h.join();
    }

    unsafe {
        check_hv(hv_vm_destroy(), "final vm_destroy");
    }

    println!("  ✓ Concurrent VM limits test complete\n");
}

// ── Main ────────────────────────────────────────────────────────────────────

fn main() {
    println!("╔══════════════════════════════════════════════╗");
    println!("║  HVF Spike Tests — aarch64 macOS            ║");
    println!("╚══════════════════════════════════════════════╝\n");

    test_basic_hvc_trap();
    test_cow_fork();
    test_vm_creation_latency();
    test_concurrent_vm_limits();

    println!("All tests passed! 🎉");
}
