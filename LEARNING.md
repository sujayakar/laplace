# Learning Plan: Convex Hypervisor Codebase

## Prerequisites

Before reading code, understand these concepts:
- **ARM64 exception levels**: EL0 (user), EL1 (kernel), EL2 (hypervisor). Our guest runs at EL1, Apple's Hypervisor.framework runs at EL2.
- **HVC instruction**: `HVC #0` traps from EL1 to EL2, causing a VM exit. This is how the guest talks to the host.
- **Copy-on-Write (CoW)**: `mmap(MAP_PRIVATE)` on a file gives you a private copy — writes go to new pages, the file is unchanged. This is how we fork VMs cheaply.
- **no_std Rust**: Rust without the standard library — no heap, no threads, no I/O. The guest runs in this mode because there's no OS.

## Reading Order

### Phase 1: The Contract (30 min)

Start with what the guest and host agree on.

**1. `shared/src/lib.rs` (43 lines)**
This is the protocol definition — hypercall IDs, memory layout, control block. Read every line. Ask yourself:
- Where does guest code live in memory? Where's the heap? The stack? The mailbox?
- What does each hypercall do?
- How does the host pass configuration (heap limit) to the guest?

**2. `CLAUDE.md` — "Hypercall protocol" and "Guest memory layout" sections**
These sections document the design intent behind the constants in shared/lib.rs.

### Phase 2: The Hypervisor Shell (1 hour)

Now understand how the host creates and runs a VM.

**3. `host/src/hvf.rs` (167 lines)**
Raw FFI bindings to Apple's Hypervisor.framework. Key things to understand:
- What's an `HvVcpu`? (just a u64 handle)
- What's `HvVcpuExit`? (the struct HVF fills in when the VM exits)
- The ~20 functions in the `extern "C"` block — these are the entire HVF API we use
- `SNAPSHOT_SYS_REGS` — the system registers we save/restore for snapshots
- `vcpu_get_reg` / `vcpu_get_sys_reg` — convenience wrappers

**4. `host/src/elf.rs` (66 lines)**
Parses the guest ELF binary using the `goblin` crate. Simple — just extracts PT_LOAD segments (code, rodata, data) and the entry point address.

**5. `host/src/main.rs` — top half (lines 1-170)**
Read these functions in order:
- `VmState` — per-VM deterministic state (frozen time + seeded PRNG)
- `alloc_pages` — allocates page-aligned memory via `mmap(MAP_ANON)`
- `load_guest_elf` — loads ELF segments into a flat memory region
- `create_vm_with_memory` — the key function: creates a VM, maps memory, creates a vCPU, sets initial register state. Pay close attention to CPSR (0x3c5 = EL1h), SCTLR_EL1 (MMU off), CPACR_EL1 (SIMD enabled), and SP_EL1 (stack pointer)
- `destroy_vm` — tears everything down

### Phase 3: The Run Loop (45 min)

This is the heart of the system — the hypercall dispatch loop.

**6. `host/src/main.rs` — `run_vcpu_loop_inner` (lines 174-350)**
The host calls `hv_vcpu_run()` in a loop. Each iteration:
1. Check timeout deadline
2. Enter the VM (`hv_vcpu_run` — blocks until the guest does something interesting)
3. Check exit reason:
   - `HV_EXIT_REASON_EXCEPTION` with EC=0x16 → HVC hypercall. Read x0 for the hypercall ID, dispatch.
   - `HV_EXIT_REASON_CANCELED` → watchdog killed us (timeout)
4. Handle the hypercall:
   - `HC_CONSOLE` → read string from guest memory, print it
   - `HC_TIME` → return frozen timestamp
   - `HC_RANDOM` → return next value from seeded PRNG
   - `HC_DB_READ` → read collection name from mailbox, write JSON response
   - `HC_READY` → guest finished init, time to snapshot
   - `HC_EXIT` → guest is done
   - `0xDE` → guest EL1 exception (from our exception vector table)

Key concept: `gpa_to_host_ptr` translates guest physical addresses to host pointers. The host can read/write guest memory directly because it's mmap'd.

### Phase 4: Snapshot & Fork (30 min)

**7. `host/src/snapshot.rs` (197 lines)**
- `CpuState::capture` — reads all 35 GPRs + system registers + 32 SIMD registers from the vCPU
- `CpuState::restore` — writes them all back
- `to_bytes`/`from_bytes` — simple serialization (raw bytes, not serde)
- `Template` — bundles CPU state + memory file path + metadata
- `mmap_cow_memory` — the fork primitive: `mmap(MAP_PRIVATE)` on the template's memory file

**8. `host/src/main.rs` — `cmd_snapshot` and `cmd_fork` (lines 370-520)**
- `cmd_snapshot`: boot guest → run to HC_READY → capture CPU state → write memory to file
- `cmd_fork`: mmap CoW memory → write control block + mailbox → create VM → restore CPU state → run to completion. Note the watchdog thread for timeout preemption.

### Phase 5: The Guest (1 hour)

Now switch to the guest — bare-metal code running inside the VM.

**9. `guest/linker.ld` (34 lines)**
Places `.text` at 0x40000000 (GUEST_BASE). Defines `__bss_start` and `__bss_end` for BSS zeroing. Note ALIGN(16) on sections — needed because the compiler generates SIMD instructions.

