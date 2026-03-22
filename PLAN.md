# Convex hypervisor MVP: execution plan

## Goal

Get a minimal "JS running deterministically in a hardware-isolated VM with hypercalls" demo working on a MacBook (aarch64 macOS). The guest is a `no_std` Rust ELF binary embedding Boa that evaluates user JS, with `console.log`, `Date.now()`, `Math.random()`, and a placeholder `db.query()` routed through hypercalls to the host. The host can fork new VMs from a snapshot in <1 ms.

This is the smallest thing that proves the full stack: hypervisor shell, guest micro-runtime, hypercall interface, determinism controls, and snapshot/fork. Everything after this (V8, multi-arch, Linux/KVM, micro-kernel, multi-language) is additive — the interfaces designed here don't change.

## Non-goals for MVP

- Linux/KVM backend (phase 2)
- x86-64 support (phase 2)
- Real Convex DB integration (just stub responses)
- Micro-kernel / Linux syscall emulation (phase 4)
- Multi-vCPU (never, by design)
- Networking in guest (never, by design)
- Production hardening, security audit, jailer

## Architecture

```
┌─────────────────────────────────────────┐
│  Guest VM (aarch64 ELF, no OS)         │
│                                         │
│  User JS code                           │
│       ↓                                 │
│  Boa interpreter (no_std)               │
│       ↓                                 │
│  convex-guest SDK                       │
│    console.log → HC_CONSOLE             │
│    Date.now()  → HC_TIME                │
│    Math.random → HC_RANDOM              │
│    db.query()  → HC_DB_READ             │
│    return val  → HC_EXIT                │
│       ↓                                 │
│  hypercall shim (HVC #0 instruction)    │
└────────────┬────────────────────────────┘
             │ trap (EL2 → HVF)
┌────────────▼────────────────────────────┐
│  Host process (macOS, Rust)             │
│                                         │
│  Hypervisor shell (Hypervisor.framework)│
│    VM create, memory map, vCPU run      │
│       ↓                                 │
│  Hypercall dispatch                     │
│    HC_CONSOLE → print to host stdout    │
│    HC_TIME    → return virtual time     │
│    HC_RANDOM  → return det. PRNG value  │
│    HC_DB_READ → return stub JSON        │
│    HC_EXIT    → halt VM, read result    │
│       ↓                                 │
│  Fork engine                            │
│    mmap(MAP_PRIVATE) snapshot memory    │
│    restore CPU state via HVF            │
└─────────────────────────────────────────┘
```

## Milestones

### M0: Hello world from a VM (week 1-2) ✅ DONE

**Goal:** A bare aarch64 ELF binary executes a single `HVC #0` instruction inside a Hypervisor.framework VM. The host traps it and prints "hypercall received." This proves the HVF plumbing works.

**Host side (Rust):**

- [x] Call `hv_vm_create(NULL)` to create a VM.
- [x] Allocate a 4 MiB region with `mmap(MAP_ANON)`. Copy the guest ELF's loadable segments into it at the right offsets.
- [x] Call `hv_vm_map(host_ptr, guest_phys_addr=0x4000_0000, size, HV_MEMORY_READ | HV_MEMORY_EXEC)` to map guest memory. Use a fixed base GPA — no need for dynamic layout.
- [x] Create a vCPU with `hv_vcpu_create(&vcpu, &exit, NULL)`.
- [x] Set up initial CPU state: `PC` = ELF entry point, `SP` = top of a 64 KiB stack region within the 4 MiB mapping, `CPSR` = EL1h (SPSel=1, EL=1, no interrupts). Set `SCTLR_EL1` with MMU off (identity-mapped physical addresses — no page tables needed for MVP).
- [x] Enter `hv_vcpu_run(vcpu)` loop. Check `exit->reason`:
  - [x] `HV_EXIT_REASON_EXCEPTION`: read syndrome from exit struct. If it's an HVC (EC=0x16), extract x0-x3 for hypercall args. HVF auto-advances PC (no manual +4 needed). Return result in x0.
  - [ ] `HV_EXIT_REASON_CANCELED` / `HV_EXIT_REASON_VTIMER_ACTIVATED`: handle or ignore. *(deferred — not encountered yet)*
  - [x] Anything else: panic with diagnostics for now.

**Guest side (Rust, `no_std`, `no_main`):**

- [x] Custom linker script placing `.text` at `0x4000_0000`, `.bss` + stack after it. Output: minimal ELF (loaded by goblin on host).
- [x] `_start` in assembly: zero BSS, call Rust `guest_main`. SP is set by host via HVF.
- [x] `guest_main` calls `hypercall(HC_CONSOLE, ptr_to_string, len, 0)` → HVC #0.
- [x] The hypercall shim (inline asm with `hvc #0`).

**Deliverable:** ✅ `just run` → "Hello from guest VM!" printed by the host after trapping the HVC.

