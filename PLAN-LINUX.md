# PLAN-LINUX.md — Deterministic Linux VM

## What changed and why

Phase 1 (M0-M5) proved the hypervisor works: sub-millisecond CoW fork, deterministic time/random via hypercalls, JS evaluation in a bare-metal VM. Phase 2 (M6-M8) built a micro-kernel with MMU, EL0/EL1 split, cooperative threading, and ~20 Linux syscalls. The original plan called for continuing to build out the micro-kernel to ~70+ syscalls (M9-M10). We're pivoting away from that.

**The new plan: boot a real (minimal) Linux kernel inside the VM.**

The insight is that a Linux kernel running on our single-vCPU VM with trapped hardware inputs is already deterministic. The kernel is just code — deterministic code running on deterministic inputs produces deterministic outputs. We don't need to reimplement Linux; we just need to control the inputs Linux sees from hardware: time, entropy, CPU identity, memory layout, and interrupt timing. We already trap all of these.

This gives us full Linux binary compatibility — V8, CPython, Go binaries, Docker containers — without reimplementing hundreds of syscalls. Syscalls execute natively inside the VM at ~100-200ns each, faster than the ~500ns VM-exit cost of our micro-kernel's hypercall-based approach.

### What we keep from phase 1 and 2

Everything from M0-M5 is unchanged and proven:

- Hypervisor shell (HVF bindings, VM lifecycle, vCPU management)
- CoW fork engine (MAP_PRIVATE snapshot, 154μs p50 fork latency)
- Hypercall dispatch (HC_CONSOLE, HC_TIME, HC_RANDOM, HC_EXIT)
- Deterministic PRNG (ChaCha8, seeded per-fork)
- Virtual time (frozen timestamp via HC_TIME)
- Mailbox protocol (64 KiB shared region for bulk data)
- CLI and benchmark harness

M6-M8 (micro-kernel, MMU, EL0/EL1 split, threading) were valuable learning but are superseded by this approach. The micro-kernel code may be useful as reference but won't ship.

### What we're building instead

A minimal Linux kernel + initramfs inside the VM, with:

- A stripped kernel config (~3-5 MB compressed, ~10 MB in memory)
- A tiny init process that seeds entropy, sets up the environment, and execs the user runtime
- virtio-vsock for guest-host communication (DB queries, console output)
- Determinism enforced by the hypervisor's existing traps + kernel boot parameters

### Why this is better

