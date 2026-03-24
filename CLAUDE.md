# CLAUDE.md

## Project

Lightweight hypervisor for running JavaScript deterministically in hardware-isolated Linux VMs. Cross-platform: macOS (Apple Silicon) using Hypervisor.framework, Linux using KVM. Production target: Linux/KVM with Zeroboot-style CoW fork.

## Architecture

The VM boots a minimal Linux kernel. A no_std init process (PID 1) creates three pipes (JS stdin, ready signal, output capture), forks a child, and execs the JS runner (V8 or Boa). The runner initializes, signals readiness via fd 3, and blocks on stdin. The host snapshots the VM. On fork, the host writes JS to the inbox, init reads it and pipes to the runner. Runner output flows back through the output pipe → init → outbox → host.

```
Guest VM (single vCPU)
├── Linux kernel (prebuilt Image-arm64, Cloud Hypervisor 6.16.9)
├── /init (no_std Rust, raw syscalls)
│   ├── Creates JS pipe + ready pipe
│   ├── Forks child, execs /runner
│   ├── Blocks on ready pipe until runner signals
│   ├── Writes CONVEX_READY to inbox
│   └── After fork: re-mmaps inbox, pipes JS to runner, collects output → outbox
└── /runner (V8 or Boa)
    ├── Initializes JS engine + console.log
    ├── Pre-loads /bundle.js if present
    ├── Signals ready on fd 3
    └── Blocks on stdin, evals JS when received

Host process (macOS or Linux)
├── Hypervisor abstraction (hypervisor/)
│   ├── types.rs — shared types (SimdReg, VcpuExit, SysReg, IccReg)
│   ├── hvf.rs — macOS backend (Hypervisor.framework)
│   └── kvm.rs — Linux backend (KVM ioctls)
├── Linux boot (linux_boot.rs) — boot, snapshot, fork, serve
├── DTB generation (dtb.rs) — CPU, memory, GIC, PL011, timer
├── PL011 UART emulation (pl011.rs)
├── PSCI handler (psci.rs)
├── Virtual timer (vtimer.rs) — deterministic, event-driven
└── Snapshot/fork engine (snapshot.rs) — memfd + CoW mmap
```

## Key constants (must stay in sync across files)

```
Guest RAM base:    0x4000_0000  (host/src/linux_boot.rs)
Guest RAM size:    512 MiB      (host/src/linux_boot.rs)
Shared region GPA: 0x3F00_0000  (host/src/linux_boot.rs, init/src/main.rs)
  Inbox (host→guest): 0x3F00_0000, 8 MiB (JS code)
  Outbox (guest→host): 0x3F80_0000, 8 MiB (captured output)
Ready pipe fd:     3            (init/src/main.rs, runner-v8, runner-js)
Output pipe:       fd 1 (stdout) (init captures, writes to outbox)
READY sentinel:    "CONVEX_READY" (init/src/main.rs, host/src/linux_boot.rs)
GICD base:         0x0800_0000  (host/src/dtb.rs)
GICR base:         0x080A_0000  (host/src/dtb.rs)
UART base:         0x0900_0000  (host/src/dtb.rs, host/src/pl011.rs)
```

## Architecture invariants

- **Single vCPU per VM, always.** Design constraint for determinism.
- **Determinism is the core goal.** Every hardware input (time, entropy, interrupts, CPU identity) is trapped and virtualized. Same inputs produce bit-identical outputs.
- **VMs are ephemeral.** Each invocation gets its own VM, destroyed after use.
- **Inbox/outbox for data, pipes for IPC.** Host writes JS to inbox. Init reads it and pipes to runner. Runner stdout flows back through output pipe → init → outbox → host reads result. Runner never touches /dev/mem.

## Cross-hypervisor abstraction

The `hypervisor/` module provides a compile-time backend selection via `cfg(target_os)`:
- **macOS:** `hypervisor/hvf.rs` — Hypervisor.framework FFI bindings
- **Linux:** `hypervisor/kvm.rs` — KVM ioctls via `kvm-ioctls`/`kvm-bindings` crates

Both backends expose: `VmHandle` (VM lifecycle, memory mapping, GIC), `VcpuHandle` (run, register get/set, MMIO read-back), `GicHandle` (SPI injection, state save/restore).