**Key risks / research needed:**

- [x] HVF's exception routing for HVC at EL1 — **confirmed**: exits to host with EC=0x16. No `hv_vcpu_config_t` needed.
- [x] ELF loading — using `goblin` crate to parse ELF and copy PT_LOAD segments. No objcopy needed.
- [x] Apple entitlement signing — ad-hoc `codesign --sign - --entitlements` works. Automated in Justfile.

**Findings:**
- HVF auto-advances PC past HVC (sets PC = HVC_addr + 4). Do NOT manually advance.
- CPSR=0x3c5 (EL1h, uses SP_EL1). 0x3c4 is EL1t (uses SP_EL0) — works only if guest never touches the stack.
- ESR syndrome is in `exit->exception.syndrome`, not via `hv_vcpu_get_sys_reg`.

### M0 pre-work: Spike validation tests ✅ DONE

- [x] Basic HVC trap: guest executes HVC #0, host sees exception exit with EC=0x16.
- [x] CoW fork semantics: MAP_PRIVATE file mappings give independent copies per VM, original file unmodified.
- [x] VM creation latency: ~35μs p50 for full setup (vm_create + vm_map + vcpu_create + reg setup). Way under 500μs target.
- [x] Concurrent VM limits: one VM per process (second hv_vm_create returns HV_BUSY). Max 64 vCPUs per VM.

### M1: Det. time + random + trap config (week 2-3) — revised milestone ✅ DONE

**Goal:** Guest reads time/random via hypercalls, results are deterministic. Same seed → identical output, different seed → different output.

**Host side:**

- [x] Virtual time state: frozen `u64` timestamp in nanoseconds (1700000000000000000 ns).
- [x] Deterministic PRNG: ChaCha8Rng seeded at VM creation. Each `HC_RANDOM` call returns next u64.
- [x] `--seed` CLI argument to control PRNG seed.
- [x] `VmState` struct holding per-VM deterministic state (time + rng).

**Guest side:**

- [x] `get_time()` → `hypercall(HC_TIME, 0, 0, 0)` returns frozen timestamp.
- [x] `get_random()` → `hypercall(HC_RANDOM, 0, 0, 0)` returns deterministic u64.
- [x] `print_u64` helper for no_std decimal output.
- [x] Guest prints time and random values to prove they work.

**Determinism validation:**

- [x] Same seed (42) → identical stdout output.
- [x] Different seed (99) → different random values, same frozen time.
- [x] Unit test: `test_vm_state_determinism` — same seed → same PRNG sequence.
- [x] Integration test: `test_determinism_end_to_end` — runs host binary twice, compares stdout.

**ARM-specific trapping:**

- [ ] Trap guest reads of `CNTVCT_EL0` / `CNTPCT_EL0` via CNTHCTL_EL2. *(deferred — requires EL2 enabled in VM config, macOS 15+. Guest cooperates via hypercalls so not needed for determinism.)*
- [ ] Trap `RNDR` / `RNDRRS` reads via HCR_EL2. *(deferred — same reason.)*
- [ ] Trap ID register reads via HCR_EL2.TID3. *(deferred — same reason.)*

### M2: Snapshot + CoW fork (week 3-5) — revised milestone ✅ DONE

**Goal:** Snapshot a VM at HC_READY, fork from it via MAP_PRIVATE CoW, resume execution. Benchmark fork latency.

**Snapshot (one-time):**

- [x] Run guest until HC_READY hypercall.
- [x] Save all CPU state via HVF:
  - [x] General registers (x0-x30, PC, CPSR, FPCR, FPSR) via `hv_vcpu_get_reg`
  - [x] System registers (SCTLR_EL1, SP_EL1, CPACR_EL1, etc.) via `hv_vcpu_get_sys_reg`
  - [x] FP/SIMD registers (v0-v31) via `hv_vcpu_get_simd_fp_reg`
- [x] Write guest memory region to file (`guest.mem`).
- [x] Serialize CPU state to file (`cpu.state`).

**Fork (<1 ms):**

- [x] `mmap(template.mem, MAP_PRIVATE)` — CoW memory.
- [x] `hv_vm_create` + `hv_vm_map` the CoW region.
- [x] `hv_vcpu_create` + restore all saved CPU state.
- [x] Write request/seed to a known offset in CoW memory (mailbox).
- [x] `hv_vcpu_run` — guest resumes from after HC_READY.
- [x] Collect output and exit code.
- [x] Destroy VM + unmap.

**Guest side:**

- [x] Add HC_READY call after initialization.
- [x] After HC_READY, read request from mailbox, do computation, HC_EXIT with result.

**Benchmark:**

- [x] Fork 1000 times, measure per-fork latency (target: <1 ms p50).
  - Result: **154μs p50**, 284μs p99 — 6.5x under the 1ms target!

