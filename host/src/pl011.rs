//! Minimal PL011 UART emulation for Linux earlycon.
//!
//! We only emulate enough for the kernel to use this as earlycon output:
//! - DR (data register): writes go to host stdout
//! - FR (flag register): always reports TX FIFO empty (ready to write)
//! - Other registers: ignored reads return 0, writes are discarded

use std::io::Write;

/// PL011 register offsets (from base address)
const UARTDR: u64 = 0x000; // Data register
const UARTFR: u64 = 0x018; // Flag register
const UARTIBRD: u64 = 0x024; // Integer baud rate
const UARTFBRD: u64 = 0x028; // Fractional baud rate
const UARTLCR_H: u64 = 0x02C; // Line control
const UARTCR: u64 = 0x030; // Control register
const UARTIMSC: u64 = 0x038; // Interrupt mask set/clear
const UARTICR: u64 = 0x044; // Interrupt clear
const UARTMIS: u64 = 0x040; // Masked interrupt status

/// Flag register bits
const FR_TXFE: u32 = 1 << 7; // TX FIFO empty
const FR_RXFE: u32 = 1 << 4; // RX FIFO empty

pub struct Pl011 {
    pub base_addr: u64,
    pub size: u64,
}

impl Pl011 {
    pub fn new(base_addr: u64) -> Self {
        Pl011 {
            base_addr,
            size: 0x1000,
        }
    }

    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.base_addr && addr < self.base_addr + self.size
    }

    /// Handle an MMIO read. Returns the value to give back to the guest.
    pub fn read(&self, offset: u64, _size: usize) -> u64 {
        match offset {
            UARTFR => (FR_TXFE | FR_RXFE) as u64, // TX empty, RX empty
            UARTMIS => 0,                          // no pending interrupts
            UARTCR => 0x0301,                      // UART enabled, TX enabled, RX enabled
            _ => 0,
        }
    }

    /// Handle an MMIO write.
    pub fn write(&self, offset: u64, value: u64, _size: usize) {
        match offset {
            UARTDR => {
                let ch = (value & 0xFF) as u8;
                let buf = [ch];
                let _ = std::io::stdout().write_all(&buf);
                let _ = std::io::stdout().flush();
            }
            // Silently accept writes to config/interrupt registers
            UARTIBRD | UARTFBRD | UARTLCR_H | UARTCR | UARTIMSC | UARTICR => {}
            _ => {}
        }
    }
}
