//! JS runner for the Linux VM using Boa (pure Rust JS engine).
//!
//! Exec'd by the no_std init after fork. Reads JS code from the
//! mailbox (already written by the host), evaluates it, prints output.

use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;

use boa_engine::{Context, Source};

fn main() {
    // Redirect stderr to /dev/kmsg for debug logs. Stdout goes to the output
    // pipe (init captures it and writes to the outbox for the host to read).
    if let Ok(kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        unsafe {
            libc::dup2(kmsg.as_raw_fd(), 2);
        }
    }

    eprintln!("[runner-js] Boa JS runner starting");

    // Initialize Boa engine
    let mut ctx = Context::default();
    boa_runtime::Console::register_with_logger(
        boa_runtime::DefaultLogger,
        &mut ctx,
    ).expect("register console");

    // Signal init that Boa is ready via ready pipe (fd 3).
    // Init blocks on this before writing READY to the mailbox.
    const READY_FD: i32 = 3;
    unsafe {
        libc::write(READY_FD, b"R".as_ptr() as *const libc::c_void, 1);
        libc::close(READY_FD);
    }

    eprintln!("[runner-js] Boa ready, reading JS from stdin...");

    // Read JS from stdin (init sends it via pipe after fork-resume)
    let mut js_code = String::new();
    std::io::stdin().read_to_string(&mut js_code).unwrap_or(0);

    if js_code.is_empty() {
        let _ = writeln!(std::io::stdout(), "[runner-js] no JS received on stdin");
        eprintln!("[runner-js] no JS received on stdin");
        std::process::exit(1);
    }

    eprintln!("[runner-js] eval: {}", js_code.trim());

    match ctx.eval(Source::from_bytes(js_code.as_bytes())) {
        Ok(val) => {
            let s = val.display().to_string();
            if s != "undefined" {
                let _ = writeln!(std::io::stdout(), "{}", s);
            }
        }
        Err(e) => {
            // Write to both stdout (→ outbox for host) and stderr (→ /dev/kmsg for debug)
            let _ = writeln!(std::io::stdout(), "[runner-js] error: {}", e);
            let _ = writeln!(std::io::stderr(), "[runner-js] error: {}", e);
        }
    }

    // Flush and exit. Don't power_off() — init needs to collect our stdout
    // from the output pipe and write it to the outbox before shutting down.
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
}