Key differences:
- **MMIO exits:** HVF returns data-abort syndrome (must decode). KVM returns `KVM_EXIT_MMIO` with decoded addr/data/len.
- **MMIO reads:** HVF writes result to guest register via `complete_mmio_read()`. KVM writes result to `kvm_run.mmio.data` via `complete_mmio_read()`.
- **PC advance:** HVF `run()` advances PC for MMIO exits. KVM kernel advances PC automatically. Do NOT double-advance.
- **GIC:** HVF uses `hv_gic_create()` with opaque state blob. KVM uses `KVM_CREATE_DEVICE(VGIC_V3)` with per-register attributes.
- **vCPU init:** KVM requires `KVM_ARM_VCPU_INIT` before any register access. vCPU must be created before GIC `CTRL_INIT`.
- **Register encoding:** HVF uses u16 IDs. KVM uses ARM64 sysreg encoding (Op0/Op1/CRn/CRm/Op2). SP_EL0, SP_EL1, SPSR_EL1, ELR_EL1 are core registers on KVM, not sysregs.
- **Timer trapping:** KVM supports `CNTHCTL_EL2` natively (except on pKVM/Asahi). HVF uses binary patching (HVC) on M1.

## Timer determinism

Three modes, auto-detected at snapshot time:
1. **CNTHCTL_EL2 (native):** Zero-overhead trapping. Available on non-pKVM KVM and M4+ HVF with EL2.
2. **HVC patching (HVF on M1):** Timer MRS/MSR instructions replaced with `HVC #imm16`. HVF forwards all HVCs to userspace. Immediate encodes operation type + register (0x100+Rt for counter read, etc.).
3. **BRK patching (KVM on pKVM):** Timer instructions replaced with `BRK #imm16`. With `KVM_SET_GUEST_DEBUG`, BRK exits as `KVM_EXIT_DEBUG`. HSR field contains the BRK immediate (bits 15:0 of ESR). Immediates use 0xE1xx-0xE2xx range to avoid collision with kernel BUG()/KASAN/FAULT_BRK_IMM.

Boot uses real timer (fast, ~300ms). Patching is deferred to after snapshot. Fork executes patched instructions → deterministic `Date.now()`.

## Snapshot/fork (Zeroboot pattern)

