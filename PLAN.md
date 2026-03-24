# Laplace: Execution Plan

## Vision

Hardware-isolated deterministic JS execution with sub-5ms cold starts. Every function invocation runs in its own VM. Same inputs produce bit-identical outputs. Full Linux binary compatibility (V8, Python, Go — anything that runs on Linux).

## Completed milestones

### Phase 1: Bare-metal MVP (M0-M5) ✅

Proved the core stack on macOS/HVF: hypervisor shell, hypercall interface, determinism controls, CoW snapshot/fork. Guest was a `no_std` Rust ELF embedding QuickJS.

- M0: VM boots, HVC trapped, hello world (154µs fork, 548µs with JS eval)
- M1: Deterministic time + random via hypercalls
- M2: Snapshot + CoW fork (<1ms)
- M3: QuickJS integration in no_std guest
- M4: Mailbox + stub db.query
- M5: End-to-end demo + benchmarks

### Phase 2: Linux VM with V8 (M9-M12) ✅

Pivoted from micro-kernel to real Linux kernel for full binary compatibility. The kernel is just deterministic code — we control all hardware inputs.

- M9a: Kernel boots to earlycon (DTB, GIC, PL011, PSCI)
- M9b: Fully virtualized timer (adaptive clock, WFI warp)
- M9c: Initramfs + userspace (no_std init, Boa runner, V8 runner)
- M9d: virtio-vsock — skipped (mailbox + pipe IPC sufficient)
- M10: Snapshot + fork with Linux (GIC/ICC/vtimer state save/restore)
- M11: Boa JS eval on Linux
- M12: V8 JS eval on Linux (~15ms fork on HVF)

Key decisions that differed from original plan:
- Prebuilt Cloud Hypervisor kernel instead of compiling our own
- Boa (pure Rust) instead of rquickjs for M11
- Mailbox + pipe IPC instead of virtio-vsock
- V8 runner dynamically linked (glibc) with zig cross-compilation
- Init is no_std with raw syscalls, not musl-linked
- Ready pipe protocol (fd 3) for V8 initialization signaling

### Phase 3: KVM backend (active) ✅ boot/snapshot/fork working

Cross-platform hypervisor abstraction with HVF and KVM backends. V8 snapshot + fork on KVM with deterministic timer.

Completed:
- Cross-hypervisor abstraction (hypervisor/ module with compile-time backend selection)
- KVM backend: VM create, memory map, vCPU, GICv3, register get/set, MMIO handling
- V8 snapshot + fork on KVM: 4.0ms avg (serve mode)
- Deterministic timer on pKVM via BRK instruction patching
- Bundle preloading verified (fork latency independent of bundle size)
- Serve mode for amortized process startup
- memfd + MAP_NORESERVE (Zeroboot pattern) for fast CoW fork

## Current performance

```
KVM serve mode (Asahi M1, pKVM):
  Hypervisor overhead:  0.5ms
  Guest execution:      3.5ms
  Total per fork:       4.0ms avg

HVF (macOS M1):
  Total per fork:       15ms (11ms CoW mmap dominates)
```

## Next steps

1. **Test on real KVM** (AWS Graviton) — native CNTHCTL_EL2 trapping, no BRK exits → expected ~2.5ms total
2. **macOS verification** — confirm HVF backend still compiles and works
3. **GIC/ICC/vtimer state save/restore on KVM** — needed for full snapshot fidelity
4. **Optimize guest execution** — explore shared memory between V8 and host, bypassing pipe IPC
5. **Multi-language support** — Python, Go binaries in the VM
6. **Production hardening** — jailer/seccomp, error handling, resource limits

## What this enables

- **Convex function execution with hardware isolation.** Every function invocation runs in its own VM. Tenant code is separated from the database by a hardware boundary.
- **Deterministic re-execution.** Same function + same DB reads + same seed = identical execution. Enables OCC transaction replay, time-travel debugging, reproducible bug reports.
- **Multi-language support.** Anything that runs on Linux runs in the VM. Determinism provided by the VM, not the language runtime.
- **Antithesis-style testing.** Snapshot a known state, inject faults, verify invariants, branch and explore.
