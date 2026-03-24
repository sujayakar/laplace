//! Minimal init process (PID 1) for our Linux VM.
//!
//! Lifecycle:
//! 1. Mount devtmpfs + procfs, open /dev/kmsg for output
//! 2. Create three pipes: JS pipe (stdin) + ready pipe (fd 3) + output pipe (stdout)
//! 3. Fork child, exec /runner with JS pipe as stdin, output pipe as stdout, ready pipe as fd 3
//! 4. Parent: block on ready pipe until runner signals "initialized"
//! 5. Parent: map shared region (inbox + outbox), write READY to inbox, spin until host overwrites
//! 6. After fork-resume: re-mmap shared region fresh, read JS from inbox, write to pipe
//! 7. Wait for child, read output pipe, write to outbox, power off

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
const __NR_FCNTL: u64 = 25;
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
const F_SETFD: u64 = 2;

// Inbox/outbox — must match LINUX_INBOX_GPA/LINUX_OUTBOX_GPA in host/src/linux_boot.rs
const INBOX_GPA: u64 = 0x3F00_0000;
const INBOX_SIZE: usize = 8 * 1024 * 1024; // 8 MiB
#[allow(dead_code)]
const OUTBOX_GPA: u64 = 0x3F80_0000; // inbox + 8 MiB
const OUTBOX_SIZE: usize = 8 * 1024 * 1024; // 8 MiB
const SHARED_SIZE: usize = 16 * 1024 * 1024; // inbox + outbox
const READY_MAGIC: &[u8] = b"CONVEX_READY";

fn write_all(fd: i32, buf: &[u8]) {
    let mut off = 0usize;
    while off < buf.len() {
        let n = unsafe {
            syscall3(
                __NR_WRITE,
                fd as u64,
                buf.as_ptr().add(off) as u64,
                (buf.len() - off) as u64,
            )
        };
        if n <= 0 {
            break;
        }
        off += n as usize;
    }
}

fn mkdirat(path: &[u8]) {
    unsafe {
        syscall3(__NR_MKDIRAT, AT_FDCWD as u64, path.as_ptr() as u64, 0o755);
    }
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

fn close(fd: i32) {
    unsafe {
        syscall1(__NR_CLOSE, fd as u64);
    }
}

/// Dup `old_fd` to `new_fd` without O_CLOEXEC (so it survives execve).
/// Handles the edge case where old_fd == new_fd: dup3 returns EINVAL in
/// that case, so we just clear FD_CLOEXEC via fcntl instead.
fn dup_to_fd(old_fd: i32, new_fd: i32) {
    if old_fd == new_fd {
        // Can't dup3 to self — just clear O_CLOEXEC
        unsafe {
            syscall3(__NR_FCNTL, old_fd as u64, F_SETFD, 0);
        }
    } else {
        // dup3 with flags=0 (no O_CLOEXEC) — closes new_fd if open
        unsafe {
            syscall3(__NR_DUP3, old_fd as u64, new_fd as u64, 0);
        }
        close(old_fd);
    }
}

fn map_shared_fresh() -> *mut u8 {
    let fd = unsafe {
        syscall3(
            __NR_OPENAT,
            AT_FDCWD as u64,
            b"/dev/mem\0".as_ptr() as u64,
            2,
        ) // O_RDWR
    };
    if fd < 0 {
        return core::ptr::null_mut();
    }
    let ptr = unsafe {
        syscall6(
            __NR_MMAP,
            0,
            SHARED_SIZE as u64,
            1 | 2,
            1,
            fd as u64,
            INBOX_GPA,
        )
    };
    close(fd as i32);
    if ptr < 0 {
        return core::ptr::null_mut();
    }
    ptr as *mut u8
}

fn munmap_shared(ptr: *mut u8) {
    unsafe {
        syscall2(__NR_MUNMAP, ptr as u64, SHARED_SIZE as u64);
    }
}

fn read_inbox_str(shared: *const u8) -> &'static [u8] {
    if shared.is_null() {
        return b"";
    }
    let mut len = 0;
    unsafe {
        while len < INBOX_SIZE && *shared.add(len) != 0 {
            len += 1;
        }
        core::slice::from_raw_parts(shared, len)
    }
}

