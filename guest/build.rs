use std::process::Command;

fn main() {
    let qjs_dir = "quickjs";
    let include_dir = format!("{}/include", qjs_dir);

    // Find llvm-ar from the Rust toolchain (macOS ar can't create valid
    // archives for ELF cross-compilation targets).
    let sysroot = String::from_utf8(
        Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .expect("failed to run rustc --print sysroot")
            .stdout,
    )
    .expect("non-utf8 sysroot")
    .trim()
    .to_string();

    let llvm_ar = format!(
        "{}/lib/rustlib/{}/bin/llvm-ar",
        sysroot,
        std::env::consts::ARCH.to_string() + "-apple-darwin"
    );

    let mut build = cc::Build::new();
    build
        .files(&[
            format!("{}/quickjs.c", qjs_dir),
            format!("{}/libregexp.c", qjs_dir),
            format!("{}/libunicode.c", qjs_dir),
            format!("{}/dtoa.c", qjs_dir),
            format!("{}/libc_stubs.c", qjs_dir),
        ])
        .include(qjs_dir)
        .include(&include_dir)
        .flag("-ffreestanding")
        .flag("-nostdinc")
        .flag(&format!("-isystem{}", include_dir))
        .flag("-include")
        .flag(&format!("{}/include/alloca.h", qjs_dir))
        .define("CONFIG_VERSION", "\"0.13.0\"")
        .define("_GNU_SOURCE", None)
        .define("__STDC_NO_ATOMICS__", "1")
        // Undefine __APPLE__ — the cross-compiler sets it because the host is macOS,
        // but we're targeting bare-metal aarch64. Without this, QuickJS calls
        // malloc_size() which doesn't exist in our freestanding libc.
        .flag("-U__APPLE__")
        // Prevent the compiler from generating SIMD loads/stores for struct copies.
        // Without this, clang generates `ldp q0, q1` which requires 16-byte alignment,
        // but data in .rodata may only be 8-byte aligned.
        .flag("-mstrict-align")
        .flag("-Wno-implicit-function-declaration")
        .flag("-Wno-sign-compare")
        .flag("-Wno-unused-parameter")
        .flag("-Wno-missing-field-initializers")
        .warnings(false)
        .opt_level(2);

    // macOS ar can't create valid ELF archives, so we use llvm-ar from
    // the Rust toolchain instead.
    build.archiver(&llvm_ar);

    build.compile("quickjs");

    println!("cargo:rerun-if-changed={}", qjs_dir);
    println!("cargo:rerun-if-changed=build.rs");
}
