# Future Work

## Production readiness

### Real Convex DB integration
Replace stub `db.query()` with actual database calls. Requires a host↔guest communication channel faster than the current pipe IPC (see below).

### Seccomp/jailer
The host process has `/dev/kvm` access. Must sandbox it before running untrusted tenant code. Firecracker's jailer is a good reference: chroot + seccomp + cgroup limits.

### Multi-VM serving
Current serve mode is single-threaded, one VM at a time. For production: thread pool where each thread creates VMs from the shared template memfd. The memfd is already shareable across threads.

### Memory overcommit / KSM
With `MAP_NORESERVE` + KSM (Kernel Samepage Merging), hundreds of VMs can share snapshot pages. Only dirty pages cost memory (~2-4MB per invocation). Enable KSM via `/sys/kernel/mm/ksm/run`.

## Performance

### Faster guest IPC
The 3.5ms guest execution time is mostly pipe IPC overhead (init reads inbox → pipes to V8 stdin → V8 evals → stdout → init reads → writes outbox). Options:
- **Shared memory**: V8 reads/writes directly from inbox/outbox via `/dev/mem` mmap, bypassing pipes entirely.
- **Virtio-vsock**: Standard guest-host socket. ~60µs round-trip vs ~1ms for pipe+serial.
- **Hypercall-based**: V8 does HVC/BRK directly for I/O, skipping the kernel's syscall path.

### Test on real KVM (AWS Graviton)
Validate native CNTHCTL_EL2 trapping (no BRK exits). Expected: guest execution drops from 3.5ms to ~2ms, total fork ~2.5ms.

### x86-64 support
The hypervisor abstraction layer is designed for this — add `hypervisor/kvm_x86.rs` backend. Key differences from aarch64:
- **Interrupt controller**: LAPIC/IOAPIC instead of GICv3
- **Timer**: APIC timer or TSC instead of ARM architected timer. TSC deadline mode for determinism.
- **Boot protocol**: bzImage/vmlinux loading instead of ARM64 Image. Different DTB (or no DTB — use ACPI).
- **Register encoding**: `KVM_GET/SET_REGS` + `KVM_GET/SET_SREGS` instead of `KVM_GET_ONE_REG`
- **Serial**: 8250 UART at 0x3f8 (I/O ports) instead of PL011 MMIO. The `uart8250.rs` is already written.
- **MMIO vs PIO**: x86 has both port I/O (`KVM_EXIT_IO`) and MMIO (`KVM_EXIT_MMIO`)
- **Deterministic timer**: TSC can be trapped via VMX controls. Alternatively, use `tsc=unstable` kernel param + KVM's TSC scaling.

Zeroboot (github.com/zerobootdev/zeroboot) is x86-only and achieves 0.8ms — good reference implementation.

## Long-lived requests

### Memory hotplug
Let the guest grow beyond the initial 512MB without re-snapshotting. KVM supports adding memory slots at runtime via `KVM_SET_USER_MEMORY_REGION`. The guest kernel needs `CONFIG_MEMORY_HOTPLUG`. Could use virtio-mem for clean integration.

### Live migration
For requests running >15s: pause VM → serialize full state → transfer to new host → resume. We have most of the capture machinery. Missing pieces:
- **GIC/ICC state save/restore on KVM** (currently stubs in kvm.rs)
- **vtimer offset save/restore** (currently stubs)
- **Dirty page tracking** via `KVM_MEM_LOG_DIRTY_PAGES` for incremental transfer
- **Network transport** for state + dirty pages

Simpler alternative for our use case: **checkpoint + replay**. Since execution is deterministic, transfer the checkpoint + input log to the new host and replay. Transfer size is just the checkpoint + inputs, not the full memory.

## Determinism hardening

### RNDR / ID register trapping
We trap the timer but not:
- `RNDR` / `RNDRRS` (hardware random number generator) — source of non-determinism if guest reads it directly
- `ID_AA64*` registers (CPU feature identification) — should return fixed values for reproducibility across different CPU models
- `CNTFRQ_EL0` (counter frequency) — currently real hardware value, should be fixed

On KVM with CNTHCTL_EL2, these can be trapped via `HCR_EL2.TID3` (ID registers) and by not advertising RNDR in ID registers.

### Determinism fuzzing
Run the same JS 1000 times with the same seed, diff all outputs byte-for-byte. Any divergence indicates a non-determinism bug. Automate this as a CI test.

### Entropy seeding
After fork, reseed `/dev/urandom` via `RNDADDENTROPY` ioctl with a per-invocation seed derived from `hash(function_id, invocation_id)`. Currently not implemented — the guest inherits the snapshot's entropy pool.

## Architecture improvements

### Proper error handling
Replace panics with `Result` propagation throughout the hypervisor. Key areas:
- Register access (currently panics on failure)
- GIC device attribute setup
- DTB generation
- Snapshot deserialization

### GIC/ICC/vtimer state on KVM
Complete the stub implementations in `kvm.rs`:
- `GicHandle::save_state()` / `restore_state()` via `KVM_DEV_ARM_VGIC_GRP_DIST_REGS` / `_REDIST_REGS`
- `GicHandle::get_icc_reg()` / `set_icc_reg()` via `KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS`
- `VcpuHandle::set_vtimer_offset()` / `get_vtimer_offset()` via CNTVOFF_EL2

### Move Phase 1 code out of main.rs
The 300-line Phase 1 bare-metal module is cfg-gated in main.rs. Should be in its own file (`phase1.rs`) for clarity.

## Multi-language support

### Python runtime
Include CPython in the initramfs. Snapshot after `import` completes. Fork latency independent of import time (same as V8 bundle preloading).

### Arbitrary containers
Extract an OCI rootfs, use it as the initramfs (or pivot_root). The init process becomes the container's entrypoint. `CONFIG_OVERLAYFS` for layered images.
