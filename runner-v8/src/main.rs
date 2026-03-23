//! V8 JS runner for the Linux VM.
//!
//! Can run as PID 1 (init) or be exec'd by the no_std init.
//! If /dev/kmsg doesn't exist, mounts devtmpfs first.
//! Reads JS from the mailbox, or from /proc/cmdline (convex.js=...),
//! or uses a default test expression.

use std::ffi::CString;
use std::io::Write;
use std::os::unix::io::AsRawFd;

// Must match LINUX_MAILBOX_GPA/SIZE in host/src/linux_boot.rs
const MAILBOX_GPA: u64 = 0x3FFF_0000;
const MAILBOX_SIZE: usize = 64 * 1024;

fn main() {
    // Mount essential filesystems if not already mounted
    mount_if_needed("devtmpfs", "/dev", "devtmpfs");
    mount_if_needed("proc", "/proc", "proc");

    // Redirect stdout/stderr to /dev/kmsg
    if let Ok(kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        unsafe {
            libc::dup2(kmsg.as_raw_fd(), 1);
            libc::dup2(kmsg.as_raw_fd(), 2);
        }
    }

    eprintln!("[runner-v8] V8 runner starting");

    // Get JS code: try mailbox first, then cmdline, then default
    eprintln!("[runner-v8] getting JS code...");
    let js_code = get_js_code();
    eprintln!("[runner-v8] eval: {}", js_code);

    // Initialize V8 in single-threaded mode to avoid scheduler dependency
    eprintln!("[runner-v8] initializing V8 platform (single-threaded)...");
    // Platform with 0 worker threads = main thread only
    let platform = v8::new_single_threaded_default_platform(false).make_shared();
    v8::V8::initialize_platform(platform);
    v8::V8::set_flags_from_string("--single-threaded");
    v8::V8::initialize();
    eprintln!("[runner-v8] V8 initialized, creating isolate...");

    let isolate = &mut v8::Isolate::new(Default::default());
    eprintln!("[runner-v8] isolate created, creating context...");

    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);
    eprintln!("[runner-v8] context created, installing console.log...");

    // Install console.log
    let global = context.global(scope);
    let console_key = v8::String::new(scope, "console").unwrap();
    let console_obj = v8::Object::new(scope);
    let log_key = v8::String::new(scope, "log").unwrap();
    let log_fn = v8::Function::new(scope, console_log_callback).unwrap();
    console_obj.set(scope, log_key.into(), log_fn.into());
    global.set(scope, console_key.into(), console_obj.into());

    eprintln!("[runner-v8] evaluating JS...");

    // Evaluate
    let code = v8::String::new(scope, &js_code).unwrap();
    match v8::Script::compile(scope, code, None) {
        Some(script) => match script.run(scope) {
            Some(result) => {
                let s = result.to_rust_string_lossy(scope);
                if s != "undefined" {
                    let _ = writeln!(std::io::stdout(), "{}", s);
                }
                eprintln!("[runner-v8] eval complete");
            }
            None => eprintln!("[runner-v8] eval returned None"),
        },
        None => eprintln!("[runner-v8] compile failed"),
    }

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    power_off();
}

fn get_js_code() -> String {
    // Check argv first (init passes JS code as argv[1] if available)
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        return args[1..].join(" ");
    }

    // Try kernel command line: look for convex.js=...
    eprintln!("[runner-v8] trying cmdline...");
    if let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") {
        for param in cmdline.split_whitespace() {
            if let Some(js) = param.strip_prefix("convex.js=") {
                return js.to_string();
            }
        }
    }

    // Default test expression
    eprintln!("[runner-v8] using default JS");
    "console.log('Hello from V8 in a deterministic VM!', 1+2)".to_string()
}

fn try_read_mailbox() -> Option<String> {
    let path = CString::new("/dev/mem").ok()?;
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return None;
    }
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            MAILBOX_SIZE,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            MAILBOX_GPA as libc::off_t,
        )
    };
    unsafe { libc::close(fd); }
    if ptr == libc::MAP_FAILED {
        return None;
    }
    let mailbox = ptr as *const u8;
    let mut len = 0;
    unsafe {
        while len < MAILBOX_SIZE && *mailbox.add(len) != 0 {
            len += 1;
        }
    }
    let s = unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(mailbox, len)) };
    Some(s.to_string())
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
    // Check if already mounted by trying to stat a known file
    if target == "/dev" && std::path::Path::new("/dev/null").exists() {
        return;
    }
    if target == "/proc" && std::path::Path::new("/proc/self").exists() {
        return;
    }
    let _ = std::fs::create_dir_all(target);
    let src = CString::new(source).unwrap();
    let tgt = CString::new(target).unwrap();
    let fst = CString::new(fstype).unwrap();
    unsafe {
        libc::mount(
            src.as_ptr(),
            tgt.as_ptr(),
            fst.as_ptr(),
            0,
            std::ptr::null::<libc::c_void>(),
        );
    }
}

fn power_off() -> ! {
    unsafe {
        std::arch::asm!(
            "mov x8, #142",
            "svc #0",
            in("x0") 0xfee1deadu64,
            in("x1") 0x28121969u64,
            in("x2") 0x4321fedcu64,
            in("x3") 0u64,
            options(noreturn),
        );
    }
}
