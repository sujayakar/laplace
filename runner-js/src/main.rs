//! JS runner for the Linux VM using Boa (pure Rust JS engine).
//!
//! Exec'd by the no_std init after fork. Reads JS code from the
//! mailbox (already written by the host), evaluates it, prints output.

use std::ffi::CString;
use std::io::Write;
use std::os::unix::io::AsRawFd;

use boa_engine::{Context, Source};

const MAILBOX_GPA: u64 = 0x3FFF_0000;
const MAILBOX_SIZE: usize = 64 * 1024;

fn main() {
    // stdout/stderr should already be set up by init, but ensure /dev/kmsg
    if let Ok(kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        unsafe {
            libc::dup2(kmsg.as_raw_fd(), 1);
            libc::dup2(kmsg.as_raw_fd(), 2);
        }
    }

    eprintln!("[runner-js] Boa JS runner starting");

    let mailbox = map_mailbox();
    let js_code = read_mailbox_str(mailbox as *const u8);
    eprintln!("[runner-js] eval: {}", js_code);

    let mut ctx = Context::default();

    // Register console.log via boa_runtime
    boa_runtime::Console::register_with_logger(
        boa_runtime::DefaultLogger,
        &mut ctx,
    ).expect("register console");

    match ctx.eval(Source::from_bytes(js_code.as_bytes())) {
        Ok(val) => {
            let s = val.display().to_string();
            if s != "undefined" {
                let _ = writeln!(std::io::stdout(), "{}", s);
            }
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "[runner-js] error: {}", e);
        }
    }

    // Flush and power off
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
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

fn map_mailbox() -> *mut u8 {
    let path = CString::new("/dev/mem").unwrap();
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
    assert!(fd >= 0, "failed to open /dev/mem");
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            MAILBOX_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            MAILBOX_GPA as libc::off_t,
        )
    };
    unsafe { libc::close(fd); }
    assert_ne!(ptr, libc::MAP_FAILED, "mmap failed");
    ptr as *mut u8
}

fn read_mailbox_str(mailbox: *const u8) -> &'static str {
    let mut len = 0;
    unsafe {
        while len < MAILBOX_SIZE && *mailbox.add(len) != 0 {
            len += 1;
        }
        std::str::from_utf8_unchecked(std::slice::from_raw_parts(mailbox, len))
    }
}
