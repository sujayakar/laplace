# CLAUDE.md

## Project

Custom lightweight hypervisor for running user code deterministically in hardware-isolated VMs. MVP target: aarch64 macOS (Apple Silicon) using Hypervisor.framework. See PLAN.md for full execution plan and milestones.

## Architecture invariants

- **Single vCPU per VM, always.** This is a design constraint for determinism, not a limitation to fix later.
- **No guest OS.** The guest is a bare-metal ELF binary (no_std Rust) running at EL1 with MMU off. No Linux kernel, no initramfs, no device drivers.
- **No device emulation.** No virtio, no UART, no PIC/PIT/IOAPIC, no MMIO devices. All guest↔host communication is through HVC hypercalls.
- **Determinism is the core design goal.** Every source of non-determinism in the guest must be trapped and virtualized. If you're unsure whether something is deterministic, trap it. Performance is secondary to correctness for MVP.
- **VMs are ephemeral.** Each VM runs one function invocation and is destroyed. No long-lived state, no migration mid-execution. The bump allocator that never frees is correct — the VM's entire address space disappears on teardown.

## Hypervisor.framework (HVF) specifics

- Headers: `/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/System/Library/Frameworks/Hypervisor.framework/Headers/`
- The Rust `hypervisor` crate exists but may be outdated. Prefer raw FFI bindings via `bindgen` or hand-written `extern "C"` blocks. The API surface we need is small (~20 functions).
- Key functions: `hv_vm_create`, `hv_vm_destroy`, `hv_vm_map`, `hv_vm_unmap`, `hv_vcpu_create`, `hv_vcpu_destroy`, `hv_vcpu_run`, `hv_vcpu_get_reg`, `hv_vcpu_set_reg`, `hv_vcpu_get_sys_reg`, `hv_vcpu_set_sys_reg`, `hv_vcpu_get_simd_fp_reg`, `hv_vcpu_set_simd_fp_reg`.
- HVF runs the guest at EL1. The hypervisor (HVF itself) is at EL2. HVC instructions from EL1 cause a VM exit with `HV_EXIT_REASON_EXCEPTION`. The ESR syndrome is in `exit->exception.syndrome` (not a sys reg read). EC=0x16 for HVC from AArch64.
- After an HVC exit, PC already points past the HVC instruction (HVF sets it to HVC_addr + 4). Do NOT manually advance PC — that would skip the next instruction. (Validated in M0 spike test.)
- Entitlement required: `com.apple.security.hypervisor` in the entitlements plist. Sign with `codesign --sign - --entitlements entitlements.plist --force target/debug/convex-hypervisor`.
- HVF error codes are `hv_return_t` — always check returns. `HV_SUCCESS = 0`. Non-zero means something failed, often silently.
- HVF is per-process single VM: only one `hv_vm_create` at a time (second returns `HV_BUSY`). Max 64 concurrent vCPUs per VM (returns `HV_NO_RESOURCES` beyond that). (Validated in M0 spike test.) No address space abstraction on arm64 (x86 HVF has `hv_vm_space_create` but arm64 does not). For concurrent VMs on macOS, use sequential VM reuse (~24μs destroy/create cycle). On Linux/KVM, multiple VMs per process via fd — the Zeroboot CoW fork pattern works directly. Production (Linux) gets the fast-fork optimization; macOS dev path skips it.
- `hv_vm_map` maps host virtual address → guest IPA (intermediate physical address). The host address must be page-aligned. Use `mmap` with `MAP_ANON` for fresh regions or `MAP_PRIVATE` on a file for CoW snapshot regions.

## ARM64 register setup for bare-metal guest

Initial vCPU state for a bare-metal EL1 guest with MMU off:

```
PC          = ELF entry point (e.g., 0x4000_0000)
SP          = top of stack region (e.g., 0x4001_0000 for 64 KiB stack)
CPSR/PSTATE = 0x3c5 (EL1h, SPSel=1, DAIF masked — all interrupts disabled)
                NOTE: 0x3c4 is EL1t (uses SP_EL0). Use 0x3c5 for EL1h (uses SP_EL1).
                M0 worked with 0x3c4 because the guest didn't use the stack.
                Any guest code with stack frames (local variables, function calls)
                needs 0x3c5 + SP_EL1 set correctly.
SCTLR_EL1   = 0x30d00800 (MMU off, caches off, no alignment check)
                bit 0 = 0 (M: MMU off)
                bit 2 = 0 (C: data cache off)  
                bit 12 = 0 (I: instruction cache off)
                Keep other bits at reset defaults.
HCR_EL2     = configured by HVF; you can set trap bits via hv_vcpu_set_sys_reg:
                TID3 = 1 (trap ID register reads for deterministic CPUID)
```

For determinism traps, set these system registers on the vCPU:
```
CNTHCTL_EL2.EL1PCTEN = 0   → trap reads of CNTVCT_EL0 (virtual timer counter)
CNTHCTL_EL2.EL1PCEN  = 0   → trap reads of CNTPCT_EL0 (physical timer counter)
MDCR_EL2.TPM         = 1   → trap performance monitor access
```

