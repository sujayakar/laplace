//! V8 JS runner for the Linux VM.
//!
//! Lifecycle:
//! 1. Mount devtmpfs/proc if needed, redirect output to /dev/kmsg
//! 2. Initialize V8 (platform, isolate, context, console.log)
//! 3. If /bundle.js exists, pre-load it into the V8 context (amortized in snapshot)
//! 4. Signal ready, block on read(stdin) for invocation JS
//! 5. Evaluate invocation JS (runs against pre-loaded context), print result, exit
//!
//! The bundle is compiled+executed BEFORE snapshot. After fork, only a tiny
//! invocation string arrives via stdin — V8 just evals it against the warm context.

use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;

fn main() {
    mount_if_needed("devtmpfs", "/dev", "devtmpfs");
    mount_if_needed("proc", "/proc", "proc");

    // Redirect stderr to /dev/kmsg for debug logs. Stdout goes to the output
    // pipe (init captures it and writes to the outbox for the host to read).
    if let Ok(kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        unsafe {
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

    // Pre-load /bundle.js if it exists (executed during snapshot, amortized)
    let bundle_path = std::path::Path::new("/bundle.js");
    if bundle_path.exists() {
        match std::fs::read_to_string(bundle_path) {
            Ok(bundle_code) => {
                let len = bundle_code.len();
                eprintln!("[runner-v8] pre-loading /bundle.js ({} bytes)...", len);
                let code = v8::String::new(scope, &bundle_code).unwrap();
                match v8::Script::compile(scope, code, None) {
                    Some(script) => match script.run(scope) {
                        Some(_) => eprintln!("[runner-v8] bundle loaded OK"),
                        None => eprintln!("[runner-v8] bundle eval returned None"),
                    },
                    None => eprintln!("[runner-v8] bundle compile failed"),
                }
            }
            Err(e) => eprintln!("[runner-v8] failed to read /bundle.js: {}", e),
        }
    }

    eprintln!("[runner-v8] signaling init...");

    // Signal init that V8 is fully initialized (+ bundle loaded) via ready pipe (fd 3).
    // Init blocks on this before writing READY to the mailbox, ensuring
    // the snapshot captures V8 in its initialized state.
    const READY_FD: i32 = 3;
    unsafe {
        libc::write(READY_FD, b"R".as_ptr() as *const libc::c_void, 1);
        libc::close(READY_FD);
    }

    eprintln!("[runner-v8] reading JS from stdin...");

    // Read JS from stdin (init sends it via pipe after fork-resume)
    let mut js_code = String::new();
    std::io::stdin().read_to_string(&mut js_code).unwrap_or(0);

    if js_code.is_empty() {
        let _ = writeln!(std::io::stdout(), "[runner-v8] no JS received on stdin");
        eprintln!("[runner-v8] no JS received on stdin");
        std::process::exit(1);
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
            None => {
                let _ = writeln!(std::io::stdout(), "[runner-v8] eval returned None");
                eprintln!("[runner-v8] eval returned None");
            }
        },
        None => {
            let _ = writeln!(std::io::stdout(), "[runner-v8] compile failed");
            eprintln!("[runner-v8] compile failed");
        }
    }

    // Flush and exit. Don't power_off() — init needs to collect our stdout
    // from the output pipe and write it to the outbox before shutting down.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}

fn console_log_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    _rv: v8::ReturnValue<v8::Value>,
) {
    let mut parts = Vec::new();
    for i in 0..args.length() {
        let arg = args.get(i);
        let s = match arg.to_string(scope) {
            Some(v) => v.to_rust_string_lossy(scope),
            None => "[toString threw]".to_string(),
        };
        parts.push(s);
    }
    let _ = writeln!(std::io::stdout(), "{}", parts.join(" "));
    let _ = std::io::stdout().flush();
}

#[allow(dead_code)]
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