| | Micro-kernel (old plan) | Linux in VM (new plan) |
|---|---|---|
| Binary compatibility | Partial (reimplement each syscall) | Full (it's Linux) |
| V8 support | Hard (need ~30 syscalls + /proc) | Works natively |
| Python support | Hard (needs filesystem, threads) | Works natively |
| Per-syscall overhead | ~500 ns (VM exit) | ~100-200 ns (native) |
| Engineering effort | 3-6 months for ~70 syscalls | 3-4 weeks for kernel config + init + virtio |
| Maintenance burden | Track Linux ABI changes forever | Bump kernel version |
| Determinism | By construction (we control every syscall) | By environment (we control every hardware input) |
| Snapshot size | ~64 MB | ~128-256 MB |

The only downside is larger snapshots. At 128 MB with CoW, per-fork RSS is still just the dirtied pages (~2-4 MB), and template distribution is a solved CDN problem.

---

## Architecture

```
+---------------------------------------------------------+
|  Guest VM (single vCPU, hardware-isolated)              |
|                                                          |
|  User binary (V8, Python, Go, any Linux ELF)            |
|       | syscall (native, ~100-200 ns)                   |
|  Linux kernel (minimal config, ~10 MB)                  |
|    mmap/mprotect  -> native page table mgmt             |
|    clone/futex    -> native CFS scheduler               |
|    clock_gettime  -> reads CNTVCT_EL0 (trapped)         |
|    getrandom      -> reads /dev/urandom (seeded)        |
|    write(vsock)   -> virtio-vsock to host               |
|       |                                                  |
|  virtio-vsock (guest kernel driver, built-in)           |
+---------+------------------------------------------------+
          | VM exits only for: trapped registers,
          | virtio-vsock doorbell, timer interrupt
+---------v------------------------------------------------+
|  Host process (macOS/Linux, Rust)                        |
|                                                          |
|  Hypervisor shell (HVF / KVM)                           |
|    VM create, CoW fork, vCPU run loop                   |
|       |                                                  |
|  Determinism layer                                      |
|    CNTVCT_EL0 trap  -> virtual time (adaptive clock)    |
|    RNDR/RNDRRS trap -> deterministic PRNG               |
|    ID register trap -> fixed CPU features               |
|    Timer management -> fully virtualized vtimer         |
|       |                                                  |
|  virtio-vsock device emulation                          |
|    DB queries   -> stub/real Convex backend             |
|    Console I/O  -> host stdout                          |
|       |                                                  |
|  Fork engine (CoW mmap, <1 ms)                          |
+----------------------------------------------------------+
```

Key difference from phase 1: the guest kernel handles syscalls natively. The hypervisor only sees VM exits for trapped register reads (time, random, CPUID), virtio-vsock doorbells, and timer interrupts. Everything else — mmap, futex, thread scheduling, signal delivery, ELF loading — runs at native speed inside the VM.

---

## Determinism model

### The core claim

**A single-vCPU Linux VM is deterministic if every input from outside the VM is deterministic.** The inputs from outside are:

1. **Timer counter** (CNTVCT_EL0 / CNTPCT_EL0) — trapped, returns virtual time
2. **Hardware entropy** (RNDR / RNDRRS registers) — trapped, returns deterministic PRNG
3. **CPU identity** (ID_AA64* registers) — trapped, returns fixed feature set
4. **Initial memory contents** — deterministic (same snapshot)
5. **virtio-vsock responses** — deterministic (same DB inputs produce same responses)
6. **Timer interrupts** — delivered at deterministic virtual time points (fully virtualized)

That's it. With these controlled, every byte of state inside the VM — kernel, userspace, all of it — evolves deterministically. The kernel's CFS scheduler, slab allocator, page fault handler, futex implementation, pipe buffers — all deterministic code on deterministic inputs.

### Adaptive virtual time

We fully virtualize the ARM timer. The physical counter and comparator are never exposed to the guest. Instead, the hypervisor maintains a virtual clock and delivers timer interrupts at deterministic virtual time points.

**The algorithm is event-driven, not tick-driven:**

```rust
struct VirtualClock {
    /// Current virtual counter value (in ticks at counter_freq_hz)
    counter: u64,

    /// Fixed increment per counter read (e.g., 24 ticks = 1us at 24 MHz)
    increment_per_read: u64,

    /// The guest's programmed timer deadline (from CNTV_CVAL_EL0 writes)
    timer_deadline: Option<u64>,

    /// Whether the guest timer is enabled and unmasked
    timer_enabled: bool,

    /// Counter frequency advertised to guest (matches real HW for sanity)
    counter_freq_hz: u64, // 24_000_000
}
```

**Exit handlers:**

- **CNTVCT_EL0 / CNTPCT_EL0 read** (trapped via CNTHCTL_EL2): Advance counter by `increment_per_read`, return new value. This handles `calibrate_delay` loops during boot and `clock_gettime` calls from userspace. Each read is a VM exit (~1-2us on Apple Silicon). Deterministic because same code makes the same sequence of reads.

- **CNTV_CVAL_EL0 write** (trapped via CNTHCTL_EL2.EL1TVT): Guest is programming the timer. Record the deadline but don't advance time. The kernel does this when scheduling the next timer interrupt.

- **CNTV_TVAL_EL0 write**: Guest sets a relative deadline. Compute `deadline = counter + tval` and record it.

- **CNTV_CTL_EL0 write**: Guest enables/disables/masks the timer. Update `timer_enabled`.

- **WFI (Wait For Interrupt)**: The guest is idle, waiting for an interrupt. This is the key optimization point. **Warp time forward to the next programmed timer deadline in one step.** If the guest sets a timer for +60s then does WFI, one VM exit, counter jumps forward, timer interrupt is injected. Not 60 million iterations at 1us each.

  ```rust
  fn handle_wfi(clock: &mut VirtualClock) -> Action {
      match clock.timer_deadline {
          Some(deadline) if clock.timer_enabled && deadline > clock.counter => {
              // Warp directly to the deadline
              clock.counter = deadline;
              clock.timer_deadline = None;
              InjectTimerIRQ
          }
          Some(deadline) if clock.timer_enabled => {
              // Timer already expired, fire immediately
              clock.timer_deadline = None;
              InjectTimerIRQ
          }
          _ => {
              // WFI with no timer = guest is stuck (shouldn't happen
              // in a well-behaved kernel). Advance a bit and retry.
              clock.counter += clock.counter_freq_hz / 100; // 10ms
              ReturnToGuest
          }
      }
  }
  ```

- **Before each VM entry**: Check if `counter >= timer_deadline` and inject a pending timer interrupt if so. This handles the case where time advanced past the deadline during compute (via counter reads) without the guest doing WFI.

- **Non-time exits** (console hypercall, DB query, etc.): Don't advance time. Time only moves on explicit counter reads or WFI warps.

**Why this is deterministic:** Every input to the time advancement decision is derived from guest state. The counter advances by a fixed amount per read. The warp decision on WFI is based on `timer_deadline`, set by a prior (deterministic) guest instruction. No wall-clock reads anywhere.

**Why we don't need instruction counting (Antithesis comparison):**

Antithesis uses PMC instruction-retired counters to drive virtual time because their exploration engine snapshots and branches at arbitrary instruction boundaries. They discovered the PMC has ~1 miscount per trillion instructions and that APIC interrupt delivery has variable latency — both required years of workaround engineering.

We don't need any of this. We need time to be *deterministic across runs*, not precisely proportional to computation. The hardest ~80% of Antithesis's hypervisor engineering (PMC workarounds, APIC jitter compensation, precision interrupt delivery) doesn't exist in our design.

### Why CNTVCT_EL0 must be trapped (vDSO consideration)

On ARM, the vDSO `clock_gettime` reads CNTVCT_EL0 directly (no syscall). We cannot avoid trapping this by "pre-setting" the counter value before VM entry because:

- CNTVCT_EL0 = CNTPCT_EL0 (physical counter) - CNTVOFF_EL2 (offset)
- The physical counter keeps ticking in real time during guest execution
- Two reads within one VM entry return different values depending on elapsed wall-clock time
- This is non-deterministic across hosts with different CPU speeds

Setting CNTVOFF_EL2 before entry gives a shifted counter, not a frozen one. The counter still drifts with real time. **Trapping is the only option for deterministic counter reads.**

Performance impact: every `clock_gettime()` and kernel-internal timer read causes a VM exit (~1-2us). For V8, mitigate by overriding `Date.now()` and `performance.now()` in the V8 API layer (Convex already does this) so V8 doesn't call `clock_gettime` in hot paths. The kernel's internal timer reads during boot (`calibrate_delay`) will be slow but only happen once, amortized by the snapshot.

### Kernel boot parameters for determinism

```
nokaslr                       # disable kernel address space layout randomization
norandmaps                    # disable mmap base randomization
random.trust_cpu=on           # seed CRNG from RDRAND/RNDR (which we trap -> deterministic)
nosmp                         # single CPU (redundant with single vCPU but explicit)
clocksource=arch_sys_counter  # use the ARM architected timer (which we trap)
nohz=off                      # disable dynamic ticks -- consistent timer interrupts
quiet                         # less boot noise
```

With these parameters + our hardware traps, the kernel initializes identically every boot. ASLR is off, the PRNG is deterministically seeded, the scheduler sees one CPU, and the clock source is our virtual timer.

### Per-fork determinism

Each fork from the snapshot starts with identical VM state. To get different-but-deterministic behavior per invocation:

1. Fork from snapshot (CoW mmap, identical starting state)
2. Write a per-invocation seed to the mailbox: `seed = hash(function_id, invocation_id)`
3. The init process reads this seed and uses it to reseed `/dev/urandom` via `RNDADDENTROPY`
4. Guest code that reads `/dev/urandom` or calls `getrandom()` gets a deterministic sequence derived from the per-invocation seed
5. `clock_gettime()` returns virtual time from the trapped counter, which is deterministic

Two forks with the same seed produce bit-identical execution. Two forks with different seeds produce different (but individually reproducible) execution.

---

## What is a GIC and how we handle it

**GIC = Generic Interrupt Controller.** It's ARM's standard hardware for routing interrupts to CPUs — the ARM equivalent of x86's APIC.

On a real ARM system, when a device (timer, UART, network card) needs to signal the CPU, it asserts an interrupt line. The GIC collects these, prioritizes them, and delivers them to the right CPU core. The current version is GICv3.

It has two main components:

- **Distributor (GICD)**: one per system. Receives all interrupt sources, determines priority and target CPU. Accessed via MMIO registers at a fixed address.
- **Redistributor (GICR)**: one per CPU. Handles per-CPU interrupts (like the virtual timer interrupt, PPI 27). Also MMIO.

The Linux kernel reads the DTB to find the GIC's MMIO addresses, then writes to those registers during early boot to configure interrupts. If nobody responds to those MMIO accesses, the kernel hangs.

**HVF provides built-in GIC emulation.** Apple's Hypervisor.framework has `hv_gic_create()` and `hv_gic_set_spi()` for injecting shared peripheral interrupts. The distributor/redistributor MMIO is handled inside HVF — we don't need to emulate the GIC ourselves. We need to:

1. Describe the GIC in our DTB with the correct MMIO addresses matching HVF's expectations
2. Call `hv_gic_create()` during VM setup
3. Use `hv_gic_set_spi(irq_number)` to inject interrupts (e.g., the virtio-vsock interrupt)

The virtual timer interrupt (PPI 27) is handled via `HV_EXIT_REASON_VTIMER_ACTIVATED`. We intercept this exit, advance virtual time to the deadline, and re-inject the interrupt.

**Risk:** HVF's GIC emulation may not be fully documented. We need to spike this early — build a minimal DTB, try to boot a kernel, and see where it crashes. Use QEMU's DTB dump (`qemu-system-aarch64 -machine virt,dumpdtb=virt.dtb`) as a reference for the expected layout.

---

## Milestones

### M9: Minimal Linux kernel boots in VM (5-7 weeks)

**Goal:** A stripped Linux kernel boots inside our existing hypervisor, reaches userspace, and runs a trivial init that prints "hello" via PL011 serial to the host. Then upgrade to virtio-vsock for production communication.

This is the biggest milestone — it involves kernel config, DTB generation, GIC setup, MMIO emulation, fully virtualized timer, and PSCI. Split into sub-milestones:

#### M9a: Kernel boots to earlycon (weeks 1-2)

The minimum: get kernel boot messages appearing on the host.

**Kernel build:**

- Cross-compile a minimal aarch64 Linux kernel. Start from `tinyconfig` and enable:
  - `CONFIG_SERIAL_AMBA_PL011` (earlycon for boot debugging)
  - `CONFIG_PRINTK` (kernel logs)
  - `CONFIG_BLK_DEV_INITRD` (initramfs support)
  - Disable: everything else (networking, block devices, filesystems, SMP, modules)
- Target: `Image` file ~2-3 MB

**Hypervisor changes:**

- **DTB generation.** Use the `vm-fdt` crate (from rust-vmm, pure Rust, hypervisor-agnostic) to build a minimal device tree:
  - 1 CPU node (single core)
  - Memory node (256 MB at 0x4000_0000)
  - GICv3 at HVF's expected MMIO addresses
  - PL011 UART at a fixed MMIO address (e.g., 0x0900_0000, matching QEMU virt)
  - Chosen node with `bootargs` (determinism parameters) and `stdout-path` pointing to PL011

- **GIC setup.** Call `hv_gic_create()` and configure. Spike first to verify HVF's GIC works and what MMIO addresses it expects.

- **PL011 UART emulation.** When the guest writes to PL011 MMIO addresses, it triggers a data abort (VM exit). Emulate just enough for earlycon:
  - Write handler: print character to host stdout
  - Read handler: return TX-ready status
  - ~50-80 lines of Rust

- **MMIO dispatch.** Data abort exits include the faulting GPA. Route to PL011 or GIC based on address range.

- **PSCI (Power State Coordination Interface).** Linux calls PSCI to discover CPUs and manage power states. Trap HVC calls with PSCI function IDs:
  - `PSCI_VERSION` -> return 1.1
  - `PSCI_CPU_ON` -> return ALREADY_ON for CPU 0
  - `PSCI_SYSTEM_OFF` -> terminate VM
  - `PSCI_FEATURES` -> return supported
  - ~30 lines of Rust

**Deliverable:** Kernel boot messages scroll on host stdout. Kernel likely panics trying to mount rootfs (no initramfs yet). That's fine — earlycon output proves the boot path works.

**Key risk:** GIC configuration. If HVF's GIC doesn't work as expected, this is where we'll find out.

#### M9b: Fully virtualized timer (week 3)

The kernel needs timer interrupts for its scheduler. We must fully virtualize the timer — HVF's real-time vtimer is non-deterministic.

- **Trap timer registers.** Configure CNTHCTL_EL2 to trap:
  - CNTVCT_EL0 reads (EL1TVCT=1) — return virtual counter
  - CNTV_CVAL_EL0, CNTV_TVAL_EL0, CNTV_CTL_EL0 writes (EL1TVT=1) — record deadline, enable/disable

- **Implement VirtualClock.** The adaptive algorithm described above:
  - Fixed increment per counter read
  - WFI warp to next timer deadline
  - Timer interrupt injection when counter >= deadline

- **Handle HV_EXIT_REASON_VTIMER_ACTIVATED.** HVF may still fire this from its internal timer. Translate to our virtual timer model — mask HVF's vtimer and use our own logic.

- **Handle WFI exits.** EC=0x01 in exception syndrome. On WFI, warp time and inject timer interrupt.

- **Timer interrupt injection.** After advancing virtual time past the deadline, inject the virtual timer interrupt (PPI 27) via the GIC before re-entering the VM.

**Deliverable:** Kernel boots past `calibrate_delay` (which reads the counter in a tight loop) without hanging. Timer interrupts fire, scheduler runs.

**Validation:** Kernel `dmesg` shows consistent `BogoMIPS` value across boots (proves timer is deterministic). Boot time is reasonable (not millions of VM exits).

#### M9c: Initramfs + userspace (week 4)

Get to userspace with a minimal init process.

**Initramfs:**

- Statically-linked init binary (Rust, `aarch64-unknown-linux-musl`, ~1-2 MB):
  1. Mount /proc, /sys, /dev (devtmpfs)
  2. Read per-fork seed from kernel command line (e.g., `convex.seed=...`)
  3. Seed /dev/urandom via `RNDADDENTROPY` ioctl
  4. Print "Hello from Linux in the VM!" to serial (write to /dev/ttyAMA0 or stdout)
  5. Execute `HC_EXIT` via a small inline-asm helper (or just `reboot` to trigger PSCI_SYSTEM_OFF)
- Pack into cpio archive, pass to kernel via DTB `linux,initrd-start`/`linux,initrd-end`

**Additional kernel config:**

- `CONFIG_TMPFS` (for /tmp, /dev)
- `CONFIG_DEVTMPFS` + `CONFIG_DEVTMPFS_MOUNT` (auto-populate /dev)
- `CONFIG_PROC_FS` (V8 reads /proc/self/maps)
- `CONFIG_SYSFS` (minimal, needed by some runtimes)

**Deliverable:** `just boot-linux` -> kernel boots, init prints "Hello from Linux in the VM!", VM exits cleanly.

#### M9d: virtio-vsock (weeks 5-7)

Replace PL011 serial with virtio-vsock for production guest-host communication. Serial is fine for console output but too slow for DB queries (~1ms per round-trip vs ~60us for vsock).

**Virtio MMIO transport emulation:**

The guest kernel accesses virtio devices via memory-mapped I/O. When the guest reads/writes a virtio MMIO address, it triggers a data abort. The hypervisor emulates the virtio MMIO register interface:

- Device identification registers (magic, version, device ID, vendor ID)
- Feature negotiation (guest reads/writes feature bits)
- Queue setup (guest configures virtqueue base address, size, notification)
- Queue notification (guest writes to kick the host to process buffers)
- Interrupt status and acknowledgment

The virtqueues themselves are in guest memory — the host reads/writes them via its mmap'd pointer to guest RAM. No separate shared memory mechanism.

**vsock protocol:**

All guest-host communication is framed over vsock:

```
Request (guest -> host):
  [4 bytes] type: u32     // HC_CONSOLE=1, HC_TIME=2, HC_RANDOM=3, HC_DB_READ=0x10, ...
  [4 bytes] payload_len: u32
  [N bytes] payload       // JSON for DB queries, raw bytes for console

Response (host -> guest):
  [4 bytes] status: u32   // 0 = success, nonzero = error
  [4 bytes] payload_len: u32
  [N bytes] payload       // JSON for DB results, u64 for time/random
```

This reuses the same hypercall IDs from phase 1, tunneled over vsock. The init process opens a vsock connection to the host (CID 2) on a well-known port.

**DTB additions:**

- virtio-vsock device node with MMIO address and interrupt number
- The interrupt is a shared peripheral interrupt (SPI) delivered via `hv_gic_set_spi()`

**Estimated size:** ~500-1000 lines of Rust for virtio MMIO transport + vsock device emulation.

**Hybrid HVC option:** For latency-sensitive calls (HC_TIME, HC_RANDOM), the init process could use HVC directly via inline asm instead of vsock round-trips (~200ns vs ~60us). The kernel can expose this via a `/dev/hvc` character device using `CONFIG_HVC_DRIVER`. Use vsock only for bulk data (DB queries, console). This is an optimization to defer until we have benchmarks.

**Fallback:** If virtio-vsock proves too complex, use PL011 serial for all communication. Serial is already working from M9a. The ~1ms latency per DB query is acceptable for MVP — optimize to vsock later.

**Deliverable:** Guest init communicates with host via vsock. DB queries work. Console output works.

**End-of-M9 deliverable:**

```bash
just boot-linux  # kernel boots, init runs, prints hello via vsock/serial
```

### M10: Snapshot + fork with Linux (2-3 weeks)

**Goal:** Snapshot the booted Linux VM (kernel initialized, init running, runtime loaded), fork from it with CoW, run user code in the fork.

**Snapshot:**

- Boot Linux, let init run to completion (runtime initialized, entropy seeded)
- Init signals readiness via vsock message or serial write
- Host pauses VM, saves:
  - All CPU state (general regs, system regs, SIMD via `hv_vcpu_get_reg/sys_reg/simd_fp_reg`)
  - Full guest memory region (write to `template.mem`)
  - Virtual clock state (counter value, timer deadline)
  - Virtio device state (queue configuration, feature bits)
- Serialize to template files (larger than phase 1: ~128-256 MB memory)

**Fork:**

- Same CoW mmap trick as phase 1: `mmap(template.mem, MAP_PRIVATE)`
- Create new VM, map CoW region, restore CPU state
- Restore virtual clock and virtio device state
- Write per-fork seed + JS code to mailbox (a known page in the CoW region)
- Resume — the guest kernel is already running, init reads the mailbox, seeds urandom, execs user code
- Collect output via vsock, destroy VM

**Boot-time amortization:** Linux boot takes ~50-200ms (kernel init + initramfs). Runtime init (V8 isolate creation) takes ~50ms. By snapshotting after both, every fork starts with a fully-initialized system. Fork latency should be comparable to phase 1 (~200-500us) plus the cost of touching more CoW pages during the first few syscalls (~100-200us for kernel data structures). Target: **<1ms total fork-to-first-instruction.**

**What changes from phase 1 fork engine:**

- Template is larger (128-256 MB vs 64 MB) but CoW means RSS stays small
- More CPU state to save/restore (Linux uses more system registers than bare metal)
- Must save/restore virtio device state (queue indices, feature bits)
- Must save/restore virtual clock state (counter value, timer deadline, timer enabled)
- The guest "wakes up" in kernel context (init process was running), not in bare-metal code
- Linux's in-kernel state (scheduler runqueues, timer lists, RCU state) is cloned via CoW — this is fine since CoW preserves exact state, and single-vCPU means no concurrent kernel state

**Determinism validation:**

- Fork twice with same seed, run same JS -> bit-identical stdout
- Fork twice with different seeds -> different `Math.random()` values, same `Date.now()` (virtual time)
- Fork 1000 times, verify all same-seed forks produce identical output

### M11: QuickJS on Linux in VM (1-2 weeks)

**Goal:** QuickJS evaluates JS through the full Linux stack. Validates that the kernel path works end-to-end before attempting V8.

- Compile a musl-static Rust binary embedding `rquickjs` for `aarch64-unknown-linux-musl`
- Pack it into the initramfs as the user runtime
- Init reads JS from the mailbox (or a known file in tmpfs), passes to QuickJS
- `console.log` -> `write(2, ...)` -> vsock -> host stdout
- `Date.now()` -> `clock_gettime(CLOCK_REALTIME)` -> kernel reads trapped CNTVCT -> virtual time
- `Math.random()` -> `getrandom()` -> kernel reads /dev/urandom (deterministically seeded) -> deterministic value
- Stub `db.query()` -> write query to vsock, read response, parse JSON

**Benchmark target:** Fork + JS eval < 1ms (comparable to bare-metal phase 1). The Linux overhead should be small since the kernel is already initialized in the snapshot.

**Deliverable:**
```bash
just snapshot-linux --runtime quickjs
just fork-linux --seed 42 --js 'console.log("hello from QuickJS on Linux!", Math.random())'
# Output: hello from QuickJS on Linux! 0.5765691460087045
# Running again with --seed 42 produces identical output
```

### M12: V8 on Linux in VM (3-4 weeks)

**Goal:** V8 boots inside the Linux VM and evaluates JS. This is the target configuration for Convex production.

- Cross-compile a minimal V8 embedding binary for `aarch64-unknown-linux-musl` (or gnu with static linking). Use `rusty_v8` / Deno's prebuilt static libs to avoid the full V8 build system.
- The V8 runner binary: creates isolate -> creates context -> installs Convex builtins -> signals ready (writes to vsock) -> host snapshots -> on fork, reads JS from mailbox -> evaluates -> writes result to vsock -> exits
- V8-specific determinism configuration:
  - `--predictable` flag (deterministic GC, no idle-time optimization) — use as safety net initially
  - Test without `--predictable` since the hypervisor controls all V8's environmental inputs
  - `Date.now()` and `Math.random()` overridden via V8's API (same as Convex does today)
  - Single-vCPU means V8's background compiler threads are scheduled deterministically by CFS

**V8-specific Linux dependencies** (all handled natively by the kernel):
- `mmap` / `mprotect` with W^X transitions (JIT code pages)
- `clone` with `CLONE_THREAD` (GC threads, compiler threads)
- `futex` (thread synchronization)
- `pipe2` / `eventfd2` (internal signaling)
- `epoll` (platform event loop)
- `/proc/self/maps` (heap layout detection)
- `sched_getaffinity` (CPU count — returns 1)

None of these need special handling — the Linux kernel implements them all. The only thing we ensure is that `/proc/self/maps` returns deterministic content (which it does, since mmap addresses are deterministic with `norandmaps`).

**Snapshot strategy:** Snapshot after V8 isolate + context creation but before user code evaluation. V8 init takes ~50ms but this is amortized across all forks. Per-module snapshots (after importing user's `node_modules`) are a future optimization.

**Benchmark targets:**
- Snapshot creation: < 5 seconds (kernel boot + V8 init)
- Fork + JS eval: < 2ms (V8 is heavier than QuickJS)
- Determinism: same seed -> bit-identical output

**Deliverable:**
```bash
just snapshot-linux --runtime v8
just fork-linux --seed 42 --js 'console.log("V8 in a deterministic VM!", Date.now(), Math.random())'
```

### M13: Arbitrary Linux binaries + containers (ongoing)

**Goal:** Run any statically-linked Linux binary or OCI container image in the VM.

- **Static binaries:** Already work after M10. Cross-compile Go, Rust, C programs for `aarch64-unknown-linux-musl`, pack into initramfs.
- **Python:** Include CPython in the initramfs. `import numpy` works if numpy is pre-installed. For per-invocation package installation, mount a read-only package directory.
- **OCI containers:** Extract an OCI rootfs, use it as the initramfs (or pivot_root into it). The init process becomes the container's entrypoint.
- **Dynamic linking:** Include a musl or glibc dynamic linker in the initramfs. Most containers expect glibc; a minimal rootfs from Alpine or Debian slim works.

This milestone is open-ended — compatibility expands with each new workload tested. The key insight is that bugs are almost always "missing kernel config option" or "missing file in rootfs," not "missing syscall implementation." The kernel handles the syscalls; we just need the right config.

---

## rust-vmm crates

The rust-vmm ecosystem is KVM-centric, but several data-plane crates are hypervisor-agnostic and useful:

| Crate | What | Why we'd use it |
|---|---|---|
| **vm-fdt** | Builds device tree blobs at runtime | Required for aarch64 Linux boot. Pure Rust, no HVF dependency. |
| **vm-memory** | Safe guest memory access (`GuestMemory` traits) | Replace our manual mmap pointer arithmetic with bounds-checked access. |
| **linux-loader** | Loads PE-format kernel images on aarch64 | Parse and load the `Image` file into guest memory. |
| **virtio-queue** | Virtio queue implementation | Implement virtio-vsock without writing queue logic from scratch. |
| **vm-superio** | UART emulation (PL011/16550) | Reference for our PL011 emulation (or use directly). |

Crates we do NOT use:
- **kvm-ioctls / kvm-bindings** — KVM-only, we have HVF
- **vmm-sys-util** — no macOS support (Linux epoll/eventfd only)
- **event-manager** — epoll-only, we'd use kqueue on macOS

For production on Linux/KVM, we'd add `kvm-ioctls` as a backend alongside HVF. The data-plane crates are shared.

---

## Snapshot/restore and rolling deploys

### For ephemeral invocations (< 15s)

Drain-and-restart: stop routing new requests to the old host, let in-flight invocations finish, shut down. Same strategy as AWS Lambda / Firecracker. VMs are ephemeral, so there's nothing to migrate.

### For longer-running requests

Two options, both leveraging determinism:

1. **Checkpoint + replay.** Periodically checkpoint the VM (save CPU state + dirty pages). On host drain, transfer the checkpoint + input log to the new host, replay from the checkpoint by feeding the same inputs. Since execution is deterministic, the replayed VM produces identical state. Transfer size is just the checkpoint + input log, not the full memory.

2. **Snapshot/restore across hosts.** Pause the VM, serialize full state (CPU + memory + device state + virtual clock) to the new host, resume. With CoW memory, only dirtied pages need to transfer. For a JS function that's been running for 30s, dirty pages might be 4-16 MB — transferable in <100ms on a fast network.

We don't need true live migration (pre-copy dirty page tracking with iterative convergence). That's designed for long-lived VMs running databases, not function invocations. Checkpoint + replay or pause-and-transfer with <100ms downtime is sufficient.

---

## Comparison with Antithesis

| | Antithesis | This project |
|---|---|---|
| Hypervisor base | Forked FreeBSD bhyve (2018) | KVM / HVF via Rust (2026) |
| Time model | PMC instruction-retired counting | Adaptive event-driven virtual clock |
| PMC precision issues | ~1 miscount per 10^12 instructions, years of workarounds | Not used at all |
| Interrupt precision | APIC delivery jitter (dozens of instructions), required workarounds | WFI warp — no jitter because we skip to the deadline |
| Core constraint | Single physical core per VM | Single vCPU per VM (same) |
| Guest OS | Full Linux (unmodified containers) | Minimal Linux (same compat, stripped config) |
| I/O model | VMCALL-based custom instruction | HVC hypercalls + virtio-vsock |
| Exploration engine | Multi-VM branching state-space search | Not built (different use case) |
| Use case | Testing distributed systems | Deterministic function execution |
| Snapshot/fork | Yes (for branching) | Yes (for per-invocation isolation, <1ms) |
| Open source | No (announced intent) | Yes (Apache-2.0) |

**The key technical difference is the time model.** Antithesis pegs virtual time to instruction count via PMC, which is necessary for their exploration engine (snapshot and branch at arbitrary instruction boundaries) but introduces hardware-level imprecision that requires years of workaround engineering. We advance time on counter reads and warp on WFI — a much simpler model that satisfies our weaker requirement (deterministic across runs, not proportional to computation).

---

## Schedule

Assuming one engineer, starting from the completed M0-M8 base:

| Milestone | What | Weeks | Cumulative |
|-----------|------|-------|------------|
| **M9a** | Kernel boots to earlycon (DTB, GIC, PL011, PSCI) | 2 | 2 |
| **M9b** | Fully virtualized timer (adaptive clock, WFI warp) | 1 | 3 |
| **M9c** | Initramfs + userspace init | 1 | 4 |
| **M9d** | virtio-vsock device emulation | 2-3 | 6-7 |
| **M10** | Snapshot + fork with Linux | 2-3 | 8-10 |
| **M11** | QuickJS on Linux (validation) | 1-2 | 9-12 |
| **M12** | V8 on Linux | 3-4 | 12-16 |
| **M13** | Arbitrary binaries + containers | Ongoing | -- |

Total to V8-in-deterministic-Linux-VM: **~12-16 weeks.**

The biggest risk is M9a (GIC + DTB). If HVF's GIC emulation is poorly documented or buggy, this sub-milestone could expand. The fallback is to dump QEMU's working DTB and match it exactly, adjusting for HVF's specific addresses.

M9d (virtio-vsock) is the largest chunk of new code. If it's too complex, PL011 serial is a proven fallback with ~1ms latency per round-trip — functional for MVP.

---

## What this enables

Once V8 runs in a deterministic Linux VM with sub-millisecond fork:

- **Convex function execution with hardware isolation.** Every function invocation runs in its own VM. Tenant code is separated from the database by a hardware boundary, not just a V8 isolate.
- **Deterministic re-execution.** Same function + same DB reads + same seed = identical execution. Enables OCC transaction replay, time-travel debugging, reproducible bug reports.
- **Multi-language support.** Python, Go, Rust, Java — anything that runs on Linux runs in the VM. No per-language porting work. The determinism guarantee is provided by the VM, not the language runtime.
- **Antithesis-style testing.** The same deterministic VM can be used for fault injection and property-based testing of Convex itself. Snapshot a known state, inject a fault (network partition, disk failure), verify invariants still hold, branch and explore.
