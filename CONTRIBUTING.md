# Contributing

## Prerequisites

- macOS 13+ on Apple Silicon (M1/M2/M3/M4)
- Rust nightly toolchain
- `just` command runner (`brew install just`)
- `zig` (`brew install zig`) — for cross-compiling the V8 runner
- Xcode Command Line Tools (for Hypervisor.framework headers)

Additional Rust targets:
```bash
rustup target add aarch64-unknown-none          # for init
rustup target add aarch64-unknown-linux-musl    # for Boa runner
rustup target add aarch64-unknown-linux-gnu     # for V8 runner
```

## Building

```bash
# Host (macOS binary, auto-codesigned with hypervisor entitlement)
cargo build -p convex-hypervisor --release
codesign --sign - --entitlements entitlements.plist --force target/release/convex-hypervisor

# Init (no_std, runs as PID 1 inside the VM)
cd init && cargo build --target aarch64-unknown-none --release

# Boa runner (musl-static, pure Rust JS engine)
cd runner-js && cargo build --target aarch64-unknown-linux-musl --release

# V8 runner (glibc, cross-compiled with zig)
cd runner-v8 && PATH="$PATH:../scripts" cargo build --target aarch64-unknown-linux-gnu --release
```

## Testing

```bash
# Run all host tests (40 tests: unit + integration)
cargo test -p convex-hypervisor

# Run with ignored tests (requires hypervisor entitlement)
just test

# End-to-end: snapshot + fork with Boa
just snapshot-linux
just fork-linux --msg '"console.log(1+2)"'
```

## Code organization

| Crate | Target | Description |
|-------|--------|-------------|
| `host/` (convex-hypervisor) | macOS native | Hypervisor host: VM lifecycle, HVF bindings, fork engine |
| `init/` (convex-init) | `aarch64-unknown-none` | no_std PID 1 for the Linux VM. Raw syscalls only. |
| `runner-v8/` (convex-runner-v8) | `aarch64-unknown-linux-gnu` | V8 JS runner, dynamically linked against glibc |
| `runner-js/` (convex-runner-js) | `aarch64-unknown-linux-musl` | Boa JS runner, statically linked |
| `guest/` (convex-guest) | `aarch64-unknown-none` | Phase 1 bare-metal guest with QuickJS |
| `shared/` (convex-shared) | no_std | Hypercall IDs and memory layout constants |

## Code style

- Rust 2021 edition.
- No `unwrap()` on HVF calls — always propagate errors or use `check_hv()`.
- Prefer explicit types for register values and GPA addresses.
- The `init` crate is `no_std` with no dependencies — all Linux interaction is via raw syscalls.
- The V8/Boa runners use `libc` for raw fd operations (write to fd 3, dup2).

## Review process

From [NOTES.md](NOTES.md):

1. Read through all the code carefully
2. Write any missing tests, iterate until fixed
3. Look for opportunities to use high-quality third-party crates
4. Review code organization and duplication

We also use GPT-5.4 xhigh as a second reviewer via `codex exec`.

## Key constants that must stay in sync

These values are defined in multiple places and must match:

| Constant | Files |
|----------|-------|
| Inbox GPA (`0x3F00_0000`, 8 MiB) | `init/src/main.rs`, `host/src/linux_boot.rs` |
| Outbox GPA (`0x3F80_0000`, 8 MiB) | `init/src/main.rs`, `host/src/linux_boot.rs` |
| Ready pipe fd (3) | `init/src/main.rs`, `runner-v8/src/main.rs`, `runner-js/src/main.rs` |
| READY sentinel (`CONVEX_READY`) | `init/src/main.rs`, `host/src/linux_boot.rs` |
| GIC addresses (GICD/GICR) | `host/src/dtb.rs` (must match HVF expectations) |
| UART address (`0x0900_0000`) | `host/src/dtb.rs`, `host/src/pl011.rs`, kernel bootargs |
| Guest RAM base (`0x4000_0000`) | `host/src/linux_boot.rs` |

## Architecture notes

### The ready pipe protocol

Init creates two pipes before forking:
1. **JS pipe**: init (parent) writes JS to runner (child) stdin after fork-resume
2. **Ready pipe**: runner writes 1 byte to fd 3 when initialized, init blocks on read

This ensures the snapshot captures the runner fully initialized. See `init/src/main.rs` and `runner-v8/src/main.rs`.

### Why /dev/mem for the inbox/outbox

The shared region (inbox + outbox) is 16 MiB at a fixed GPA below guest RAM. Init accesses it via `/dev/mem` mmap. After CoW fork, the old mmap is stale — init munmaps and re-mmaps fresh. The runner never touches `/dev/mem` (avoids the stale-mmap problem). Runner output flows through the output pipe (stdout → init → outbox).

### Quiet kernel for performance

The kernel boots with `quiet loglevel=0` to suppress console output. This eliminates most PL011 UART MMIO exits during fork (from ~660 exits to ~0). For debugging, remove these from the bootargs in `host/src/dtb.rs`.

### Watchdog variants

- `spawn_watchdog` (100ms interval): used during boot/snapshot where V8 takes seconds to initialize
- `spawn_watchdog_fast` (1ms interval): used during fork for minimal join latency
