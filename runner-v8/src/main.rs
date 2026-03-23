//! V8 JS runner for the Linux VM.
//!
//! Lifecycle:
//! 1. Mount devtmpfs/proc, redirect output to /dev/kmsg
//! 2. Initialize V8 (platform, isolate, context, console.log)
//! 3. Write READY to mailbox → host snapshots
//! 4. Spin until mailbox changes (fork-resume writes JS code)
//! 5. Evaluate JS, print result, power off
//!
//! V8 initialization (step 2) happens BEFORE the snapshot, so it's
//! amortized across all forks. Forks skip directly to step 4.

use std::ffi::CString;
use std::io::Write;
use std::os::unix::io::AsRawFd;

const MAILBOX_GPA: u64 = 0x3FFF_0000;
const MAILBOX_SIZE: usize = 64 * 1024;
const READY_MAGIC: &[u8] = b"CONVEX_READY";

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

    // Check if JS was passed via argv (fork path — init already read mailbox)
    let args: Vec<String> = std::env::args().collect();
    let js_from_argv = if args.len() > 1 { Some(args[1..].join(" ")) } else { None };

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

    eprintln!("[runner-v8] V8 ready");

    // Get JS code
    let js_code = if let Some(code) = js_from_argv {
        // Fork path: JS was passed as argv[1] by init
        code
    } else {
        // Snapshot path: write READY, spin until host writes JS code
        // Map mailbox and write READY
        if let Some(mailbox) = map_mailbox_rw() {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    READY_MAGIC.as_ptr(), mailbox, READY_MAGIC.len(),
                );
                *mailbox.add(READY_MAGIC.len()) = 0;
            }
            eprintln!("[runner-v8] READY (waiting for JS in mailbox)");

            // Spin until mailbox content changes.
            // After fork-restore, the host replaces the mailbox page.
            // The stale mmap may SIGBUS, so we re-mmap after detecting change.
            // Spin with fresh mmap each iteration to survive CoW fork
            let code = spin_for_mailbox_change();
            code
        } else {
            // Can't access mailbox — use default
            "console.log('Hello from V8!', 1+2)".to_string()
        }
    };

    eprintln!("[runner-v8] eval: {}", js_code);

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

fn spin_for_mailbox_change() -> String {
    loop {
        let fresh = map_mailbox_rw();
        if let Some(ptr) = fresh {
            let first = unsafe { std::ptr::read_volatile(ptr) };
            if first != READY_MAGIC[0] {
                let code = read_mailbox_str(ptr as *const u8).to_string();
                return code;
            }
            unsafe { libc::munmap(ptr as *mut libc::c_void, MAILBOX_SIZE); }
        }
        std::hint::spin_loop();
    }
}

fn map_mailbox_rw() -> Option<*mut u8> {
    let path = CString::new("/dev/mem").ok()?;
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
    if fd < 0 { return None; }
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(), MAILBOX_SIZE,
            libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED,
            fd, MAILBOX_GPA as libc::off_t,
        )
    };
    unsafe { libc::close(fd); }
    if ptr == libc::MAP_FAILED { return None; }
    Some(ptr as *mut u8)
}

fn read_mailbox_str(mailbox: *const u8) -> &'static str {
    let mut len = 0;
    unsafe {
        while len < MAILBOX_SIZE && *mailbox.add(len) != 0 { len += 1; }
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(mailbox, len))
    }
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
