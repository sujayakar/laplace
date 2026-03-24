#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::arch::asm;
use core::cell::UnsafeCell;
use core::panic::PanicInfo;

use convex_shared::{HC_CONSOLE, HC_EXIT, HC_READY, HC_TIME};
use convex_shared::{HEAP_GPA, HEAP_SIZE, MAILBOX_GPA, MAILBOX_SIZE};

mod quickjs_ffi;

// ── Hypercall shim ──────────────────────────────────────────────

#[inline(always)]
unsafe fn hypercall(id: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    asm!(
        "hvc #0",
        in("x0") id,
        in("x1") a1,
        in("x2") a2,
        in("x3") a3,
        lateout("x0") ret,
        options(nostack)
    );
    ret
}

fn console_write(s: &str) {
    unsafe {
        hypercall(HC_CONSOLE, s.as_ptr() as u64, s.len() as u64, 0);
    }
}

fn console_write_bytes(s: &[u8]) {
    unsafe {
        hypercall(HC_CONSOLE, s.as_ptr() as u64, s.len() as u64, 0);
    }
}

// ── C callbacks (called from libc_stubs.c) ──────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn _guest_console_write(ptr: *const u8, len: usize) {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    console_write_bytes(bytes);
}

#[unsafe(no_mangle)]
pub extern "C" fn _guest_abort() -> ! {
    console_write("ABORT in guest!\n");
    unsafe {
        hypercall(HC_EXIT, 1, 0, 0);
    }
    loop {}
}

#[unsafe(no_mangle)]
pub extern "C" fn _guest_get_time_ns() -> u64 {
    unsafe { hypercall(HC_TIME, 0, 0, 0) }
}

// ── Bump allocator ──────────────────────────────────────────────

struct BumpAllocator {
    next: UnsafeCell<usize>,
    end: usize,
}

// Safety: single vCPU, no threading
unsafe impl Sync for BumpAllocator {}

unsafe impl GlobalAlloc for BumpAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let current = *self.next.get();
        let aligned = (current + layout.align() - 1) & !(layout.align() - 1);
        let new_next = aligned + layout.size();
        if new_next > self.end {
            return core::ptr::null_mut();
        }
        *self.next.get() = new_next;
        aligned as *mut u8
    }

    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {
        // Bump allocator doesn't free. VM is torn down after each invocation.
    }
}

#[global_allocator]
static ALLOCATOR: BumpAllocator = BumpAllocator {
    next: UnsafeCell::new(HEAP_GPA as usize),
    end: (HEAP_GPA as usize) + HEAP_SIZE,
};

// ── C malloc/free/realloc (for QuickJS) ─────────────────────────

