entitlements := "entitlements.plist"
guest_bin := "guest/target/aarch64-unknown-none/release/convex-guest"
template_dir := "/tmp/hvf-template"
host_bin := "./target/release/convex-hypervisor"

init_bin := "init/target/aarch64-unknown-none/release/convex-init"
runner_js_bin := "runner-js/target/aarch64-unknown-linux-musl/release/convex-runner-js"
runner_v8_bin := "runner-v8/target/aarch64-unknown-linux-gnu/release/convex-runner-v8"
initramfs := "/tmp/hvf-initramfs.cpio"
linux_kernel := "kernel/Image-arm64"
linux_template := "/tmp/hvf-linux-template"

kernel_url := "https://github.com/cloud-hypervisor/linux/releases/download/ch-release-v6.16.9-20251112/Image-arm64"

# ── Development commands ─────────────────────────────────────────────────────

# Format all code
format:
    #!/usr/bin/env bash
    set -euo pipefail
    cargo fmt --all
    for crate in guest init runner-js runner-v8; do
        cargo fmt --manifest-path "$crate/Cargo.toml"
    done

# Run clippy lint checks
lint:
    cargo clippy -p convex-hypervisor -p convex-shared -- -D warnings

# Run all tests
test:
    cargo test -p convex-hypervisor -p convex-shared

# Download the Cloud Hypervisor aarch64 kernel
download-kernel:
    mkdir -p kernel
    curl -L -o kernel/Image-arm64 "{{kernel_url}}"
    @echo "Kernel: $(ls -lh kernel/Image-arm64 | awk '{print $5}')"
    @file kernel/Image-arm64

# ── Top-level commands (all-in-one) ──────────────────────────────────────────

# Build init + boa runner, make initramfs, snapshot. All-in-one.
snapshot-boa: build-init build-runner-js
    ./scripts/mkinitramfs.sh {{init_bin}} {{initramfs}} {{runner_js_bin}}
    just build-host
    {{host_bin}} snapshot-linux {{linux_kernel}} --initrd {{initramfs}} --quiet {{linux_template}}

# Build host, codesign, fork with message (boa)
fork-boa msg: build-host
    {{host_bin}} fork-linux --msg '{{msg}}' {{linux_template}}

# Build init + v8 runner, make initramfs (with glibc libs), snapshot
snapshot-v8: build-init build-runner-v8
    just _initramfs-v8
    just build-host
    {{host_bin}} snapshot-linux {{linux_kernel}} --initrd {{initramfs}} --quiet {{linux_template}}

# Build host, codesign, fork with message (v8)
fork-v8 msg: build-host
    {{host_bin}} fork-linux --msg '{{msg}}' {{linux_template}}

# Fork with --js-file (v8)
fork-v8-file path: build-host
    {{host_bin}} fork-linux --js-file '{{path}}' {{linux_template}}

# ── Lower-level build recipes (fast iteration) ──────────────────────────────

# Build and codesign the host binary only
build-host: (_build-and-sign "convex-hypervisor" "convex-hypervisor")

# Build the init binary (PID 1 for the Linux VM)
build-init:
    cd init && cargo build --release --target aarch64-unknown-none

# Build the Boa JS runner (musl-static)
build-runner-js:
    cd runner-js && cargo build --target aarch64-unknown-linux-musl --release

# Build the V8 runner (glibc, needs zig on PATH)
build-runner-v8:
    cd runner-v8 && CC_aarch64_unknown_linux_gnu=$(pwd)/../scripts/zig-gnu-cc \
        CXX_aarch64_unknown_linux_gnu=$(pwd)/../scripts/zig-gnu-cc \
        AR_aarch64_unknown_linux_gnu="zig ar" \
        CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=$(pwd)/../scripts/zig-gnu-cc \
        cargo build --target aarch64-unknown-linux-gnu --release

# ── Initramfs recipes ───────────────────────────────────────────────────────

# Build initramfs from init only (no runner)
initramfs: build-init
    ./scripts/mkinitramfs.sh {{init_bin}} {{initramfs}}

# Build initramfs with Boa JS runner
initramfs-js: build-init build-runner-js
    ./scripts/mkinitramfs.sh {{init_bin}} {{initramfs}} {{runner_js_bin}}