#[allow(dead_code)]
fn write_outbox(shared: *mut u8, data: &[u8]) {
    if shared.is_null() || data.is_empty() {
        return;
    }
    let outbox = unsafe { shared.add(INBOX_SIZE) };
    let len = if data.len() < OUTBOX_SIZE {
        data.len()
    } else {
        OUTBOX_SIZE - 1
    };
    unsafe {
        core::ptr::copy_nonoverlapping(data.as_ptr(), outbox, len);
        *outbox.add(len) = 0; // null-terminate
    }
}

fn power_off() -> ! {
    unsafe {
        syscall3(__NR_REBOOT, 0xfee1dead, 0x28121969, 0x4321fedc);
    }
    loop {}
}

#[no_mangle]
pub extern "C" fn _start() -> ! {
    // Mount essential filesystems
    mkdirat(b"/dev\0");
    mount(b"devtmpfs\0", b"/dev\0", b"devtmpfs\0");
    mkdirat(b"/proc\0");
    mount(b"proc\0", b"/proc\0", b"proc\0");

    // Open /dev/kmsg for output (O_WRONLY | O_CLOEXEC to prevent leaking to runner)
    let kmsg_fd = unsafe {
        syscall3(
            __NR_OPENAT,
            AT_FDCWD as u64,
            b"/dev/kmsg\0".as_ptr() as u64,
            1 | O_CLOEXEC,
        ) // O_WRONLY | O_CLOEXEC
    };
    let out_fd = if kmsg_fd >= 0 { kmsg_fd as i32 } else { 1 };

    // Try to set up pipes + fork for /runner (V8 pre-init pattern)
    // JS pipe: init writes JS to runner's stdin after fork-resume
    // Ready pipe: runner writes 1 byte when initialized, init blocks on it
    // Output pipe: runner's stdout → init reads after exit → outbox
    let mut js_pipe = [0i32; 2];
    let mut ready_pipe = [0i32; 2];
    let mut output_pipe = [0i32; 2];
    let js_pipe_ret = unsafe { syscall2(__NR_PIPE2, js_pipe.as_mut_ptr() as u64, O_CLOEXEC) };
    let ready_pipe_ret = unsafe { syscall2(__NR_PIPE2, ready_pipe.as_mut_ptr() as u64, O_CLOEXEC) };
    let output_pipe_ret =
        unsafe { syscall2(__NR_PIPE2, output_pipe.as_mut_ptr() as u64, O_CLOEXEC) };

    let mut runner_pipe_write: i32 = -1;
    let mut ready_pipe_read: i32 = -1;
    let mut output_pipe_read: i32 = -1;

    if js_pipe_ret >= 0 && ready_pipe_ret >= 0 && output_pipe_ret >= 0 {
        // clone(SIGCHLD, 0) = fork()
        let child_pid = unsafe { syscall5(__NR_CLONE, SIGCHLD, 0, 0, 0, 0) };

        if child_pid == 0 {
            // --- CHILD PROCESS ---
            // Close parent-side ends
            close(js_pipe[1]); // JS pipe write end
            close(ready_pipe[0]); // ready pipe read end
            close(output_pipe[0]); // output pipe read end

            // Dup JS pipe read → fd 0 (stdin), without O_CLOEXEC
            dup_to_fd(js_pipe[0], 0);

            // Dup output pipe write → fd 1 (stdout), without O_CLOEXEC
            dup_to_fd(output_pipe[1], 1);

            // Dup ready pipe write → fd 3, without O_CLOEXEC (must survive execve)
            dup_to_fd(ready_pipe[1], 3);

            let path = b"/runner\0";
            let argv: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
            let envp: [*const u8; 1] = [core::ptr::null()];
            unsafe {
                syscall3(
                    __NR_EXECVE,
                    path.as_ptr() as u64,
                    argv.as_ptr() as u64,
                    envp.as_ptr() as u64,
                );
            }
            // execve failed — exit child
            unsafe {
                syscall1(__NR_EXIT, 1);
            }
            loop {}
        } else if child_pid > 0 {
            // --- PARENT PROCESS ---
            close(js_pipe[0]); // JS pipe read end (child has it)
            close(ready_pipe[1]); // ready pipe write end (child has it)
            close(output_pipe[1]); // output pipe write end (child has it)
            runner_pipe_write = js_pipe[1];
            ready_pipe_read = ready_pipe[0];
            output_pipe_read = output_pipe[0];
            write_all(out_fd, b"[convex-init] forked /runner child\n");
        } else {
            // clone failed — fall through to simple mode
            close(js_pipe[0]);
            close(js_pipe[1]);
            close(ready_pipe[0]);
            close(ready_pipe[1]);
            close(output_pipe[0]);
            close(output_pipe[1]);
        }
    } else {
        // pipe creation failed — clean up any that succeeded
        if js_pipe_ret >= 0 {
            close(js_pipe[0]);
            close(js_pipe[1]);
        }
        if ready_pipe_ret >= 0 {
            close(ready_pipe[0]);
            close(ready_pipe[1]);
        }
        if output_pipe_ret >= 0 {
            close(output_pipe[0]);
            close(output_pipe[1]);
        }
    }

    // Wait for runner to signal readiness (if we have a runner)
    if ready_pipe_read >= 0 {
        write_all(
            out_fd,
            b"[convex-init] waiting for runner to initialize...\n",
        );
        let mut ready_byte = [0u8; 1];
        let n = unsafe {
            syscall3(
                __NR_READ,
                ready_pipe_read as u64,
                ready_byte.as_mut_ptr() as u64,
                1,
            )
        };
        close(ready_pipe_read);
        if n > 0 {
            write_all(out_fd, b"[convex-init] runner signaled ready\n");
        } else {
            write_all(
                out_fd,
                b"[convex-init] runner exited without signaling ready\n",
            );
        }
    }

    // Map shared region (inbox + outbox), write READY to inbox, spin
    let shared = map_shared_fresh();
    if shared.is_null() {
        write_all(
            out_fd,
            b"[convex-init] ERROR: failed to map shared region\n",
        );
        power_off();
    }

    write_all(out_fd, b"[convex-init] writing READY\n");
    unsafe {
        core::ptr::copy_nonoverlapping(READY_MAGIC.as_ptr(), shared, READY_MAGIC.len());
        *shared.add(READY_MAGIC.len()) = 0;
    }

    // Spin until inbox changes (host wrote per-fork data).
    // Compare first 8 bytes ("CONVEX_R") to avoid false positives if
    // JS happens to start with a matching prefix.
    let ready_tag = u64::from_ne_bytes([
        READY_MAGIC[0],
        READY_MAGIC[1],
        READY_MAGIC[2],
        READY_MAGIC[3],
        READY_MAGIC[4],
        READY_MAGIC[5],
        READY_MAGIC[6],
        READY_MAGIC[7],
    ]);
    loop {
        let tag = unsafe { core::ptr::read_volatile(shared as *const u64) };
        if tag != ready_tag {
            break;
        }
        unsafe {
            core::arch::asm!("isb");
        }
    }

    // --- We are now in a forked VM ---
    // The OLD shared mmap is stale. Re-mmap fresh to see new data.
    munmap_shared(shared);
    let fresh_shared = map_shared_fresh();
    let msg = read_inbox_str(fresh_shared as *const u8);

    if runner_pipe_write >= 0 {
        // Send JS to runner via pipe (or just close to send EOF if no JS)
        if !msg.is_empty() {
            write_all(out_fd, b"[convex-init] sending JS to runner via pipe\n");
            write_all(runner_pipe_write, msg);
        } else {
            write_all(out_fd, b"[convex-init] no JS in inbox, closing pipe\n");
        }
        close(runner_pipe_write);

        // Drain the output pipe BEFORE wait4. If we wait4 first, the runner
        // can deadlock: it fills the pipe buffer (~64KB), blocks on write(),
        // never exits, and init stays stuck in wait4 forever.
        let mut total_written = 0usize;
        let mut truncated = false;
        if output_pipe_read >= 0 {
            let mut buf = [0u8; 4096];
            let outbox = unsafe { fresh_shared.add(INBOX_SIZE) };
            // Reserve space for potential truncation message + null
            let max_output = OUTBOX_SIZE - 32; // leave room for error/truncation suffix
            loop {
                let n = unsafe {
                    syscall3(
                        __NR_READ,
                        output_pipe_read as u64,
                        buf.as_mut_ptr() as u64,
                        buf.len() as u64,
                    )
                };
                if n <= 0 {
                    break;
                } // EOF (runner exited) or error
                let n = n as usize;
                let remaining = if max_output > total_written {
                    max_output - total_written
                } else {
                    0
                };
                let to_copy = if n < remaining { n } else { remaining };
                if to_copy > 0 {
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            buf.as_ptr(),
                            outbox.add(total_written),
                            to_copy,
                        );
                    }
                    total_written += to_copy;
                }
                if to_copy < n {
                    truncated = true;
                    // Keep reading to drain the pipe (avoid blocking runner) but discard
                }
            }
            close(output_pipe_read);
        }

        // Wait for runner child to exit (it already closed stdout → pipe EOF above)
        let mut status: i32 = 0;
        unsafe {
            syscall4(
                __NR_WAIT4,
                (-1i64) as u64,
                &mut status as *mut i32 as u64,
                0,
                0,
            );
        }

        // Append status/truncation info to outbox
        let outbox = unsafe { fresh_shared.add(INBOX_SIZE) };
        if truncated {
            let msg = b"\n[output truncated]\n";
            let space = OUTBOX_SIZE - total_written - 1;
            let len = if msg.len() < space { msg.len() } else { space };
            unsafe {
                core::ptr::copy_nonoverlapping(msg.as_ptr(), outbox.add(total_written), len);
            }
            total_written += len;
        }
        // Check runner exit status: WIFEXITED(s) = (s & 0x7f) == 0, WEXITSTATUS = (s >> 8) & 0xff
        let exited_normally = (status & 0x7f) == 0;
        let exit_code = (status >> 8) & 0xff;
        if !exited_normally || exit_code != 0 {
            // Append error to outbox
            let code_byte = b'0' + (exit_code as u8 % 10); // simple single-digit
            let err = [
                b'\n', b'[', b'r', b'u', b'n', b'n', b'e', b'r', b' ', b'e', b'x', b'i', b't',
                b' ', code_byte, b']', b'\n',
            ];
            let space = OUTBOX_SIZE - total_written - 1;
            let len = if err.len() < space { err.len() } else { space };
            unsafe {
                core::ptr::copy_nonoverlapping(err.as_ptr(), outbox.add(total_written), len);
            }
            total_written += len;
        }
        // Null-terminate outbox
        if total_written < OUTBOX_SIZE {
            unsafe {
                *outbox.add(total_written) = 0;
            }
        }
    } else {
        // No runner — print inbox text directly to outbox
        let outbox = unsafe { fresh_shared.add(INBOX_SIZE) };
        if !msg.is_empty() && msg != READY_MAGIC {
            let len = if msg.len() < OUTBOX_SIZE - 1 {
                msg.len()
            } else {
                OUTBOX_SIZE - 1
            };
            unsafe {
                core::ptr::copy_nonoverlapping(msg.as_ptr(), outbox, len);
                *outbox.add(len) = 0;
            }
        }
    }

    power_off();
}