const MALLOC_HEADER_SIZE: usize = 16;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn malloc(size: usize) -> *mut u8 {
    let total_size = match MALLOC_HEADER_SIZE.checked_add(size.max(1)) {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };
    let total = match Layout::from_size_align(total_size, 16) {
        Ok(l) => l,
        Err(_) => return core::ptr::null_mut(),
    };
    let ptr = ALLOCATOR.alloc(total);
    if ptr.is_null() {
        return ptr;
    }
    // Store original size in header for realloc
    *(ptr as *mut usize) = size;
    ptr.add(MALLOC_HEADER_SIZE)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn calloc(count: usize, size: usize) -> *mut u8 {
    let total = match count.checked_mul(size) {
        Some(s) => s,
        None => return core::ptr::null_mut(),
    };
    let ptr = malloc(total);
    if !ptr.is_null() {
        core::ptr::write_bytes(ptr, 0, total);
    }
    ptr
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn realloc(ptr: *mut u8, new_size: usize) -> *mut u8 {
    if ptr.is_null() {
        return malloc(new_size);
    }
    let old_size = *(ptr.sub(MALLOC_HEADER_SIZE) as *const usize);
    let new_ptr = malloc(new_size);
    if !new_ptr.is_null() {
        let copy_size = if old_size < new_size {
            old_size
        } else {
            new_size
        };
        core::ptr::copy_nonoverlapping(ptr, new_ptr, copy_size);
    }
    // old memory is not freed (bump allocator)
    new_ptr
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn free(_ptr: *mut u8) {
    // Bump allocator — no-op
}

// ── libm symbols (forwarded to the `libm` crate) ───────────────

macro_rules! libm_export {
    ($name:ident, $f:path, ($($arg:ident: $ty:ty),*) -> $ret:ty) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn $name($($arg: $ty),*) -> $ret {
            $f($($arg),*)
        }
    };
}

libm_export!(sin, libm::sin, (x: f64) -> f64);
libm_export!(cos, libm::cos, (x: f64) -> f64);
libm_export!(tan, libm::tan, (x: f64) -> f64);
libm_export!(asin, libm::asin, (x: f64) -> f64);
libm_export!(acos, libm::acos, (x: f64) -> f64);
libm_export!(atan, libm::atan, (x: f64) -> f64);
libm_export!(atan2, libm::atan2, (y: f64, x: f64) -> f64);
libm_export!(pow, libm::pow, (x: f64, y: f64) -> f64);
libm_export!(sqrt, libm::sqrt, (x: f64) -> f64);
libm_export!(log, libm::log, (x: f64) -> f64);
libm_export!(log2, libm::log2, (x: f64) -> f64);
libm_export!(log10, libm::log10, (x: f64) -> f64);
libm_export!(log1p, libm::log1p, (x: f64) -> f64);
libm_export!(exp, libm::exp, (x: f64) -> f64);
libm_export!(exp2, libm::exp2, (x: f64) -> f64);
libm_export!(expm1, libm::expm1, (x: f64) -> f64);
libm_export!(floor, libm::floor, (x: f64) -> f64);
libm_export!(ceil, libm::ceil, (x: f64) -> f64);
libm_export!(round, libm::round, (x: f64) -> f64);
libm_export!(trunc, libm::trunc, (x: f64) -> f64);
libm_export!(fabs, libm::fabs, (x: f64) -> f64);
libm_export!(fmod, libm::fmod, (x: f64, y: f64) -> f64);
libm_export!(remainder, libm::remainder, (x: f64, y: f64) -> f64);
libm_export!(cbrt, libm::cbrt, (x: f64) -> f64);
libm_export!(hypot, libm::hypot, (x: f64, y: f64) -> f64);
libm_export!(copysign, libm::copysign, (x: f64, y: f64) -> f64);
libm_export!(scalbn, libm::scalbn, (x: f64, n: i32) -> f64);
libm_export!(ldexp, libm::ldexp, (x: f64, n: i32) -> f64);
// frexp/modf/lrint have special C signatures — defined manually below
libm_export!(cosh, libm::cosh, (x: f64) -> f64);
libm_export!(sinh, libm::sinh, (x: f64) -> f64);
libm_export!(tanh, libm::tanh, (x: f64) -> f64);
libm_export!(acosh, libm::acosh, (x: f64) -> f64);
libm_export!(asinh, libm::asinh, (x: f64) -> f64);
libm_export!(atanh, libm::atanh, (x: f64) -> f64);
libm_export!(fminf, libm::fminf, (x: f32, y: f32) -> f32);
libm_export!(fmaxf, libm::fmaxf, (x: f32, y: f32) -> f32);
libm_export!(fmin, libm::fmin, (x: f64, y: f64) -> f64);
libm_export!(fmax, libm::fmax, (x: f64, y: f64) -> f64);
libm_export!(rint, libm::rint, (x: f64) -> f64);

// frexp needs special handling — C signature is double frexp(double, int*)
// but libm returns (f64, i32)
#[unsafe(no_mangle)]
pub extern "C" fn frexp(x: f64, exp: *mut i32) -> f64 {
    let (frac, e) = libm::frexp(x);
    unsafe {
        *exp = e;
    }
    frac
}

#[unsafe(no_mangle)]
pub extern "C" fn modf(x: f64, iptr: *mut f64) -> f64 {
    let (frac, int_part) = libm::modf(x);
    unsafe {
        *iptr = int_part;
    }
    frac
}

#[unsafe(no_mangle)]
pub extern "C" fn nearbyint(x: f64) -> f64 {
    libm::rint(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn lrint(x: f64) -> i64 {
    libm::rint(x) as i64
}

// ── Main entry point ────────────────────────────────────────────

#[unsafe(no_mangle)]
pub extern "C" fn guest_main() -> ! {
    console_write("Guest initializing (QuickJS)...\n");

    // Signal snapshot point
    unsafe {
        hypercall(HC_READY, 0, 0, 0);
    }

    // --- Execution resumes here after fork ---
    // Read JS code from mailbox
    let mailbox = unsafe { core::slice::from_raw_parts(MAILBOX_GPA as *const u8, MAILBOX_SIZE) };
    // Find null terminator
    let js_len = mailbox.iter().position(|&b| b == 0).unwrap_or(MAILBOX_SIZE);
    let js_code = &mailbox[..js_len];

    if js_len == 0 {
        console_write("No JS code in mailbox.\n");
        unsafe {
            hypercall(HC_EXIT, 0, 0, 0);
        }
        loop {}
    }

    // Run JS via QuickJS
    let exit_code = quickjs_ffi::eval_js(js_code);

    unsafe {
        hypercall(HC_EXIT, exit_code, 0, 0);
    }
    loop {}
}

// Exception vector table for debugging — each entry does HVC with ESR in x1
// so the host can see what EL1 exception occurred.
core::arch::global_asm!(
    ".section .text.vectors, \"ax\"",
    ".balign 2048",
    ".global _vectors",
    "_vectors:",
    // --- Current EL with SP_EL0 (not used since we run EL1h) ---
    // 0x000: Synchronous
    "mrs x1, esr_el1",
    "mrs x2, far_el1",
    "mrs x3, elr_el1",
    "mov x0, #0xDE", // HC_EXCEPTION (debug)
    "hvc #0",
    "b .",
    ".balign 0x80",
    // 0x080: IRQ
    "b .",
    ".balign 0x80",
    // 0x100: FIQ
    "b .",
    ".balign 0x80",
    // 0x180: SError
    "b .",
    ".balign 0x80",
    // --- Current EL with SP_ELx (this is what we use — EL1h with SP_EL1) ---
    // 0x200: Synchronous
    "mrs x1, esr_el1",
    "mrs x2, far_el1",
    "mrs x3, elr_el1",
    "mov x0, #0xDE", // HC_EXCEPTION (debug)
    "hvc #0",
    "b .",
    ".balign 0x80",
    // 0x280: IRQ
    "b .",
    ".balign 0x80",
    // 0x300: FIQ
    "b .",
    ".balign 0x80",
    // 0x380: SError
    "b .",
);

#[unsafe(no_mangle)]
#[link_section = ".text._start"]
pub unsafe extern "C" fn _start() -> ! {
    asm!(
        // Install exception vector table
        "adr x0, _vectors",
        "msr vbar_el1, x0",
        "isb",
        // Zero BSS
        "adr x0, __bss_start",
        "adr x1, __bss_end",
        "2:",
        "cmp x0, x1",
        "b.ge 3f",
        "str xzr, [x0], #8",
        "b 2b",
        "3:",
        "bl guest_main",
        options(noreturn)
    );
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    console_write("PANIC: guest panicked\n");
    unsafe {
        hypercall(HC_EXIT, 1, 0, 0);
    }
    loop {}
}
