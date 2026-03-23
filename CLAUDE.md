# CLAUDE.md

## Project

Lightweight hypervisor for running JavaScript deterministically in hardware-isolated Linux VMs. Development target: aarch64 macOS (Apple Silicon) using Hypervisor.framework. Production target: Linux/KVM with Zeroboot-style CoW fork.

The project has two phases:
- **Phase 1 (M0-M5, completed):** Bare-metal guest with QuickJS, proving HVF + CoW fork + determinism. See PLAN.md.
- **Phase 2 (M9-M12, active):** Real Linux kernel in VM with V8/Boa. See PLAN-LINUX.md.

## Current architecture (Phase 2 — Linux VM)

The VM boots a minimal Linux kernel. A no_std init process (PID 1) creates three pipes (JS stdin, ready signal, output capture), forks a child, and execs the JS runner (V8 or Boa). The runner initializes, signals readiness via fd 3, and blocks on stdin. The host snapshots the VM. On fork, the host writes JS to the inbox, init reads it and pipes to the runner. Runner output flows back through the output pipe → init → outbox → host.

```
Guest VM (single vCPU)
├── Linux kernel (prebuilt Image-arm64)
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

Host process (macOS)
├── HVF bindings (hvf.rs)
├── Linux boot (linux_boot.rs) — boot, snapshot, fork
├── DTB generation (dtb.rs) — CPU, memory, GIC, PL011, timer
├── PL011 UART emulation (pl011.rs)
├── PSCI handler (psci.rs)
├── Virtual timer (vtimer.rs) — deterministic, event-driven
└── Snapshot/fork engine (snapshot.rs) — CoW mmap
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

## Hypervisor.framework (HVF) specifics

- HVF runs the guest at EL1. HVC instructions cause VM exits (EC=0x16).
- After HVC, PC already points past the instruction — do NOT advance manually.
- Per-process single VM (`hv_vm_create` returns `HV_BUSY` on second call).
- Entitlement required: `com.apple.security.hypervisor` in entitlements.plist.
- GIC emulation is built into HVF (`hv_gic_create`, `hv_gic_set_state`).
- GIC state must be restored AFTER `hv_vcpu_create` (Apple requirement).
- SIMD registers (`HvSimdFpUchar16`) require 16-byte alignment (not the default 1-byte).

## Linux VM boot sequence

1. Load kernel Image-arm64 into guest memory at RAM base + 0x80000
2. Generate DTB with: 1 CPU, memory, GICv3, PL011 UART, timer, PSCI, chosen (bootargs + initrd)
3. Create VM, map memory, create GIC, create vCPU
4. Set PC = kernel entry, X0 = DTB address
5. Run vCPU loop handling: PL011 MMIO, PSCI HVCs, timer, Spectre mitigations

## Fork sequence

1. Load template (cpu.state, gic.state, icc.state, timer.meta)
2. `mmap(MAP_PRIVATE)` on guest.mem (512 MiB CoW)
3. Create VM, map memory + shared region (inbox+outbox), create GIC
4. Restore: GIC state → ICC registers → vtimer offset → CPU registers → unmask vtimer
5. Write JS to inbox, resume VM
6. Guest: init re-mmaps shared region, reads JS from inbox, pipes to runner
7. Runner evals JS, stdout flows through output pipe → init → outbox
8. Init calls PSCI SYSTEM_OFF, host reads outbox, prints result
7. Host: collect result, destroy VM (~15ms total on M1)

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

## Code style

- Rust 2021 edition.
- No `unwrap()` on HVF calls — use `check_hv()`.
- Explicit types for register values and GPA addresses.
- `init` is `no_std` with zero dependencies — raw syscalls only.
- Tests in `#[cfg(test)]` on the host side. Guest code tested by running in a VM.

## Performance (macOS M1)

```
V8 fork + eval:     ~15ms total (2ms guest, 11ms CoW mmap, 2ms restore)
Boa fork + eval:    ~15ms total
With bundle preload: ~15ms regardless of bundle size (1KB-10MB)
Phase 1 bare-metal: 154µs p50 fork, 548µs with QuickJS eval
```

## Reference code

- **Zeroboot** (github.com/zerobootdev/zeroboot): KVM fork engine, CoW mmap patterns
- **crosvm** (chromium.googlesource.com/chromiumos/platform/crosvm): HVF backend reference
- **libkrun** (github.com/containers/libkrun): HVF + ARM64 register setup
- **Hyperlight** (github.com/hyperlight-dev/hyperlight): Microsoft's micro-VM, similar architecture
