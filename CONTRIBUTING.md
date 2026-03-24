# Contributing

## Prerequisites

**macOS:**
- macOS 13+ on Apple Silicon (M1/M2/M3/M4)
- Xcode Command Line Tools (for Hypervisor.framework headers)
- `zig` (`brew install zig`) — for cross-compiling the V8 runner

**Linux (aarch64):**
- KVM support (`/dev/kvm` must exist)
- gcc (for linking the V8 runner natively)

**Both platforms:**
- Rust stable toolchain
- `just` command runner

Additional Rust targets:
```bash
rustup target add aarch64-unknown-none          # for init
rustup target add aarch64-unknown-linux-musl    # for Boa runner
rustup target add aarch64-unknown-linux-gnu     # for V8 runner
```

## Building

```bash
# Host binary
cargo build -p convex-hypervisor --release

# macOS only: codesign with hypervisor entitlement
codesign --sign - --entitlements entitlements.plist --force target/release/convex-hypervisor

# Init (no_std, runs as PID 1 inside the VM)
cd init && cargo build --target aarch64-unknown-none --release

# Boa runner (musl-static, pure Rust JS engine)
cd runner-js && cargo build --target aarch64-unknown-linux-musl --release

# V8 runner (glibc)
# On macOS: needs zig cc (uncomment linker in runner-v8/.cargo/config.toml)
# On Linux/aarch64: builds natively
cd runner-v8 && cargo build --target aarch64-unknown-linux-gnu --release
```

## Testing

```bash
# Run all host tests (41 tests)
cargo test -p convex-hypervisor

# End-to-end (Linux): snapshot + fork with V8
./scripts/mkinitramfs.sh init/target/aarch64-unknown-none/release/convex-init /tmp/initramfs.cpio runner-v8/target/aarch64-unknown-linux-gnu/release/convex-runner-v8
cargo run -p convex-hypervisor --release -- snapshot-linux kernel/Image-arm64 --initrd /tmp/initramfs.cpio --quiet /tmp/template
cargo run -p convex-hypervisor --release -- fork-linux --msg 'console.log(1+2)' /tmp/template

# Serve mode (multiple requests, amortized startup)
echo 'console.log(1+1)' | cargo run -p convex-hypervisor --release -- serve-linux /tmp/template
```

## Code organization

| Crate | Target | Description |
|-------|--------|-------------|
| `host/` (convex-hypervisor) | native | Hypervisor host: cross-platform VM lifecycle, fork engine |
| `init/` (convex-init) | `aarch64-unknown-none` | no_std PID 1 for the Linux VM. Raw syscalls only. |
| `runner-v8/` (convex-runner-v8) | `aarch64-unknown-linux-gnu` | V8 JS runner, dynamically linked against glibc |
| `runner-js/` (convex-runner-js) | `aarch64-unknown-linux-musl` | Boa JS runner, statically linked |
| `guest/` (convex-guest) | `aarch64-unknown-none` | Phase 1 bare-metal guest with QuickJS |
| `shared/` (convex-shared) | no_std | Hypercall IDs and memory layout constants |

## Key constants that must stay in sync

| Constant | Files |
|----------|-------|
| Inbox GPA (`0x3F00_0000`, 8 MiB) | `init/src/main.rs`, `host/src/linux_boot.rs` |
| Outbox GPA (`0x3F80_0000`, 8 MiB) | `init/src/main.rs`, `host/src/linux_boot.rs` |
| Ready pipe fd (3) | `init/src/main.rs`, `runner-v8/src/main.rs`, `runner-js/src/main.rs` |
| READY sentinel (`CONVEX_READY`) | `init/src/main.rs`, `host/src/linux_boot.rs` |
| GIC addresses (GICD/GICR) | `host/src/dtb.rs` |
| UART address (`0x0900_0000`) | `host/src/dtb.rs`, `host/src/pl011.rs`, kernel bootargs |
| Guest RAM base (`0x4000_0000`) | `host/src/linux_boot.rs` |
