//! Minimal 8250/16550A UART emulation for earlycon output.
//! Currently unused (Cloud Hypervisor kernel uses PL011), but retained
//! for compatibility with Firecracker kernels.
//!
//! The 8250 is simpler than PL011: just a handful of byte-wide registers.
//! We only emulate enough for earlycon and console output:
//! - THR (TX holding register): writes go to host stdout
//! - LSR (line status register): always reports TX empty

use std::io::Write;

/// 8250 register offsets (byte-addressed)
const THR: u64 = 0x00; // Transmit Holding Register (write)
const RBR: u64 = 0x00; // Receive Buffer Register (read)
const IER: u64 = 0x01; // Interrupt Enable Register
const IIR: u64 = 0x02; // Interrupt Identification Register (read)
const FCR: u64 = 0x02; // FIFO Control Register (write)
const LCR: u64 = 0x03; // Line Control Register
const MCR: u64 = 0x04; // Modem Control Register
const LSR: u64 = 0x05; // Line Status Register
const MSR: u64 = 0x06; // Modem Status Register
const SCR: u64 = 0x07; // Scratch Register

/// LSR bits
const LSR_DR: u8 = 0x01; // Data Ready
const LSR_THRE: u8 = 0x20; // TX Holding Register Empty
const LSR_TEMT: u8 = 0x40; // Transmitter Empty

pub struct Uart8250 {
    pub base_addr: u64,
    pub size: u64,
    lcr: u8,
    mcr: u8,
    scr: u8,
    ier: u8,
    dll: u8, // Divisor Latch Low (when DLAB=1)
    dlh: u8, // Divisor Latch High (when DLAB=1)
}

impl Uart8250 {
    pub fn new(base_addr: u64) -> Self {
        Uart8250 {
            base_addr,
            size: 0x8, // 8 registers
            lcr: 0,
            mcr: 0,
            scr: 0,
            ier: 0,
            dll: 0,
            dlh: 0,
        }
    }

    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.base_addr && addr < self.base_addr + self.size
    }

    pub fn read(&self, offset: u64, _size: usize) -> u64 {
        let dlab = (self.lcr & 0x80) != 0;
        match offset {
            RBR if !dlab => 0,               // No data to read
            0x00 if dlab => self.dll as u64, // Divisor Latch Low
            IER if !dlab => self.ier as u64,
            0x01 if dlab => self.dlh as u64, // Divisor Latch High
            IIR => 0x01,                     // No interrupt pending
            LCR => self.lcr as u64,
            MCR => self.mcr as u64,
            LSR => (LSR_THRE | LSR_TEMT) as u64, // TX always ready, no RX data
            MSR => 0,                            // No modem signals
            SCR => self.scr as u64,
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u64, _size: usize) {
        let dlab = (self.lcr & 0x80) != 0;
        match offset {
            THR if !dlab => {
                let ch = (value & 0xFF) as u8;
                let buf = [ch];
                let _ = std::io::stdout().write_all(&buf);
                let _ = std::io::stdout().flush();
            }
            0x00 if dlab => { /* DLL - ignore */ }
            IER if !dlab => self.ier = value as u8,
            0x01 if dlab => { /* DLH - ignore */ }
            FCR => { /* FIFO control - ignore */ }
            LCR => self.lcr = value as u8,
            MCR => self.mcr = value as u8,
            SCR => self.scr = value as u8,
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsr_reports_tx_empty() {
        let uart = Uart8250::new(0x3f8);
        let lsr = uart.read(LSR, 1);
        assert_ne!(lsr & LSR_THRE as u64, 0);
        assert_ne!(lsr & LSR_TEMT as u64, 0);
    }

    #[test]
    fn write_thr_does_not_panic() {
        let mut uart = Uart8250::new(0x3f8);
        uart.write(THR, b'A' as u64, 1);
    }
}
