//! Fully virtualized ARM timer for deterministic execution.
//!
//! The ARM architected timer (CNTVCT_EL0) is the primary source of
//! non-determinism in a VM. We trap all timer register accesses and
//! maintain a virtual counter that advances by a fixed increment per read.
//!
//! On WFI (wait for interrupt), we warp time directly to the next
//! programmed timer deadline instead of spinning.

/// Fixed counter frequency advertised to the guest (matches Apple Silicon).
pub const COUNTER_FREQ_HZ: u64 = 24_000_000;

/// How many counter ticks to advance per CNTVCT read.
/// 24 ticks = 1 microsecond at 24 MHz.
const INCREMENT_PER_READ: u64 = 24;

pub struct VirtualTimer {
    /// Current virtual counter value.
    pub counter: u64,

    /// The guest's programmed timer comparator value (CNTV_CVAL_EL0).
    pub cval: u64,

    /// CNTV_CTL_EL0: bit 0 = ENABLE, bit 1 = IMASK
    pub ctl: u64,
}

impl VirtualTimer {
    pub fn new() -> Self {
        VirtualTimer {
            counter: 0,
            cval: u64::MAX,
            ctl: 0,
        }
    }

    /// Handle a trapped CNTVCT_EL0 or CNTPCT_EL0 read.
    /// Advances the counter by a fixed increment and returns the new value.
    pub fn read_counter(&mut self) -> u64 {
        self.counter += INCREMENT_PER_READ;
        self.counter
    }

    /// Handle a trapped CNTV_CVAL_EL0 write (absolute deadline).
    pub fn write_cval(&mut self, value: u64) {
        self.cval = value;
    }

    /// Handle a trapped CNTV_TVAL_EL0 write (relative deadline).
    /// TVAL sets CVAL = counter + tval.
    pub fn write_tval(&mut self, value: u64) {
        // TVAL is a signed 32-bit value sign-extended to 64 bits
        let tval = value as i32 as i64;
        self.cval = (self.counter as i64 + tval) as u64;
    }

    /// Handle a trapped CNTV_CTL_EL0 write.
    pub fn write_ctl(&mut self, value: u64) {
        self.ctl = value;
    }

    /// Read CNTV_CTL_EL0. Bit 2 (ISTATUS) is set when counter >= cval.
    pub fn read_ctl(&self) -> u64 {
        let mut val = self.ctl;
        let enabled = (val & 1) != 0;
        if enabled && self.counter >= self.cval {
            val |= 1 << 2; // ISTATUS
        }
        val
    }

    /// Read CNTV_CVAL_EL0.
    pub fn read_cval(&self) -> u64 {
        self.cval
    }

    /// Is the timer enabled and unmasked?
    pub fn timer_enabled_unmasked(&self) -> bool {
        let enabled = (self.ctl & 1) != 0;
        let masked = (self.ctl & 2) != 0;
        enabled && !masked
    }

    /// Is the timer expired (counter >= deadline)?
    pub fn timer_expired(&self) -> bool {
        self.timer_enabled_unmasked() && self.counter >= self.cval
    }

    /// Handle WFI: warp time to the next timer deadline.
    /// Returns true if a timer interrupt should be injected.
    pub fn handle_wfi(&mut self) -> bool {
        if !self.timer_enabled_unmasked() {
            // No timer active — advance a small amount and return.
            // This shouldn't happen in a well-behaved kernel, but prevents hangs.
            self.counter += COUNTER_FREQ_HZ / 100; // 10ms
            return false;
        }

        if self.counter >= self.cval {
            // Timer already expired, fire immediately
            true
        } else {
            // Warp to the deadline
            self.counter = self.cval;
            true
        }
    }

    /// Check if a timer interrupt should be pending before VM entry.
    /// Returns true if the timer has expired and should fire.
    pub fn check_pending(&self) -> bool {
        self.timer_expired()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_advances_by_fixed_increment() {
        let mut vt = VirtualTimer::new();
        assert_eq!(vt.counter, 0);
        let v1 = vt.read_counter();
        let v2 = vt.read_counter();
        assert_eq!(v1, INCREMENT_PER_READ);
        assert_eq!(v2, INCREMENT_PER_READ * 2);
    }

    #[test]
    fn timer_not_pending_when_disabled() {
        let mut vt = VirtualTimer::new();
        vt.write_cval(0); // deadline in the past
        vt.read_counter(); // advance past 0
        assert!(!vt.check_pending()); // ctl=0, timer disabled
    }

    #[test]
    fn timer_pending_when_enabled_and_expired() {
        let mut vt = VirtualTimer::new();
        vt.write_ctl(1); // enable, unmask
        vt.write_cval(10); // deadline at 10
        // Advance counter past deadline
        for _ in 0..10 {
            vt.read_counter();
        }
        assert!(vt.counter >= 10);
        assert!(vt.check_pending());
    }

    #[test]
    fn timer_not_pending_when_masked() {
        let mut vt = VirtualTimer::new();
        vt.write_ctl(3); // enable + mask (IMASK=1)
        vt.write_cval(0); // expired
        vt.read_counter();
        assert!(!vt.check_pending());
    }

    #[test]
    fn ctl_istatus_reflects_expiry() {
        let mut vt = VirtualTimer::new();
        vt.write_ctl(1); // enable
        vt.write_cval(100);
        // Not expired yet
        assert_eq!(vt.read_ctl() & (1 << 2), 0);
        // Advance past deadline
        vt.counter = 200;
        assert_ne!(vt.read_ctl() & (1 << 2), 0);
    }

    #[test]
    fn write_tval_sets_cval_relative() {
        let mut vt = VirtualTimer::new();
        vt.counter = 1000;
        vt.write_tval(500); // tval=500 -> cval=1500
        assert_eq!(vt.cval, 1500);
    }

    #[test]
    fn write_tval_handles_negative() {
        let mut vt = VirtualTimer::new();
        vt.counter = 1000;
        // -100 as u64 (sign-extended from i32)
        vt.write_tval((-100i32) as u64);
        assert_eq!(vt.cval, 900);
    }

    #[test]
    fn wfi_warps_to_deadline() {
        let mut vt = VirtualTimer::new();
        vt.write_ctl(1); // enable
        vt.write_cval(1_000_000);
        assert_eq!(vt.counter, 0);
        let inject = vt.handle_wfi();
        assert!(inject);
        assert_eq!(vt.counter, 1_000_000);
    }

    #[test]
    fn wfi_without_timer_advances_10ms() {
        let mut vt = VirtualTimer::new();
        // Timer disabled (ctl=0)
        let inject = vt.handle_wfi();
        assert!(!inject);
        assert_eq!(vt.counter, COUNTER_FREQ_HZ / 100);
    }

    #[test]
    fn wfi_with_expired_timer_fires_immediately() {
        let mut vt = VirtualTimer::new();
        vt.write_ctl(1);
        vt.write_cval(10);
        vt.counter = 100; // already past deadline
        let inject = vt.handle_wfi();
        assert!(inject);
        assert_eq!(vt.counter, 100); // counter unchanged
    }
}