# Build initramfs with V8 runner + glibc libs
_initramfs-v8:
    #!/usr/bin/env bash
    set -euo pipefail
    TMPDIR=$(mktemp -d)
    trap "rm -rf $TMPDIR" EXIT
    mkdir -p "$TMPDIR"/{dev,proc,sys,tmp,lib}
    cp {{init_bin}} "$TMPDIR/init"
    chmod 755 "$TMPDIR/init"
    cp {{runner_v8_bin}} "$TMPDIR/runner"
    chmod 755 "$TMPDIR/runner"
    # Extract glibc shared libs from zig's sysroot
    ZIG_LIB=$(zig env | grep lib_dir | head -1 | sed 's/.*": "//;s/".*//')
    SYSROOT="$ZIG_LIB/libc/glibc"
    # Copy the zig-provided glibc stubs + ld-linux
    ZIG_GLIBC="$ZIG_LIB/aarch64-linux-gnu-musl"
    if [ -d "$ZIG_LIB/aarch64-linux-gnu" ]; then
        ZIG_GLIBC="$ZIG_LIB/aarch64-linux-gnu"
    fi
    # Use ldd-like approach: copy what the binary needs
    # For zig-linked binaries, the needed libs are in zig's sysroot
    for lib in ld-linux-aarch64.so.1 libc.so.6 libm.so.6 libdl.so.2 libpthread.so.0 libgcc_s.so.1 libstdc++.so.6; do
        found=""
        for search in "$ZIG_LIB"/aarch64-linux-gnu*/ "$ZIG_LIB"/libc/glibc/ /usr/aarch64-linux-gnu/lib/; do
            if [ -f "$search/$lib" ]; then
                cp "$search/$lib" "$TMPDIR/lib/"
                found=1
                break
            fi
        done
        # Not fatal if a lib is missing — the binary may not need it
    done
    # Also check for libs in the zig lib dir itself
    if [ -f "$ZIG_LIB/ld-linux-aarch64.so.1" ]; then
        cp "$ZIG_LIB/ld-linux-aarch64.so.1" "$TMPDIR/lib/"
    fi
    cd "$TMPDIR"
    find . | cpio -o -H newc --quiet > {{initramfs}}
    echo "initramfs (v8): $(du -h {{initramfs}} | cut -f1)"

# ── Legacy / existing recipes ───────────────────────────────────────────────

# Build everything and run the guest directly (no snapshot)
run *args: guest (_build-and-sign "convex-hypervisor" "convex-hypervisor")
    {{host_bin}} run {{args}} {{guest_bin}}

# Create a snapshot template from the guest binary
snapshot: guest (_build-and-sign "convex-hypervisor" "convex-hypervisor")
    {{host_bin}} snapshot {{guest_bin}} {{template_dir}}

# Fork from a snapshot template
fork *args: (_build-and-sign "convex-hypervisor" "convex-hypervisor")
    {{host_bin}} fork {{args}} {{template_dir}}

# Benchmark fork latency
bench *args: (_build-and-sign "convex-hypervisor" "convex-hypervisor")
    {{host_bin}} bench {{args}} {{template_dir}}

# Full end-to-end demo: snapshot, JS eval, determinism proof, benchmarks
demo: guest (_build-and-sign "convex-hypervisor" "convex-hypervisor")
    @echo "═══════════════════════════════════════════════════"
    @echo "  Convex Hypervisor MVP Demo"
    @echo "═══════════════════════════════════════════════════"
    @echo ""
    @echo "1. Creating snapshot template (boot QuickJS → HC_READY)..."
    {{host_bin}} snapshot {{guest_bin}} {{template_dir}}
    @echo ""
    @echo "2. Running JS with console.log, Date.now, Math.random, db.query..."
    {{host_bin}} fork --seed 42 --js 'var users = db.query("users"); console.log("Users:", JSON.stringify(users)); console.log("Time:", Date.now(), "Random:", Math.random())' {{template_dir}}
    @echo ""
    @echo "3. Proving determinism (same seed → identical output)..."
    @echo "   Run 1:" && {{host_bin}} fork --seed 42 --js 'console.log(db.query("users")[0].name, Math.random())' {{template_dir}} 2>/dev/null
    @echo "   Run 2:" && {{host_bin}} fork --seed 42 --js 'console.log(db.query("users")[0].name, Math.random())' {{template_dir}} 2>/dev/null
    @echo "   Run 3 (different seed):" && {{host_bin}} fork --seed 99 --js 'console.log(db.query("users")[0].name, Math.random())' {{template_dir}} 2>/dev/null
    @echo ""
    @echo "4. Benchmarking fork+JS eval with db.query (200 iterations)..."
    {{host_bin}} bench --iterations 200 --js 'var u = db.query("users"); u[0].name' {{template_dir}}
    @echo ""
    @echo "═══════════════════════════════════════════════════"
    @echo "  Demo complete!"
    @echo "═══════════════════════════════════════════════════"

# Boot a Linux kernel in the VM (kernel only, no initramfs)
boot-linux kernel *args: (_build-and-sign "convex-hypervisor" "convex-hypervisor")
    {{host_bin}} boot-linux {{kernel}} {{args}}

# Snapshot: boot Linux to HC_READY, save template (with JS runner)
snapshot-linux: initramfs-js (_build-and-sign "convex-hypervisor" "convex-hypervisor")
    {{host_bin}} snapshot-linux {{linux_kernel}} --initrd {{initramfs}} --quiet {{linux_template}}

# Fork: resume from snapshot with a message
fork-linux *args: (_build-and-sign "convex-hypervisor" "convex-hypervisor")
    {{host_bin}} fork-linux {{args}} {{linux_template}}

# Build the guest (no_std aarch64 binary)
guest:
    cd guest && cargo build --release

# Run all host tests including ignored (requires hypervisor entitlement on macOS)
test-all: guest (_build-and-sign "convex-hypervisor" "convex-hypervisor")
    cargo test -p convex-hypervisor -- --include-ignored

# Build and codesign the spike test binary
spike: (_build-and-sign "hvf-spike" "hvf-spike")

# Run the spike test
run-spike: spike
    ./target/debug/hvf-spike

# Build a binary, then codesign it with the hypervisor entitlement
_build-and-sign crate binary:
    cargo build --release -p {{crate}}
    codesign --sign - --entitlements {{entitlements}} --force target/release/{{binary}}