Note: HVF may not expose all EL2 registers for direct manipulation. Some trap configuration may need to go through `hv_vcpu_config_t` at vCPU creation time. Check HVF headers for `hv_vcpu_config_create` and `HV_FEATURE_REG_*` constants. If a trap can't be set via HVF's API, the fallback is to not trap and instead handle it at the guest level (e.g., guest micro-runtime never reads the timer directly, always uses hypercalls).

## Hypercall protocol

Guest executes `HVC #0`. Arguments in x0-x3, return value in x0.

```
HC_CONSOLE  = 0x01   // x1 = ptr to string in guest mem, x2 = length
HC_TIME     = 0x02   // returns: virtual timestamp in nanoseconds
HC_RANDOM   = 0x03   // returns: 8 bytes of deterministic random
HC_DB_READ  = 0x10   // x1 = request length in mailbox. returns: response length
HC_DB_WRITE = 0x11   // x1 = request length in mailbox. returns: 0 or error
HC_ALLOC    = 0x20   // x1 = size in bytes. returns: guest physical address
HC_READY    = 0xFE   // guest signals initialization complete (snapshot point)
HC_EXIT     = 0xFF   // x1 = exit code. 0 = success. result in mailbox
```

When handling HC_CONSOLE and HC_DB_*, the host reads/writes the guest's memory directly through its mmap'd pointer. The "mailbox" is just a known region in guest memory (e.g., 64 KiB at GPA 0x4010_0000), not a separate shared memory mechanism.

## Guest memory layout

```
0x4000_0000  .text (guest code)          variable, ~2-5 MB with Boa
0x4xxx_xxxx  .rodata, .data, .bss        follows .text
0x4010_0000  mailbox (64 KiB)            shared with host for bulk data
0x4020_0000  heap start                  bump allocator grows upward
0x4200_0000  heap end / stack guard      32 MiB heap region
0x4200_0000  stack bottom                grows downward
0x4201_0000  stack top (initial SP)      64 KiB stack
```

These addresses are arbitrary but must be consistent between the linker script, the host's VM memory mapping, and the guest's allocator/stack setup. Pick them once and don't change them.

## Guest binary build

Target: `aarch64-unknown-none` (Rust tier 2 target, available in nightly).

```bash
cargo build -p convex-guest --target aarch64-unknown-none --release
# produces: target/aarch64-unknown-none/release/convex-guest (ELF)
```

Use `aarch64-unknown-none` (not `aarch64-unknown-none-softfloat`) — we want hardware FP/SIMD.

The linker script must place `.text` at the base GPA (0x4000_0000). The ELF entry point is `_start`, which is a small assembly stub that sets SP and calls Rust `main`.

For loading into the VM: either parse the ELF and copy PT_LOAD segments to the right offsets, or use `objcopy -O binary` to produce a flat binary and memcpy it to the base GPA. Flat binary is simpler for MVP.

## Key risks flagged in PLAN.md

1. **HVF + MAP_PRIVATE CoW**: ✅ VALIDATED. Works perfectly — MAP_PRIVATE gives full isolation between VMs, original file unmodified. (M0 spike test)
2. **Boa no_std**: the feature flag exists but may be broken. The plan is to build M0-M2 without Boa (pure Rust guest) and add it in M3. If Boa doesn't work in no_std, alternatives: mujs (C, tiny), boa with std stubs, or a minimal expression evaluator.
3. **HVF VM creation latency**: ✅ VALIDATED. Full setup (vm_create + vm_map + vcpu_create + reg setup) is ~35μs p50, ~89μs p99 — way under the 500μs target. (M0 spike test)
4. **HVF concurrent VM limits**: ✅ VALIDATED. Per-process single VM (second hv_vm_create returns HV_BUSY). Max 64 vCPUs. For concurrent isolates, we'll need separate processes or sequential VM reuse. (M0 spike test)

## Code style

- Rust 2021 edition, nightly toolchain.
- `thiserror` for error types on the host side. `core::fmt` for guest errors (no_std).
- No `unwrap()` on HVF calls — always propagate errors. HVF failures are often silent/confusing.
- Prefer explicit types over inference for register values and GPA addresses — these are easy to get wrong and hard to debug.
- Test with `#[cfg(test)]` on the host side. Guest code can't use the standard test harness — test by running in a VM and checking output.

## Reference code

- **Zeroboot** (github.com/zerobootdev/zeroboot): KVM fork engine, CoW mmap, CPU state restore order. The patterns translate to HVF with different API calls.
- **crosvm** (chromium.googlesource.com/chromiumos/platform/crosvm): HVF backend in `src/hypervisor/haxm/` and Apple-specific code. Best reference for HVF Rust FFI.
- **libkrun** (github.com/containers/libkrun): Another HVF backend in Rust. Good reference for the ARM64 register setup dance.
- **Hyperlight** (github.com/hyperlight-dev/hyperlight): Microsoft's no-OS micro-VM. Similar architecture (bare-metal guest, hypercalls for everything, no device emulation). They target KVM and Windows WHV, not HVF, but the guest-side patterns are relevant.