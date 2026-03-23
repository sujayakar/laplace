//! V8 JS runner for the Linux VM.

use std::ffi::CString;
use std::io::Write;
use std::os::unix::io::AsRawFd;
const MAILBOX_GPA: u64 = 0x3FFF_0000;
const MAILBOX_SIZE: usize = 64 * 1024;

fn main() {
    if let Ok(kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        unsafe {
            libc::dup2(kmsg.as_raw_fd(), 1);
            libc::dup2(kmsg.as_raw_fd(), 2);
        }
    }

    eprintln!("[runner-v8] V8 runner starting");

    let mailbox = map_mailbox();
    let js_code = read_mailbox_str(mailbox as *const u8);
    eprintln!("[runner-v8] eval: {}", js_code);

    // Initialize V8
    let platform = v8::new_default_platform(0, false).make_shared();
    v8::V8::initialize_platform(platform);
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

    // Evaluate
    let code = v8::String::new(scope, js_code).unwrap();
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
    unsafe {
        std::arch::asm!(
            "mov x8, #142", "svc #0",
            in("x0") 0xfee1deadu64, in("x1") 0x28121969u64,
            in("x2") 0x4321fedcu64, in("x3") 0u64,
            options(noreturn),
        );
    }
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

fn map_mailbox() -> *mut u8 {
    let path = CString::new("/dev/mem").unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
    assert!(fd >= 0, "failed to open /dev/mem");
    let ptr = unsafe {
        libc::mmap(std::ptr::null_mut(), MAILBOX_SIZE,
            libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED,
            fd, MAILBOX_GPA as libc::off_t)
    };
    unsafe { libc::close(fd); }
    assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
    ptr as *mut u8
}

fn read_mailbox_str(mailbox: *const u8) -> &'static str {
    let mut len = 0;
    unsafe {
        while len < MAILBOX_SIZE && *mailbox.add(len) != 0 { len += 1; }
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(mailbox, len))
    }
}
