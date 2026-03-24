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
            // PL011 identification registers (PeriphID and CellID)
            // These must return the correct values for the Linux PL011 driver
            // to recognize the device (checked by amba_bus match).
            0xFE0 => 0x11, // UART_PERIPHID0: partnum[7:0]
            0xFE4 => 0x10, // UART_PERIPHID1: designer[3:0] | partnum[11:8]
            0xFE8 => 0x14, // UART_PERIPHID2: revision | designer[7:4] (ARM = 0x41 -> 0x14)
            0xFEC => 0x00, // UART_PERIPHID3
            0xFF0 => 0x0D, // UART_CELLID0: 0x0D
            0xFF4 => 0xF0, // UART_CELLID1: 0xF0
            0xFF8 => 0x05, // UART_CELLID2: 0x05
            0xFFC => 0xB1, // UART_CELLID3: 0xB1
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_range() {
        let uart = Pl011::new(0x0900_0000);
        assert!(uart.contains(0x0900_0000));
        assert!(uart.contains(0x0900_0FFF));
        assert!(!uart.contains(0x0900_1000));
        assert!(!uart.contains(0x08FF_FFFF));
    }

    #[test]
    fn flag_register_reports_tx_empty() {
        let uart = Pl011::new(0x0900_0000);
        let fr = uart.read(UARTFR, 4);
        assert_ne!(fr & FR_TXFE as u64, 0, "TX FIFO should be empty");
        assert_ne!(fr & FR_RXFE as u64, 0, "RX FIFO should be empty");
    }

    #[test]
    fn write_to_data_register_does_not_panic() {
        let uart = Pl011::new(0x0900_0000);
        uart.write(UARTDR, b'A' as u64, 1);
        uart.write(UARTDR, b'\n' as u64, 1);
    }

    #[test]
    fn config_writes_accepted_silently() {
        let uart = Pl011::new(0x0900_0000);
        uart.write(UARTIBRD, 0x1234, 4);
        uart.write(UARTCR, 0x0301, 4);
        uart.write(UARTIMSC, 0, 4);
        uart.write(UARTICR, 0xFFFF, 4);
    }

    #[test]
    fn unknown_register_reads_zero() {
        let uart = Pl011::new(0x0900_0000);
        assert_eq!(uart.read(0x100, 4), 0);
    }

    #[test]
    fn identification_registers() {
        let uart = Pl011::new(0x0900_0000);
        // CellID should be 0x0D, 0xF0, 0x05, 0xB1
        assert_eq!(uart.read(0xFF0, 4), 0x0D);
        assert_eq!(uart.read(0xFF4, 4), 0xF0);
        assert_eq!(uart.read(0xFF8, 4), 0x05);
        assert_eq!(uart.read(0xFFC, 4), 0xB1);
    }
}