**Tests:**

- [x] Fork produces correct output (mailbox value passed through as exit code).
- [x] Fork with same seed → deterministic output (`test_fork_determinism`).
- [x] Fork with different seed → different output (`test_fork_determinism`).
- [x] Multiple sequential forks work (benchmark runs 1000 sequential forks).

**CLI commands added:**
- `just snapshot` — creates template at `/tmp/hvf-template/`
- `just fork --seed N --mailbox N` — forks from template
- `just bench --iterations N` — benchmarks fork latency

### M3: Allocator + JS engine integration (week 5-7) — revised milestone ✅ DONE

**Goal:** Guest binary initializes a JS engine, evaluates JS, output appears on host via HC_CONSOLE.

**Key decision:** Boa was abandoned — no `no_std` support (deep std dependencies: thread_local, dashmap, async). **QuickJS-NG** was chosen instead: tiny C library (~200KB), designed for bare-metal embedding. Required a freestanding libc shim (~600 lines of C stubs).

- [x] Bump allocator in guest (GlobalAlloc over fixed region, non-atomic for bare-metal).
- [x] QuickJS-NG cross-compiled for aarch64-unknown-none with freestanding libc stubs.
- [x] Wire `console.log` to HC_CONSOLE.
- [x] Wire `Date.now()` to HC_TIME (deterministic virtual time).
- [x] Wire `Math.random()` to HC_RANDOM (deterministic seeded PRNG).
- [x] Evaluate `console.log("hello " + (1 + 2))` → prints "hello 3" on host.
- [x] `--js` flag on fork command for passing JS code via mailbox.
- [x] Exception vector table for debugging guest EL1 exceptions.

**Critical bugs found and fixed:**
- Atomic operations (ldxr/stxr) don't work with MMU/caches off → replaced with plain UnsafeCell.
- Clang generates SIMD struct copies (ldp q0,q1) requiring 16-byte alignment, but data only 8-byte aligned → fixed with `-mstrict-align`.
- VBAR_EL1=0 masked real exceptions by vectoring to unmapped 0x200 → installed exception vector table.
- CPACR_EL1 not set → SIMD/FP disabled at EL1 → now set to enable FP/SIMD.
- `__APPLE__` defined during cross-compilation caused QuickJS to call nonexistent `malloc_size()`.

**Benchmark:** Fork+run with QuickJS init: ~548μs p50, ~934μs p99 (still under 1ms target).

**Tests:** 12 tests pass (8 unit + 4 integration including JS eval, Date.now, Math.random determinism).

### M4: Mailbox + stub db.query (week 7-8) — revised milestone ✅ DONE

**Goal:** Guest JS calls `db.query("users")`, gets stub data back via mailbox.

- [x] 64 KiB mailbox region at known GPA (0x4010_0000) — already in place from M2/M3.
- [x] Guest writes request (collection name) to mailbox, does HC_DB_READ.
- [x] Host reads request, writes canned JSON response to mailbox, returns length in x0.
- [x] Guest reads response, parses as JS value with `JS_ParseJSON`.
- [x] Deliverable: `db.query("users")` returns `[{id:1,name:"Alice"},{id:2,name:"Bob"}]`.
- [x] Stub responses for "users", "posts", and empty array for unknown collections.

**Tests:** 15 tests pass (added 3 new: db.query users, unknown collection, db+random determinism).

### M5: End-to-end demo + benchmarks (week 8-9) — revised milestone ✅ DONE

**Goal:** Polished demo: template creation, sub-ms fork, deterministic JS, DB hypercalls.

- [x] CLI: `snapshot`, `fork --seed --js`, `bench --iterations --js`, `demo`.
- [x] Template creation (boot → QuickJS init → HC_READY → snapshot).
- [x] Fork from template, run JS, verify determinism.
- [x] Benchmark: fork p50/p99, throughput, JS eval overhead, memory info.
- [x] Prove: same seed → bit-identical output across runs.
- [x] `just demo` — single command showcasing the full stack.

**Benchmark results (debug build, macOS Apple Silicon):**
- Empty fork+run: ~501μs p50, ~1872 exec/sec
- Fork+JS eval with db.query: ~685μs p50, ~1398 exec/sec
- JS eval overhead: ~184μs per invocation
- Template size: 64 MiB (guest.mem), per-fork CoW: ~0 MiB (MAP_PRIVATE)

**Tests:** 15 tests pass (8 unit + 7 integration).

## Repo structure