**10. `guest/src/main.rs` (353 lines)**
Read in this order:
- `_start` (bottom of file) — assembly: installs exception vector table, zeros BSS, calls `guest_main`
- Exception vector table (global_asm) — each EL1 exception does an HVC to report ESR/FAR/ELR to the host
- `hypercall` — inline asm for `HVC #0`
- `console_write` / `_guest_console_write` — sends strings to host via HC_CONSOLE
- `BumpAllocator` — the global allocator. Uses `UnsafeCell` (not atomics — MMU is off, exclusive monitors don't work). `set_heap_limit` is called after fork resume.
- `malloc`/`calloc`/`realloc`/`free` — C-compatible allocator for QuickJS. malloc stores size in a 16-byte header for realloc.
- `libm_export!` macro + manual libm functions — forwards C math functions to the Rust `libm` crate
- `guest_main` — the entry point: HC_READY (snapshot point) → read control block → read JS from mailbox → eval → HC_EXIT

**11. `guest/src/quickjs_ffi.rs` (364 lines)**
- `JSValue` struct — 128-bit: `{ u64 union, i64 tag }`. Tags distinguish ints, floats, strings, objects, etc.
- FFI declarations — `JS_NewRuntime`, `JS_Eval`, etc. Note that some QuickJS functions are `static inline` in the header and needed Rust wrappers (`js_to_cstring_len`, `js_is_exception`, `js_new_cfunction`).
- `js_console_log` — native C function registered as `console.log`. Converts each arg to string via `JS_ToCStringLen2`, sends to host.
- `js_date_now` — returns `HC_TIME / 1_000_000` (ns → ms)
- `js_math_random` — returns `HC_RANDOM >> 11` normalized to [0, 1)
- `js_db_query` — writes collection name to mailbox, does HC_DB_READ, parses JSON response with `JS_ParseJSON`
- `install_builtins` — sets up all JS globals: console.log, Date.now, Math.random, db.query
- `eval_js` — creates runtime + context, installs builtins, evaluates JS, handles exceptions

### Phase 6: The C Stubs (30 min)

**12. `guest/quickjs/libc_stubs.c` (590 lines)**
QuickJS is a C library that expects a libc. We provide a minimal freestanding shim:
- `errno`, `stdout`/`stderr` stubs
- `printf`/`vsnprintf` — hand-rolled, handles %s/%d/%u/%x/%p/%f (float is integer-truncated, QuickJS uses its own dtoa)
- String functions: `memchr`, `strchr`, `strstr`, `strcmp`, `strncmp`, `strlen`, `strdup`, etc.
- `strtol`/`strtod` — minimal parsers
- `ctype` functions: `isdigit`, `isalpha`, etc.
- `qsort` — insertion sort with heap-allocated swap buffer
- Time stubs: `time`, `gettimeofday`, `clock_gettime` — all route through `_guest_get_time_ns` (HC_TIME)
- pthread stubs — all no-ops (single vCPU, no threading)
- `setjmp`/`longjmp` — hand-written aarch64 assembly (saves/restores callee-saved registers). QuickJS needs these for exception handling.

**13. `guest/build.rs` (68 lines)**
Cross-compiles QuickJS for `aarch64-unknown-none-elf`. Key flags:
- `-ffreestanding -nostdinc` — don't use system headers
- `-U__APPLE__` — prevent QuickJS from calling `malloc_size()`
- `-mstrict-align` — prevent SIMD struct copies that require 16-byte alignment
- `--STDC_NO_ATOMICS__` — disable C11 atomics (no threading)
- Uses `llvm-ar` from the Rust toolchain (macOS `ar` can't create ELF archives)

### Phase 7: Build & Test Infrastructure (15 min)

**14. `Justfile` (64 lines)**
Build recipes: `guest`, `snapshot`, `fork`, `bench`, `demo`, `test`. Note the `_build-and-sign` helper that codesigns the binary with the hypervisor entitlement.

**15. `host/src/main.rs` — CLI and tests (lines 610-end)**
- Clap derive structs for CLI parsing
- `run_host_cmd` / `run_host_cmd_full` — test helpers that invoke the signed binary
- Integration tests: JS eval, Date.now, db.query, determinism, heap limit, timeout

### Bonus: The Spike Test

**16. `spike/src/main.rs` (705 lines)**
The original proof-of-concept from M0. Tests HVF basics: VM creation, HVC trapping, CoW semantics, VM creation latency, concurrent VM limits. Good for understanding the raw HVF API without the abstraction layers.

## Key Questions to Test Understanding

After reading, you should be able to answer:
1. Why does the guest use `UnsafeCell` instead of `AtomicUsize` for the allocator?
2. What happens if the guest executes an instruction at an unmapped address?
3. How does the host know the guest wants to print something vs. query the database?
4. Why do we need `-mstrict-align` for the C code but not for Rust?
5. What's the difference between `cmd_run` and `cmd_fork`?
6. How does the watchdog thread avoid calling `hv_vcpus_exit` after the VM is destroyed?
7. Why does `JS_ToCStringLen` need a Rust wrapper but `JS_Eval` doesn't?
