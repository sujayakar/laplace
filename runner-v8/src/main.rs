//! V8 JS runner for the Linux VM.
//!
//! Lifecycle:
//! 1. Mount devtmpfs/proc if needed, redirect output to /dev/kmsg
//! 2. Initialize V8 (platform, isolate, context, console.log)
//! 3. Block on read(stdin) for JS code (init sends via pipe)
//! 4. Evaluate JS, print result, exit
//!
//! V8 init happens BEFORE snapshot (amortized). After fork, the init
//! sends JS via the pipe and V8 just evals — no V8 init needed.

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;

fn main() {
    mount_if_needed("devtmpfs", "/dev", "devtmpfs");
    mount_if_needed("proc", "/proc", "proc");

    if let Ok(kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        unsafe {
            libc::dup2(kmsg.as_raw_fd(), 1);
            libc::dup2(kmsg.as_raw_fd(), 2);
        }
    }

    eprintln!("[runner-v8] V8 runner starting");

    // Initialize V8
    eprintln!("[runner-v8] initializing V8...");
    let platform = v8::new_single_threaded_default_platform(false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::set_flags_from_string("--single-threaded");
    v8::V8::initialize();

    let isolate = &mut v8::Isolate::new(Default::default());
    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    // Install console.log
    let global = context.global(scope);
    let console_key = v8::String::new(scope, "console").unwrap();
    let console_obj = v8::Object::new(scope);
    let log_key = v8::String::new(scope, "log").unwrap();
    let log_fn = v8::Function::new(scope, console_log_callback).unwrap();
    console_obj.set(scope, log_key.into(), log_fn.into());
    global.set(scope, console_key.into(), console_obj.into());

    eprintln!("[runner-v8] V8 ready, reading JS from stdin...");

    // Read JS from stdin (init sends it via pipe after fork-resume)
    let mut js_code = String::new();
    std::io::stdin().read_to_string(&mut js_code).unwrap_or(0);

    if js_code.is_empty() {
        eprintln!("[runner-v8] no JS received on stdin");
        power_off();
    }

    eprintln!("[runner-v8] eval: {}", js_code.trim());

    let code = v8::String::new(scope, &js_code).unwrap();
    match v8::Script::compile(scope, code, None) {
        Some(script) => match script.run(scope) {
            Some(result) => {
                let s = result.to_rust_string_lossy(scope);
                if s != "undefined" {
                    let _ = writeln!(std::io::stdout(), "{}", s);
                }
            }
            None => eprintln!("[runner-v8] eval returned None"),
        },
        None => eprintln!("[runner-v8] compile failed"),
    }

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    power_off();
}

fn console_log_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let mut parts = Vec::new();
    for i in 0..args.length() {
        let arg = args.get(i);
        let s = arg.to_string(scope).unwrap().to_rust_string_lossy(scope);
        parts.push(s);
    }
    let _ = writeln!(std::io::stdout(), "{}", parts.join(" "));
    let _ = std::io::stdout().flush();
}

fn mount_if_needed(source: &str, target: &str, fstype: &str) {
    if target == "/dev" && std::path::Path::new("/dev/null").exists() { return; }
    if target == "/proc" && std::path::Path::new("/proc/self").exists() { return; }
    let _ = std::fs::create_dir_all(target);
    let src = CString::new(source).unwrap();
    let tgt = CString::new(target).unwrap();
    let fst = CString::new(fstype).unwrap();
    unsafe {
        libc::mount(src.as_ptr(), tgt.as_ptr(), fst.as_ptr(), 0,
            std::ptr::null::<libc::c_void>());
    }
}

fn power_off() -> ! {
    unsafe {
        std::arch::asm!(
            "mov x8, #142", "svc #0",
            in("x0") 0xfee1deadu64, in("x1") 0x28121969u64,
            in("x2") 0x4321fedcu64, in("x3") 0u64,
            options(noreturn),
        );
    }
}
