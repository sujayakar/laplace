# Laplace

*Named after [Laplace's demon](https://en.wikipedia.org/wiki/Laplace%27s_demon) — the thought experiment that a being with perfect knowledge of every particle's state could predict the future with certainty. Laplace controls every hardware input to the VM (time, entropy, interrupts, CPU identity), making execution perfectly deterministic. And like any good systems project, it runs as a daemon.*

A lightweight hypervisor for running JavaScript in hardware-isolated VMs with deterministic execution and sub-20ms cold starts. Built on Apple Hypervisor.framework (aarch64 macOS), with a path to Linux/KVM for production.

## What it does

1. **Boots a minimal Linux kernel** inside a single-vCPU VM
2. **Pre-initializes V8** (or Boa) inside the VM, then snapshots the entire VM state
3. **Forks from the snapshot** using CoW memory (`mmap MAP_PRIVATE`) — new VM in ~15ms
4. **Evaluates JavaScript** in the forked VM with full V8, then destroys the VM

Every fork starts from identical state. Same inputs produce bit-identical outputs. The hypervisor controls all hardware inputs (time, entropy, CPU identity, interrupts) to guarantee determinism.

## Performance

Measured on Apple M1, macOS, Hypervisor.framework:

| Metric | Value |
|--------|-------|
| V8 fork + JS eval (simple expression) | **15ms** |
| V8 fork + 460KB bundle (lodash, zod, date-fns, marked, handlebars, ajv) | **17ms** |
| V8 fork + 10MB bundle (no preload) | 30ms |
| V8 fork + 10MB bundle (preloaded in snapshot) | **16ms** |
| Guest execution time (V8 eval only) | ~2ms |
| CoW mmap overhead (512 MiB) | ~11ms |

With bundle preloading, **fork latency is independent of bundle size** — the bundle is compiled once during snapshot, and each fork only receives a tiny invocation string.

### Breakdown

```
Fork timing (15ms total):
  Template load:    0.1ms   (read cpu.state, gic.state, timer.meta from disk)
  CoW mmap:        11.0ms   (MAP_PRIVATE on 512 MiB guest.mem file)
  VM create:        0.1ms   (hv_vm_create + hv_vm_map)
  GIC restore:      0.2ms   (hv_gic_create + hv_gic_set_state)
  CPU restore:      4.5ms   (35 GPRs + 36 sys regs + 32 SIMD + ICC + vtimer)
  Guest execution:  2.0ms   (kernel resume + pipe read + V8 eval + PSCI shutdown)
```

On Linux/KVM with Zeroboot-style `fork()`, the mmap + CPU restore costs vanish (inherited by the child process). Target: **<1ms**.

## Quick start

Requirements: macOS 13+ on Apple Silicon, Rust nightly, `just`, `zig` (for cross-compiling V8 runner).

```bash
# Build everything
just init                    # Build no_std init (PID 1)
just runner-js               # Build Boa JS runner (musl-static)

# Snapshot + fork with Boa (fast path)
just snapshot-linux           # Boot kernel → init Boa → snapshot
just fork-linux --msg '"console.log(1+2)"'  # Fork → eval → "3"

# V8 (requires zig for cross-compilation)
# See "Building the V8 runner" below
```

### Building the V8 runner

The V8 runner is dynamically linked against glibc and cross-compiled with `zig cc`:

```bash
# Ensure zig-gnu-cc is on PATH
export PATH="$PATH:$(pwd)/scripts"

# Build V8 runner
cd runner-v8 && cargo build --target aarch64-unknown-linux-gnu --release

# Build initramfs with V8 + glibc shared libs
# (requires aarch64 glibc libs in /tmp/v8-extract/lib/)
./scripts/mkinitramfs.sh init/target/aarch64-unknown-none/release/convex-init \
  /tmp/hvf-initramfs-v8.cpio \
  runner-v8/target/aarch64-unknown-linux-gnu/release/convex-runner-v8

# Snapshot with V8
./target/release/convex-hypervisor snapshot-linux kernel/Image-arm64 \
  --initrd /tmp/hvf-initramfs-v8.cpio /tmp/hvf-v8-template

# Fork with V8
./target/release/convex-hypervisor fork-linux \
  --msg 'console.log("V8!", 1+2)' /tmp/hvf-v8-template
```

### Bundle preloading

For large JS bundles, include the bundle in the initramfs as `/bundle.js`. V8 compiles it during snapshot, so forks only receive a tiny invocation:

```bash
# Include bundle.js in initramfs (alongside init + runner + libs)
# Then snapshot — V8 pre-compiles the bundle
# Fork with just the invocation:
./target/release/convex-hypervisor fork-linux \
  --msg 'console.log(handleRequest({action: "query"}))' /tmp/hvf-v8-template
```

## Architecture

```
+-----------------------------------------------------------+
|  Guest VM (single vCPU, hardware-isolated)                |
|                                                            |
|  /runner (V8 or Boa JS engine)                            |
|    Reads JS from stdin (pipe from init)                   |
|    Evaluates, writes output to /dev/kmsg                  |
|       | stdin pipe                                        |
|  /init (no_std Rust, PID 1)                               |
|    Creates ready pipe + JS pipe, forks, execs /runner     |
|    Waits for runner "ready" signal on fd 3                |
|    Writes READY to inbox → host snapshots                 |
|    After fork: reads JS from inbox, pipes to runner       |
|    Collects runner stdout → writes to outbox              |
|       | /dev/mem mmap                                     |
|  Linux kernel (prebuilt Image-arm64, minimal config)      |
|    Handles syscalls natively (~100-200ns each)            |
|    Timer interrupts via virtualized ARM timer             |
+----------+------------------------------------------------+
           | VM exits: MMIO, PSCI, timer
+----------v------------------------------------------------+
|  Host process (macOS, Rust)                                |
|                                                            |
|  Hypervisor shell (Apple Hypervisor.framework)            |
|    VM create, CoW fork, vCPU run loop                     |
|  PL011 UART emulation (earlycon output)                   |
|  PSCI handler (SYSTEM_OFF, CPU_ON, etc.)                  |
|  GICv3 (via HVF built-in emulation)                       |
|  Virtual timer (deterministic, event-driven)              |
|  Snapshot/fork engine (MAP_PRIVATE CoW)                    |
|  Inbox+Outbox (16 MiB at GPA 0x3F00_0000)                |
+------------------------------------------------------------+
```

### Key design decisions

- **Single vCPU per VM, always.** Design constraint for determinism.
- **Real Linux kernel.** Full binary compatibility — V8, Python, Go, anything that runs on Linux.
- **No virtio.** Communication via inbox/outbox (shared memory) + pipe IPC. Host writes JS to inbox, reads output from outbox.
- **Determinism by environment.** The kernel is deterministic code; we control all hardware inputs (time, entropy, interrupts).
- **Ephemeral VMs.** Each invocation gets its own VM, destroyed after use.

## Repo structure

```
convex-hypervisor/
├── host/src/
│   ├── main.rs          # CLI: run, snapshot, fork, bench, boot-linux, etc.
│   ├── hvf.rs           # Hand-written FFI bindings for Hypervisor.framework
│   ├── linux_boot.rs    # Linux VM lifecycle (boot, snapshot, fork)
│   ├── dtb.rs           # Device tree blob generation (CPU, memory, GIC, PL011, timer)
│   ├── pl011.rs         # Minimal PL011 UART emulation
│   ├── psci.rs          # PSCI v1.1 + SMCCC handler
│   ├── vtimer.rs        # Deterministic virtual timer (adaptive, event-driven)
│   ├── snapshot.rs      # CPU state capture/restore + template serialization
│   └── elf.rs           # ELF loader (for bare-metal guest)
├── init/src/
│   └── main.rs          # no_std PID 1: 3 pipes, fork, exec /runner, inbox/outbox
├── runner-v8/src/
│   └── main.rs          # V8 JS runner: init V8, signal ready, read stdin, eval
├── runner-js/src/
│   └── main.rs          # Boa JS runner: same pattern, pure Rust
├── guest/src/            # Bare-metal guest (phase 1, QuickJS — still works)
├── shared/src/
│   └── lib.rs           # Hypercall IDs, memory layout constants
├── kernel/
│   └── Image-arm64      # Prebuilt minimal Linux kernel
├── scripts/
│   ├── mkinitramfs.sh   # Build cpio initramfs from init + runner
│   └── zig-gnu-cc       # zig cc wrapper for aarch64-linux-gnu cross-compilation
├── Justfile             # Build recipes
├── PLAN.md              # Phase 1 plan (bare-metal, M0-M5, completed)
├── PLAN-LINUX.md        # Phase 2 plan (Linux VM, M9-M13)
└── CLAUDE.md            # Architecture reference for AI assistants
```

## How it works

### Snapshot (one-time, ~4s with V8)

1. Boot Linux kernel with initramfs containing init + V8 runner
2. Init mounts devtmpfs/proc, creates pipes, forks child, execs `/runner`
3. V8 runner initializes V8 engine (platform, isolate, context, console.log)
4. If `/bundle.js` exists, V8 compiles and executes it (pre-loading)
5. V8 runner signals init via ready pipe (fd 3)
6. Init writes `CONVEX_READY` to inbox
7. Host detects READY, saves: CPU state (GPRs + sys regs + SIMD), GIC state, ICC registers, vtimer offset, guest memory (512 MiB)

### Fork (per-request, ~15ms)

1. `mmap(MAP_PRIVATE)` the 512 MiB template file (CoW — pages fault on write)
2. Create VM, map memory, create GIC, create vCPU
3. Restore GIC state, ICC registers, vtimer offset, all CPU registers
4. Write JS code to inbox (8 MiB shared region)
5. Resume VM — kernel is already running, init wakes from spin loop
6. Init re-mmaps shared region (fresh after CoW), reads JS from inbox, pipes to V8's stdin
7. V8 evaluates JS, stdout flows through output pipe → init → outbox
8. Init calls PSCI SYSTEM_OFF, host reads outbox, prints result, destroys VM

## Development

```bash
# Run host tests (40 tests)
cargo test -p convex-hypervisor

# Build init (no_std, aarch64-unknown-none)
cd init && cargo build --target aarch64-unknown-none --release

# Build Boa runner (musl-static)
cd runner-js && cargo build --target aarch64-unknown-linux-musl --release

# Build V8 runner (glibc, zig cc)
cd runner-v8 && PATH="$PATH:../scripts" cargo build --target aarch64-unknown-linux-gnu --release
```

## Project history

- **Phase 1 (M0-M5):** Bare-metal hypervisor with QuickJS in a no_std Rust guest. Proved: HVF works, CoW fork in 154us, deterministic JS eval in 548us. See [PLAN.md](PLAN.md).
- **Phase 2 (M9-M12):** Pivoted to Linux-in-VM for full binary compatibility. Boots real Linux kernel, runs V8/Boa, snapshot/fork with pre-initialized engine. See [PLAN-LINUX.md](PLAN-LINUX.md).

## License

Apache-2.0
