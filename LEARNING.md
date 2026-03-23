# Learning Guide: Convex Hypervisor Codebase

## Prerequisites

Before reading code, understand these concepts:
- **ARM64 exception levels**: EL0 (user), EL1 (kernel), EL2 (hypervisor). Our guest runs at EL1, Apple's Hypervisor.framework runs at EL2.
- **HVC instruction**: `HVC #0` traps from EL1 to EL2, causing a VM exit. Used for PSCI power management.
- **Copy-on-Write (CoW)**: `mmap(MAP_PRIVATE)` on a file gives you a private copy — writes go to new pages, the file is unchanged. This is how we fork VMs cheaply.
- **GICv3**: ARM's Generic Interrupt Controller. Routes interrupts to CPUs. HVF provides built-in emulation.
- **Device Tree Blob (DTB)**: Data structure describing hardware to the Linux kernel (CPU, memory, GIC, UART, timer).

## Two codebases in one repo

**Phase 1 (bare-metal):** `guest/`, `shared/`, `host/src/main.rs` top half. A `no_std` Rust guest with QuickJS running directly at EL1, communicating via HVC hypercalls. Still works, still has its own `just run`/`just snapshot`/`just fork` commands.

**Phase 2 (Linux VM):** `init/`, `runner-v8/`, `runner-js/`, `host/src/linux_boot.rs` and friends. A real Linux kernel with userspace JS runners, communicating via inbox/outbox (shared memory) + 3 pipes (stdin, ready, stdout). This is the active development path.

## Reading order (Phase 2 — Linux VM)

### Phase 1: The host-side Linux boot path (1 hour)

**1. `host/src/hvf.rs` (~340 lines)**
Raw FFI bindings to Apple's Hypervisor.framework. Key things:
- `HvSimdFpUchar16` — 16-byte aligned SIMD register type (alignment matters for FFI!)
- `SNAPSHOT_SYS_REGS` — the ~36 system registers saved/restored for snapshots
- `ICC_REGS` — GIC CPU interface registers
- GIC functions: `hv_gic_create`, `hv_gic_get_state`, `hv_gic_set_state`
- vtimer offset: `hv_vcpu_get_vtimer_offset`, `hv_vcpu_set_vtimer_offset`

**2. `host/src/dtb.rs` (~190 lines)**
Builds the device tree blob using `vm-fdt` crate. Describes: 1 CPU, memory region, GICv3, PL011 UART, ARM timer, PSCI, chosen node with bootargs and initrd location.

**3. `host/src/linux_boot.rs` (~1200 lines)**
The heart of Phase 2. Three main functions:
- `cmd_boot_linux` — one-shot boot (for debugging)
- `cmd_snapshot_linux` — boot to READY, save template (CPU + GIC + ICC + vtimer + memory)
- `cmd_fork_linux` — CoW mmap, restore state, write JS to inbox, run, read outbox

Also contains: MMIO dispatch, PL011 routing, binary timer patching, watchdog threads, the vCPU run loop, and timing instrumentation.

Key concepts:
- `MmioAccess` + `decode_data_abort` — decodes ARM data-abort syndrome for MMIO traps
- `mmio_read_reg`/`mmio_write_reg` — handles SRT==31 (XZR) and sign extension
- `spawn_watchdog` / `spawn_watchdog_fast` — periodic VM exits for timer injection
- Fork timing breakdown: load, mmap, vm_create, vm_map, gic, restore, run

**4. `host/src/pl011.rs` (~80 lines)**
Minimal PL011 UART emulation. Just enough for earlycon: write chars to host stderr, return TX-ready status on reads.

**5. `host/src/psci.rs` (~90 lines)**
PSCI v1.1 handler + SMCCC dispatch. Handles SYSTEM_OFF, CPU_ON, FEATURES, VERSION. Also returns `SMCCC_RET_NOT_REQUIRED` for Spectre mitigation queries.

**6. `host/src/vtimer.rs` (~150 lines)**
Deterministic virtual timer. Fixed counter increment per read. WFI warps time forward to the next timer deadline. Event-driven, not tick-driven.

**7. `host/src/snapshot.rs` (~200 lines)**
CPU state capture/restore: 35 GPRs + system registers + 32 SIMD registers. Simple byte serialization (not serde). `Template` struct bundles CPU state + memory file path.

### Phase 2: The guest-side code (30 min)

**8. `init/src/main.rs` (~290 lines)**
The most constrained code in the repo: `no_std`, `no_main`, zero dependencies, raw syscalls only.
- Syscall wrappers (`syscall1` through `syscall6`) via `svc #0` inline asm
- `dup_to_fd` — handles dup3 edge case when old_fd == new_fd (uses fcntl)
- Pipe + fork lifecycle: creates JS pipe + ready pipe, forks, execs /runner
- Shared region (inbox/outbox): maps `/dev/mem` at GPA 0x3F00_0000, writes READY to inbox, spins
- After fork-resume: re-mmaps shared region fresh (old mmap is stale after CoW), reads JS from inbox, pipes to runner
- After runner exits: reads output pipe, writes to outbox for host to read
- `write_all` handles short writes; READY spin compares 8 bytes

**9. `runner-v8/src/main.rs` (~135 lines)**
V8 JS runner using `rusty_v8`:
- Initializes V8 platform, isolate, context
- Installs `console.log` callback (handles toString exceptions)
- Pre-loads `/bundle.js` if it exists (compiled during snapshot, amortized)
- Signals readiness to init on fd 3, blocks on stdin
- Receives JS from stdin, evaluates, prints result, calls PSCI SYSTEM_OFF

**10. `runner-js/src/main.rs` (~75 lines)**
Boa (pure Rust) JS runner. Same lifecycle as V8 runner but lighter. Signals ready after Context creation.

### Phase 3: Build infrastructure (15 min)

**11. `scripts/mkinitramfs.sh`** — builds cpio initramfs from init binary + optional runner
**12. `scripts/zig-gnu-cc`** — zig cc wrapper for aarch64-linux-gnu cross-compilation (V8 runner)
**13. `Justfile`** — build recipes for all targets + snapshot/fork commands

## Key questions to test understanding

1. Why does init need two pipes (JS pipe + ready pipe)?
2. Why must init re-mmap `/dev/mem` after fork-resume?
3. What happens if the runner crashes before signaling on fd 3?
4. Why does `dup_to_fd` need a special case for old_fd == new_fd?
5. Why is `/dev/kmsg` opened with `O_CLOEXEC`?
6. What is the ready sentinel, and why do we compare 8 bytes instead of 1?
7. Where does the 11ms CoW mmap cost come from, and how would Linux/KVM eliminate it?
8. What does bundle preloading do, and why doesn't it help for bundles under ~1MB?