```
convex-hypervisor/
├── Cargo.toml                    # workspace
├── host/
│   ├── Cargo.toml                # host binary
│   └── src/
│       ├── main.rs               # CLI: template, exec, bench
│       ├── hvf/
│       │   ├── mod.rs            # Hypervisor.framework bindings
│       │   ├── vm.rs             # VM create, map, destroy
│       │   └── vcpu.rs           # vCPU create, get/set state, run
│       ├── hypervisor.rs         # Hypervisor trait (for future KVM backend)
│       ├── fork.rs               # Snapshot + CoW fork engine
│       ├── hypercall.rs          # Dispatch table
│       ├── determinism.rs        # Virtual time, deterministic PRNG
│       └── mailbox.rs            # Shared memory read/write
├── guest/
│   ├── Cargo.toml                # no_std guest binary
│   ├── aarch64-unknown-none.json # custom target spec (if needed)
│   ├── linker.ld                 # linker script: .text at 0x4000_0000
│   └── src/
│       ├── main.rs               # _start → init Boa → HC_READY → eval loop
│       ├── alloc.rs              # bump allocator
│       ├── hypercall.rs          # HVC #0 wrapper
│       ├── mailbox.rs            # read/write shared page
│       ├── bindings.rs           # Boa NativeFunction registrations
│       └── panic.rs              # panic handler (HC_EXIT with error)
├── shared/
│   └── protocol.rs               # Hypercall IDs, mailbox format
└── tests/
    ├── determinism.rs             # same-seed-same-output tests
    ├── fork_bench.rs              # fork latency benchmarks
    └── js_eval.rs                 # JS feature coverage tests
```

## Open questions to resolve before starting

**Q1: Does Boa actually work in `no_std` today?**

The Boa repo has a `no_std` feature flag, but its maintenance status is unclear. Before committing to this plan, spend 2-3 hours trying to compile Boa for `aarch64-unknown-none` with a bump allocator. If it doesn't work, options are: (a) fix it upstream (Boa is Apache-2.0, active contributors), (b) use a simpler JS engine (mujs compiled to Rust via FFI, or a minimal hand-rolled expression evaluator for MVP), (c) skip Boa for M0-M2 and just have the guest binary do hypercalls directly from Rust — prove the hypervisor works without JS, then add Boa.

Recommendation: **start M0-M2 without Boa.** ✅ This is what we did.

**Q2: HVF CoW behavior with `MAP_PRIVATE`.** ✅ VALIDATED

Validated in spike test. MAP_PRIVATE gives perfect isolation between VMs, original file unmodified.

**Q3: HVF per-process VM limits.** ✅ VALIDATED

One VM per process (second hv_vm_create returns HV_BUSY). Max 64 vCPUs. On macOS, use sequential VM reuse (~24μs destroy/create cycle). On Linux/KVM, multiple VMs per process via fd — the Zeroboot CoW fork pattern works directly.

**Q4: Guest memory model (MMU on or off?).**

For MVP, running with MMU off (identity-mapped physical addresses) is vastly simpler — no page tables, no TLB management. Boa doesn't need virtual memory. The tradeoff: no memory protection within the guest (a buggy JS function could corrupt Boa's internals). For MVP this is fine — the VM boundary protects the host regardless. Turn on MMU in a later phase when/if we add the micro-kernel.

Recommendation: **MMU off for MVP.** ✅ This is what we did.

## Revised milestone summary

After reviewing the plan, the main change is: decouple the hypervisor work (M0-M2) from the JS engine work (M3 onward). This lets us validate the hardest/riskiest piece (HVF, hypercalls, CoW fork) without depending on Boa's `no_std` support.

| Milestone | What | Weeks | Deliverable | Status |
|-----------|------|-------|-------------|--------|
| **M0** | VM boots, HVC trapped, hello world | 1-2 | Rust guest does hypercall, host prints message | ✅ Done |
| **M1** | Det. time + random + trap config | 2-3 | Guest reads time/random via hypercalls, results deterministic | ✅ Done |
| **M2** | Snapshot + CoW fork | 3-5 | Fork from snapshot in <1 ms, benchmark harness | ✅ Done (154μs p50) |
| **M3** | Allocator + QuickJS integration | 5-7 | JS eval inside VM, console.log via HC_CONSOLE | ✅ Done (548μs p50) |
| **M4** | Mailbox + stub db.query | 7-8 | JS calls db.query(), gets stub data back | ✅ Done |
| **M5** | End-to-end demo + benchmarks | 8-9 | Polished CLI, determinism tests, fork benchmarks | ✅ Done |

Total: **~9 weeks** for one engineer working full-time, with ~2 weeks of buffer for the Boa `no_std` risk and HVF surprises.

## Dependencies

- [x] Rust nightly (for `#![no_std]` + `asm!` + custom targets) — actually works on stable!
- [x] Xcode command line tools (for Hypervisor.framework headers)
- [x] Apple Developer certificate (for entitlement signing — ad-hoc is fine for local dev)
- [x] QuickJS-NG (replaces Boa — no `no_std` support for Boa) — used for M3
- [x] macOS 13+ (Ventura — HVF aarch64 API is stable from here)
