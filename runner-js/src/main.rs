//! QuickJS runner for the Linux VM.
//!
//! Runs as PID 1 (init). Maps the mailbox via /dev/mem, writes READY,
//! spins until the host writes JS code, evaluates it, prints the result.

use std::ffi::CString;
use std::io::Write;
use std::os::unix::io::AsRawFd;

const MAILBOX_GPA: u64 = 0x3FFF_0000;
const MAILBOX_SIZE: usize = 64 * 1024;
const READY_MAGIC: &[u8] = b"CONVEX_READY";

fn main() {
    mount("devtmpfs", "/dev", "devtmpfs");
    mount("proc", "/proc", "proc");

    // Redirect stdout/stderr to /dev/kmsg
    if let Ok(kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        unsafe {
            libc::dup2(kmsg.as_raw_fd(), 1);
            libc::dup2(kmsg.as_raw_fd(), 2);
        }
    }

    eprintln!("[runner-js] QuickJS runner starting");

    // The mailbox already has JS code (written by the host during fork).
    // The init process set up devtmpfs and /dev/mem before exec'ing us.
    let mailbox = map_mailbox();
    let js_code = read_mailbox_str(mailbox as *const u8);
    eprintln!("[runner-js] eval: {}", js_code);

    // Test: can we allocate memory at all?
    let v = vec![1u8; 1024];
    eprintln!("[runner-js] alloc test ok: {} bytes", v.len());

    let rt = rquickjs::Runtime::new().expect("Runtime::new");
    eprintln!("[runner-js] Runtime created");
    let ctx = rquickjs::Context::full(&rt).expect("Context::full");
    eprintln!("[runner-js] Context created");

    eprintln!("[runner-js] evaluating JS...");
    // Set a stack size limit for QuickJS
    rt.set_max_stack_size(1024 * 1024); // 1 MiB
    ctx.with(|ctx| {
        match ctx.eval::<rquickjs::Value, _>(js_code.to_string()) {
            Ok(val) => {
                eprintln!("[runner-js] eval succeeded");
                if let Some(n) = val.as_int() {
                    let _ = writeln!(std::io::stdout(), "{}", n);
                } else if let Some(f) = val.as_float() {
                    let _ = writeln!(std::io::stdout(), "{}", f);
                } else if let Some(s) = val.as_string() {
                    let _ = writeln!(std::io::stdout(), "{}", s.to_string().unwrap_or_default());
                } else if val.is_undefined() {
                    // silent
                } else {
                    let _ = writeln!(std::io::stdout(), "{:?}", val);
                }
            }
            Err(e) => {
                let _ = writeln!(std::io::stderr(), "[runner-js] error: {}", e);
            }
        }
    });

    // Power off via raw syscall
    unsafe {
        std::arch::asm!(
            "mov x8, #142",  // __NR_reboot
            "svc #0",
            in("x0") 0xfee1deadu64,
            in("x1") 0x28121969u64,
            in("x2") 0x4321fedcu64,
            in("x3") 0u64,
            options(noreturn),
        );
    }
}

fn mount(source: &str, target: &str, fstype: &str) {
    let _ = std::fs::create_dir_all(target);
    let src = CString::new(source).unwrap();
    let tgt = CString::new(target).unwrap();
    let fst = CString::new(fstype).unwrap();
    unsafe {
        // mount(2) = syscall 40 on aarch64
        libc::syscall(
            40,
            src.as_ptr(),
            tgt.as_ptr(),
            fst.as_ptr(),
            0u64,
            std::ptr::null::<u8>(),
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
