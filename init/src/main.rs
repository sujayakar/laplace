//! Minimal init process (PID 1) for our Linux VM.
//!
//! Lifecycle:
//! 1. Mount devtmpfs + procfs, open /dev/kmsg for output
//! 2. Create pipe, fork child, exec /runner with pipe as stdin
//!    - If /runner doesn't exist: fall back to simple READY/spin
//! 3. Parent: map mailbox, write READY, spin until host overwrites
//! 4. After fork-resume: re-mmap mailbox fresh, read JS, write to pipe
//! 5. Wait for child, power off

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

// Syscall wrappers
unsafe fn syscall1(nr: u64, a0: u64) -> i64 {
    let ret: i64;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret, clobber_abi("C"));
    ret
}
unsafe fn syscall2(nr: u64, a0: u64, a1: u64) -> i64 {
    let ret: i64;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
        in("x1") a1, clobber_abi("C"));
    ret
}
unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let ret: i64;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
        in("x1") a1, in("x2") a2, clobber_abi("C"));
    ret
}
unsafe fn syscall4(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let ret: i64;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
        in("x1") a1, in("x2") a2, in("x3") a3, clobber_abi("C"));
    ret
}
unsafe fn syscall5(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    let ret: i64;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
        in("x1") a1, in("x2") a2, in("x3") a3, in("x4") a4, clobber_abi("C"));
    ret
}
unsafe fn syscall6(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: i64;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret,
        in("x1") a1, in("x2") a2, in("x3") a3, in("x4") a4, in("x5") a5,
        clobber_abi("C"));
    ret
}

// Syscall numbers (aarch64)
const __NR_DUP3: u64 = 24;
const __NR_MKDIRAT: u64 = 34;
const __NR_MOUNT: u64 = 40;
const __NR_OPENAT: u64 = 56;
const __NR_CLOSE: u64 = 57;
const __NR_PIPE2: u64 = 59;
const __NR_READ: u64 = 63;
const __NR_WRITE: u64 = 64;
const __NR_EXIT: u64 = 93;
const __NR_REBOOT: u64 = 142;
const __NR_MUNMAP: u64 = 215;
const __NR_CLONE: u64 = 220;
const __NR_EXECVE: u64 = 221;
const __NR_MMAP: u64 = 222;
const __NR_WAIT4: u64 = 260;

const AT_FDCWD: i32 = -100;
const SIGCHLD: u64 = 17;
const O_CLOEXEC: u64 = 0x80000;

// Mailbox
const MAILBOX_GPA: u64 = 0x3FFF_0000;
const MAILBOX_SIZE: usize = 64 * 1024;
const READY_MAGIC: &[u8] = b"CONVEX_READY";

fn write_all(fd: i32, buf: &[u8]) {
    unsafe { syscall3(__NR_WRITE, fd as u64, buf.as_ptr() as u64, buf.len() as u64); }
}

fn mkdirat(path: &[u8]) {
    unsafe { syscall3(__NR_MKDIRAT, AT_FDCWD as u64, path.as_ptr() as u64, 0o755); }
}

fn mount(src: &[u8], target: &[u8], fstype: &[u8]) {
    unsafe { syscall5(__NR_MOUNT, src.as_ptr() as u64, target.as_ptr() as u64,
        fstype.as_ptr() as u64, 0, 0); }
}

fn close(fd: i32) {
    unsafe { syscall1(__NR_CLOSE, fd as u64); }
}

fn map_mailbox_fresh() -> *mut u8 {
    let fd = unsafe {
        syscall3(__NR_OPENAT, AT_FDCWD as u64, b"/dev/mem\0".as_ptr() as u64, 2) // O_RDWR
    };
    if fd < 0 { return core::ptr::null_mut(); }
    let ptr = unsafe {
        syscall6(__NR_MMAP, 0, MAILBOX_SIZE as u64, 1 | 2, 1, fd as u64, MAILBOX_GPA)
    };
    close(fd as i32);
    if ptr < 0 { return core::ptr::null_mut(); }
    ptr as *mut u8
}

fn munmap_mailbox(ptr: *mut u8) {
    unsafe { syscall2(__NR_MUNMAP, ptr as u64, MAILBOX_SIZE as u64); }
}

fn read_mailbox_str(mailbox: *const u8) -> &'static [u8] {
    if mailbox.is_null() { return b""; }
    let mut len = 0;
    unsafe {
        while len < MAILBOX_SIZE && *mailbox.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(mailbox, len)
    }
}