Follows the same approach as [Zeroboot](https://github.com/zerobootdev/zeroboot):
1. Boot Linux + V8 to READY point (one-time, ~300ms)
2. Save CPU state + guest memory to template
3. For each invocation: `memfd_create` + `mmap(MAP_PRIVATE | MAP_NORESERVE)` for CoW memory, create fresh VM + vCPU + GIC, restore CPU state, write JS to inbox, run

`serve-linux` command handles multiple requests in a loop (amortizes process startup).

## Kernel boot parameters

```
earlycon=pl011,mmio32,0x09000000  # early console for debugging
nokaslr norandmaps                # disable ASLR for determinism
random.trust_cpu=on               # seed CRNG from trapped RNDR
nosmp                             # single CPU
clocksource=arch_sys_counter      # ARM timer (virtualized)
nohz=off                          # consistent timer interrupts
console=ttyAMA0                   # PL011 serial console
quiet loglevel=0                  # suppress kernel messages (performance)
rdinit=/init                      # our no_std init as PID 1
```

## Performance

```
KVM serve mode (Asahi M1, pKVM, BRK timer patching):
  Hypervisor overhead: 0.5ms (VM create + mmap + vCPU + GIC + restore)
  Guest execution:     3.5ms (V8 eval via pipe IPC)
  Total per fork:      4.0ms avg
  Bundle preloading:   fork latency independent of bundle size (92KB→4.2ms, 950KB→5.3ms)

HVF (macOS M1):
  Total per fork:      15ms (11ms CoW mmap + 2ms restore + 2ms guest)
  Bundle preloading:   ~15ms regardless of bundle size

Phase 1 bare-metal (macOS M1, QuickJS):
  Fork:                154µs p50
  Fork + JS eval:      548µs p50
```

## Code style

- Rust 2021 edition.
- No `unwrap()` on hypervisor calls — use `check_hv()` (HVF) or `expect()` with context (KVM).
- Explicit types for register values and GPA addresses.
- `init` is `no_std` with zero dependencies — raw syscalls only.
- Tests in `#[cfg(test)]` on the host side. Guest code tested by running in a VM.

## Review process

- Read through all code carefully
- Run `codex exec -m gpt-5.4 -c model_reasoning_effort="xhigh"` for second opinions on tricky issues
- Write any missing tests, iterate until fixed
- Check for opportunities to use high quality 3rd party crates
- Review code organization / duplication

## Known limitations

**macOS HVF:**
- Per-process single VM (`hv_vm_create` returns `HV_BUSY` on second call)
- No EL2 access on M1 (timer trapping via CNTHCTL_EL2 not available — uses HVC patching)
- M4 may expose EL2 timer trapping
- CoW mmap of 512 MiB takes ~11ms — dominates fork latency
- SIMD FFI alignment: `HvSimdFpUchar16` must be `#[repr(C, align(16))]`, not a type alias

**Linux KVM:**
- pKVM (Asahi) restricts CNTHCTL_EL2 — uses BRK patching for deterministic timer
- pKVM restricts some sysregs (CSSELR_EL1, PAC keys) — gracefully returns 0 on ENOENT
- GIC/ICC state save/restore not yet implemented (stubs in kvm.rs)
- vtimer offset save/restore not yet implemented
- True process fork() doesn't work on stock KVM (vCPU is thread-bound)

## Bugs found and fixed

- **SIMD alignment**: HvSimdFpUchar16 was `[u8; 16]` (align=1) but SDK type has align=16
- **SRT==31 MMIO**: Register 31 in data-abort syndrome means XZR, not PC
- **MMIO sign extension**: LDRSB/LDRSH/LDRSW loads need sign extension, not zero extension
- **dup3 old==new**: Returns EINVAL; use fcntl(F_SETFD, 0) to clear O_CLOEXEC instead
- **kmsg fd leak**: /dev/kmsg opened without O_CLOEXEC leaked to runner via execve
- **READY spin**: Comparing 1 byte could match JS starting with 'C'; now compares 8 bytes
- **write_all short writes**: Pipe writes can be partial; loop until complete
- **console_log panic**: V8 toString() can throw; use match not unwrap
- **Boa stale mmap**: Runner-js was reading mailbox via /dev/mem; switched to stdin pipe
- **Boa ready signal timing**: Signaled before Context creation; moved after
- **Watchdog join latency**: 100ms sleep meant 100ms join; added fast variant (1ms)
- **V8 snapshot timing**: Init wrote READY before V8 finished; added ready pipe protocol
- **TVAL sign extension**: `as i32 as u64` zero-extends; need `as i32 as i64 as u64`
- **Initrd placement**: Kernel image_size > file size; BSS overwrote initrd
- **Spectre HVC**: Kernel patched exception return with HVC; return SMCCC_RET_NOT_REQUIRED
- **SP_EL1/SP_EL0 KVM encoding**: Core registers (kvm_regs struct), not sysregs
- **SPSR_EL1/ELR_EL1 on KVM**: Also core registers, not sysregs
- **KVM vCPU init order**: Must call KVM_ARM_VCPU_INIT before any register get/set
- **KVM GIC init order**: vCPU must be created BEFORE GIC CTRL_INIT
- **KVM MMIO read-back**: Must write response to kvm_run.mmio.data before next KVM_RUN
- **PL011 ID registers**: Linux driver checks PeriphID/CellID at 0xFE0-0xFFC
- **HVF double PC advance**: hvf.rs run() advances PC for MMIO; linux_boot.rs must NOT advance again
- **KVM HVC not forwarded**: Non-PSCI HVCs handled in-kernel; use BRK instead
- **BRK immediate collision**: 0x100 collides with kernel FAULT_BRK_IMM; moved to 0xE1xx-0xE2xx
- **BRK HSR extraction**: KVM_EXIT_DEBUG provides ESR in hsr field; no guest memory read needed
- **MMIO patching doesn't work on KVM**: Guest LDR/STR uses VA not GPA; abandoned in favor of BRK

## Reference code

- **Zeroboot** (github.com/zerobootdev/zeroboot): KVM fork engine, CoW mmap, memfd patterns
- **Cloud Hypervisor** (github.com/cloud-hypervisor): Prebuilt aarch64 kernels, KVM GIC setup
- **Firecracker** (github.com/firecracker-microvm/firecracker): KVM aarch64 kernel configs
- **crosvm** (chromium.googlesource.com/chromiumos/platform/crosvm): HVF backend reference
- **libkrun** (github.com/containers/libkrun): HVF + ARM64 register setup
- **Hyperlight** (github.com/hyperlight-dev/hyperlight): Microsoft's micro-VM, similar architecture
