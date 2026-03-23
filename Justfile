entitlements := "entitlements.plist"
guest_bin := "guest/target/aarch64-unknown-none/release/convex-guest"
template_dir := "/tmp/hvf-template"
host_bin := "./target/debug/convex-hypervisor"

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

# Boot a Linux kernel in the VM
boot-linux kernel *args: (_build-and-sign "convex-hypervisor" "convex-hypervisor")
    {{host_bin}} boot-linux {{kernel}} {{args}}

# Build the guest (no_std aarch64 binary)
guest:
    cd guest && cargo build --release

# Run all host tests (unit + integration)
test: guest (_build-and-sign "convex-hypervisor" "convex-hypervisor")
    cargo test -p convex-hypervisor -- --include-ignored

# Build and codesign the spike test binary
spike: (_build-and-sign "hvf-spike" "hvf-spike")

# Run the spike test
run-spike: spike
    ./target/debug/hvf-spike

# Build a binary, then codesign it with the hypervisor entitlement
_build-and-sign crate binary:
    cargo build -p {{crate}}
    codesign --sign - --entitlements {{entitlements}} --force target/debug/{{binary}}