fn power_off() -> ! {
    unsafe { syscall3(__NR_REBOOT, 0xfee1dead, 0x28121969, 0x4321fedc); }
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Mount essential filesystems
    mkdirat(b"/dev\0");
    mount(b"devtmpfs\0", b"/dev\0", b"devtmpfs\0");
    mkdirat(b"/proc\0");
    mount(b"proc\0", b"/proc\0", b"proc\0");

    // Open /dev/kmsg for output
    let kmsg_fd = unsafe {
        syscall3(__NR_OPENAT, AT_FDCWD as u64, b"/dev/kmsg\0".as_ptr() as u64, 1)
    };
    let out_fd = if kmsg_fd >= 0 { kmsg_fd as i32 } else { 1 };

    // Try to set up pipe + fork for /runner (V8 pre-init pattern)
    let mut pipe_fds = [0i32; 2];
    let pipe_ret = unsafe {
        syscall2(__NR_PIPE2, pipe_fds.as_mut_ptr() as u64, O_CLOEXEC)
    };

    let mut runner_pipe_write: i32 = -1;

    if pipe_ret >= 0 {
        let pipe_read = pipe_fds[0];
        let pipe_write = pipe_fds[1];

        // clone(SIGCHLD, 0) = fork()
        let child_pid = unsafe { syscall5(__NR_CLONE, SIGCHLD, 0, 0, 0, 0) };

        if child_pid == 0 {
            // --- CHILD PROCESS ---
            // Dup pipe_read to stdin, close pipe_write, exec /runner
            close(pipe_write);
            unsafe { syscall3(__NR_DUP3, pipe_read as u64, 0, 0); } // stdin = pipe_read
            close(pipe_read);

            let path = b"/runner\0";
            let argv: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
            let envp: [*const u8; 1] = [core::ptr::null()];
            unsafe {
                syscall3(__NR_EXECVE, path.as_ptr() as u64,
                    argv.as_ptr() as u64, envp.as_ptr() as u64);
            }
            // execve failed — exit child
            unsafe { syscall1(__NR_EXIT, 1); }
            loop {}
        } else if child_pid > 0 {
            // --- PARENT PROCESS ---
            close(pipe_read);
            runner_pipe_write = pipe_write;
            write_all(out_fd, b"[convex-init] forked /runner child\n");
        } else {
            // clone failed — fall through to simple mode
            close(pipe_fds[0]);
            close(pipe_fds[1]);
        }
    }

    // Map mailbox, write READY, spin
    let mailbox = map_mailbox_fresh();
    if mailbox.is_null() {
        write_all(out_fd, b"[convex-init] ERROR: failed to map mailbox\n");
        power_off();
    }

    write_all(out_fd, b"[convex-init] writing READY\n");
    unsafe {
        core::ptr::copy_nonoverlapping(READY_MAGIC.as_ptr(), mailbox, READY_MAGIC.len());
        *mailbox.add(READY_MAGIC.len()) = 0;
    }

    // Spin until mailbox changes (host wrote per-fork data)
    loop {
        let first_byte = unsafe { core::ptr::read_volatile(mailbox) };
        if first_byte != READY_MAGIC[0] { break; }
        unsafe { core::arch::asm!("isb"); }
    }

    // --- We are now in a forked VM ---
    // The OLD mailbox mmap is stale. Re-mmap fresh to see new data.
    munmap_mailbox(mailbox);
    let fresh_mailbox = map_mailbox_fresh();
    let msg = read_mailbox_str(fresh_mailbox as *const u8);

    if runner_pipe_write >= 0 && !msg.is_empty() {
        // Send JS to runner via pipe
        write_all(out_fd, b"[convex-init] sending JS to runner via pipe\n");
        write_all(runner_pipe_write, msg);
        close(runner_pipe_write);

        // Wait for runner child to exit
        let mut status: i32 = 0;
        unsafe {
            syscall4(__NR_WAIT4, u64::MAX, // -1 = any child
                &mut status as *mut i32 as u64, 0, 0);
        }
    } else {
        // No runner — print mailbox text directly
        if msg.is_empty() || msg == READY_MAGIC {
            write_all(out_fd, b"[convex-init] no message in mailbox\n");
        } else {
            write_all(out_fd, b"[convex-init] ");
            write_all(out_fd, msg);
            write_all(out_fd, b"\n");
        }
    }

    power_off();
}
