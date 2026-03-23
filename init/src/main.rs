//! Minimal init process (PID 1) for our Linux VM.
//!
//! Uses raw inline asm syscalls to avoid any libc/runtime complexity.

#![no_std]
#![no_main]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

unsafe fn syscall3(nr: u64, a0: u64, a1: u64, a2: u64) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "svc #0",
        in("x8") nr,
        inlateout("x0") a0 => ret,
        in("x1") a1,
        in("x2") a2,
        clobber_abi("C"),
    );
    ret
}

unsafe fn syscall5(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "svc #0",
        in("x8") nr,
        inlateout("x0") a0 => ret,
        in("x1") a1,
        in("x2") a2,
        in("x3") a3,
        in("x4") a4,
        clobber_abi("C"),
    );
    ret
}

const __NR_DUP3: u64 = 24;
const __NR_MKDIRAT: u64 = 34;
const __NR_MOUNT: u64 = 40;
const __NR_OPENAT: u64 = 56;
const __NR_WRITE: u64 = 64;
const __NR_REBOOT: u64 = 142;

const AT_FDCWD: i32 = -100;
const O_RDWR: u64 = 2;

fn write_all(fd: i32, buf: &[u8]) {
    unsafe { syscall3(__NR_WRITE, fd as u64, buf.as_ptr() as u64, buf.len() as u64); }
}

fn mkdirat(path: &[u8]) {
    unsafe { syscall3(__NR_MKDIRAT, AT_FDCWD as u64, path.as_ptr() as u64, 0o755); }
}

fn mount(src: &[u8], target: &[u8], fstype: &[u8]) {
    unsafe {
        syscall5(
            __NR_MOUNT,
            src.as_ptr() as u64,
            target.as_ptr() as u64,
            fstype.as_ptr() as u64,
            0,
            0,
        );
    }
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Mount essential filesystems
    mkdirat(b"/dev\0");
    mount(b"devtmpfs\0", b"/dev\0", b"devtmpfs\0");
    mkdirat(b"/proc\0");
    mount(b"proc\0", b"/proc\0", b"proc\0");

    // Write to /dev/kmsg — the kernel log ring buffer.
    // This always works and outputs via earlycon/printk.
    let kmsg_fd = unsafe {
        syscall3(__NR_OPENAT, AT_FDCWD as u64, b"/dev/kmsg\0".as_ptr() as u64, 1) // O_WRONLY=1
    };
    if kmsg_fd >= 0 {
        write_all(kmsg_fd as i32, b"Hello from Linux in the VM!\n");
    }

    // Power off
    unsafe {
        syscall3(__NR_REBOOT, 0xfee1dead, 0x28121969, 0x4321fedc);
    }
    loop {}
}

