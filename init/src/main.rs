//! Minimal init process (PID 1) for our Linux VM.
//!
//! Lifecycle:
//! 1. Mount devtmpfs + procfs, open /dev/kmsg for output
//! 2. Map the mailbox page via /dev/mem (at MAILBOX_GPA)
//! 3. Write READY_MAGIC to the mailbox, then spin until the host overwrites it
//! 4. Host detects READY via periodic forced VM exits, snapshots the VM
//! 5. On fork-resume, the mailbox contains per-fork data — read and print it
//! 6. Power off via reboot(POWER_OFF)

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

/// Syscall with 6 arguments (needed for mmap).
unsafe fn syscall6(nr: u64, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let ret: i64;
    core::arch::asm!(
        "svc #0",
        in("x8") nr,
        inlateout("x0") a0 => ret,
        in("x1") a1,
        in("x2") a2,
        in("x3") a3,
        in("x4") a4,
        in("x5") a5,
        clobber_abi("C"),
    );
    ret
}

const __NR_MKDIRAT: u64 = 34;
const __NR_MOUNT: u64 = 40;
const __NR_OPENAT: u64 = 56;
const __NR_WRITE: u64 = 64;
const __NR_REBOOT: u64 = 142;
const __NR_MMAP: u64 = 222;

const AT_FDCWD: i32 = -100;

// Mailbox: a known guest physical address where the host writes per-fork data.
// Must match LINUX_MAILBOX_GPA in linux_boot.rs.
const MAILBOX_GPA: u64 = 0x3FFF_0000;
const MAILBOX_SIZE: usize = 64 * 1024;

// Magic marker written to mailbox to signal "ready for snapshot"
const READY_MAGIC: &[u8] = b"CONVEX_READY";

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

/// Map the mailbox page into our address space via mmap of /dev/mem.
/// Returns a pointer to the mailbox data, or null on failure.
fn map_mailbox(writable: bool) -> *mut u8 {
    let flags: u64 = if writable { 2 } else { 0 }; // O_RDWR=2, O_RDONLY=0
    let fd = unsafe {
        syscall3(__NR_OPENAT, AT_FDCWD as u64, b"/dev/mem\0".as_ptr() as u64, flags)
    };
    if fd < 0 {
        return core::ptr::null_mut();
    }

    let prot: u64 = if writable { 1 | 2 } else { 1 }; // PROT_READ | PROT_WRITE
    // mmap(NULL, size, prot, MAP_SHARED, fd, offset=MAILBOX_GPA)
    let ptr = unsafe {
        syscall6(
            __NR_MMAP,
            0,                    // addr
            MAILBOX_SIZE as u64,  // length
            prot,                 // prot
            1,                    // MAP_SHARED
            fd as u64,            // fd
            MAILBOX_GPA,          // offset (6th arg)
        )
    };

    if ptr < 0 {
        return core::ptr::null_mut();
    }
    ptr as *mut u8
}

/// Read a null-terminated string from the mailbox.
fn read_mailbox_str(mailbox: *const u8) -> &'static [u8] {
    if mailbox.is_null() {
        return b"(mailbox not mapped)";
    }
    let mut len = 0;
    unsafe {
        while len < MAILBOX_SIZE && *mailbox.add(len) != 0 {
            len += 1;
        }
        core::slice::from_raw_parts(mailbox, len)
    }
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

    // Map the mailbox page (read-write so we can write the READY marker)
    let mailbox = map_mailbox(true);
    if mailbox.is_null() {
        write_all(out_fd, b"[convex-init] ERROR: failed to map mailbox via /dev/mem\n");
        unsafe { syscall3(__NR_REBOOT, 0xfee1dead, 0x28121969, 0x4321fedc); }
        loop {}
    }

    // If /runner exists and mailbox is empty (direct boot, not fork),
    // exec the runner immediately without going through snapshot flow.
    let first_byte = unsafe { core::ptr::read_volatile(mailbox) };
    if first_byte == 0 {
        // Mailbox is empty — this is a direct boot, not a fork resume.
        // Try to exec /runner directly for testing.
        write_all(out_fd, b"[convex-init] direct boot, trying /runner\n");
        unsafe {
            let path = b"/runner\0";
            let argv: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
            let envp: [*const u8; 1] = [core::ptr::null()];
            syscall3(221, path.as_ptr() as u64, argv.as_ptr() as u64, envp.as_ptr() as u64);
            // execve failed — /runner doesn't exist, continue to snapshot flow
        }
    }

    // Write READY marker to mailbox.
    // The host will force a VM exit, see the marker, and snapshot.
    // After fork, the host overwrites the mailbox with per-fork data
    // and resumes — we'll detect the change and proceed.
    write_all(out_fd, b"[convex-init] initialized, writing READY to mailbox\n");
    unsafe {
        core::ptr::copy_nonoverlapping(READY_MAGIC.as_ptr(), mailbox, READY_MAGIC.len());
        *mailbox.add(READY_MAGIC.len()) = 0;
    }

    // Spin until the mailbox content changes (host wrote per-fork data).
    // The host's snapshot captures this spin state. On fork-resume, the
    // mailbox has new data, so this loop exits immediately.
    loop {
        let first_byte = unsafe { core::ptr::read_volatile(mailbox) };
        // READY_MAGIC starts with 'C' (0x43). When the host writes new data
        // (or clears it), the first byte changes.
        if first_byte != READY_MAGIC[0] {
            break;
        }
        // Brief yield — ISB to prevent tight spin from being optimized away
        unsafe { core::arch::asm!("isb"); }
    }

    // --- We are now running in a forked VM ---

    // Try to exec /runner (e.g. QuickJS runner). If present, it takes over
    // and handles the mailbox content. If not, fall back to printing as text.
    unsafe {
        let path = b"/runner\0";
        let argv: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
        let envp: [*const u8; 1] = [core::ptr::null()];
        syscall3(221, path.as_ptr() as u64, argv.as_ptr() as u64, envp.as_ptr() as u64);
        // If we get here, execve failed — fall through to text output
    }

    let msg = read_mailbox_str(mailbox);
    if msg.is_empty() || msg == READY_MAGIC {
        write_all(out_fd, b"[convex-init] no message in mailbox\n");
    } else {
        write_all(out_fd, b"[convex-init] ");
        write_all(out_fd, msg);
        write_all(out_fd, b"\n");
    }

    // Power off
    unsafe { syscall3(__NR_REBOOT, 0xfee1dead, 0x28121969, 0x4321fedc); }
    loop {}
}

