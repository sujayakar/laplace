#![no_std]

// Hypercall IDs — must be kept in sync between guest and host.
pub const HC_CONSOLE: u64 = 0x01;
pub const HC_TIME: u64 = 0x02;
pub const HC_RANDOM: u64 = 0x03;
pub const HC_DB_READ: u64 = 0x10;
pub const HC_DB_WRITE: u64 = 0x11;
pub const HC_ALLOC: u64 = 0x20;
pub const HC_READY: u64 = 0xFE;
pub const HC_EXIT: u64 = 0xFF;

// Guest memory layout constants.
// Mailbox: 64 KiB region at offset 0x10_0000 (1 MiB) from GUEST_BASE.
// The host writes a request here before resuming a forked VM.
pub const GUEST_BASE: u64 = 0x4000_0000;
pub const GUEST_MEM_SIZE: usize = 64 * 1024 * 1024; // 64 MiB total

// Mailbox: 64 KiB at offset 1 MiB
pub const MAILBOX_OFFSET: u64 = 0x10_0000;
pub const MAILBOX_GPA: u64 = GUEST_BASE + MAILBOX_OFFSET;
pub const MAILBOX_SIZE: usize = 64 * 1024;

// Heap: starts at 2 MiB offset, extends to end minus stack
pub const HEAP_OFFSET: u64 = 0x20_0000;
pub const HEAP_GPA: u64 = GUEST_BASE + HEAP_OFFSET;
pub const HEAP_SIZE: usize = 60 * 1024 * 1024; // 60 MiB for heap

// Stack: top 2 MiB of guest memory (grows downward)
pub const STACK_SIZE: usize = 2 * 1024 * 1024;
