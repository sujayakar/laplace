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
