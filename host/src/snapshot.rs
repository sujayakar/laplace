use std::io::Write;
use std::path::Path;
use std::ptr;

use crate::hypervisor::{SimdReg, VcpuHandle, SNAPSHOT_SYS_REGS};

/// Captured CPU state for snapshot/restore.
pub struct CpuState {
    /// x0-x30, PC, FPCR, FPSR, CPSR (35 registers)
    pub gpr: [u64; 35],
    /// System registers (indexed same as SNAPSHOT_SYS_REGS)
    pub sys_regs: Vec<u64>,
    /// SIMD/FP registers Q0-Q31
    pub simd: [SimdReg; 32],
}

impl CpuState {
    /// Capture all CPU state from a vCPU.
    pub fn capture(vcpu: &VcpuHandle) -> Self {
        let mut gpr = [0u64; 35];
        for i in 0..35 {
            gpr[i] = vcpu.get_reg(i as u32);
        }

        let mut sys_regs = Vec::with_capacity(SNAPSHOT_SYS_REGS.len());
        for &reg in SNAPSHOT_SYS_REGS {
            sys_regs.push(vcpu.get_sys_reg(reg));
        }

        let mut simd = [SimdReg::default(); 32];
        for i in 0..32 {
            simd[i] = vcpu.get_simd_reg(i as u32);
        }

        CpuState { gpr, sys_regs, simd }
    }

    /// Restore all CPU state to a vCPU.
    pub fn restore(&self, vcpu: &VcpuHandle) {
        for i in 0..35 {
            vcpu.set_reg(i as u32, self.gpr[i]);
        }

        for (idx, &reg) in SNAPSHOT_SYS_REGS.iter().enumerate() {
            vcpu.set_sys_reg(reg, self.sys_regs[idx]);
        }

        for i in 0..32 {
            vcpu.set_simd_reg(i as u32, &self.simd[i]);
        }
    }

    /// Serialize to bytes (simple format: raw gpr + sys_reg count + sys_regs + simd).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        for &val in &self.gpr {
            buf.extend_from_slice(&val.to_le_bytes());
        }
        let count = self.sys_regs.len() as u32;
        buf.extend_from_slice(&count.to_le_bytes());
        for &val in &self.sys_regs {
            buf.extend_from_slice(&val.to_le_bytes());
        }
        for reg in &self.simd {
            buf.extend_from_slice(&reg.0);
        }
        buf
    }

    /// Deserialize from bytes.
    pub fn from_bytes(data: &[u8]) -> Self {
        let mut off = 0;

        let mut gpr = [0u64; 35];
        for val in &mut gpr {
            *val = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
            off += 8;
        }

        let count = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        off += 4;

        let mut sys_regs = Vec::with_capacity(count);
        for _ in 0..count {
            sys_regs.push(u64::from_le_bytes(data[off..off + 8].try_into().unwrap()));
            off += 8;
        }

        let mut simd = [SimdReg::default(); 32];
        for reg in &mut simd {
            reg.0.copy_from_slice(&data[off..off + 16]);
            off += 16;
        }

        CpuState { gpr, sys_regs, simd }
    }
}

/// A saved VM template: CPU state + memory file path.
pub struct Template {
    pub cpu_state: CpuState,
    pub mem_path: std::path::PathBuf,
    pub mem_size: usize,
    pub guest_base: u64,
    /// Cached memfd for CoW mmap. Created on first use, reused across forks.
    /// Using memfd avoids re-reading the file from disk on each fork.
    memfd: std::cell::Cell<i32>,
}

impl Template {
    pub fn new(cpu_state: CpuState, mem_path: std::path::PathBuf, mem_size: usize, guest_base: u64) -> Self {
        Template { cpu_state, mem_path, mem_size, guest_base, memfd: std::cell::Cell::new(-1) }
    }

    pub fn save(&self, dir: &Path) {
        std::fs::create_dir_all(dir).expect("create template dir");

        let state_path = dir.join("cpu.state");
        let mut f = std::fs::File::create(&state_path).expect("create cpu.state");
        f.write_all(&self.cpu_state.to_bytes()).expect("write cpu.state");

        let meta = format!(
            "mem_size={}\nguest_base=0x{:x}\n",
            self.mem_size,
            self.guest_base,
        );
        std::fs::write(dir.join("meta.txt"), meta).expect("write meta");
    }

    pub fn load(dir: &Path) -> Self {
        let state_data = std::fs::read(dir.join("cpu.state")).expect("read cpu.state");
        let cpu_state = CpuState::from_bytes(&state_data);

        let meta = std::fs::read_to_string(dir.join("meta.txt")).expect("read meta");
        let mut mem_size = 0usize;
        let mut guest_base = 0u64;
        for line in meta.lines() {
            if let Some(v) = line.strip_prefix("mem_size=") {
                mem_size = v.parse().unwrap();
            } else if let Some(v) = line.strip_prefix("guest_base=") {
                guest_base = u64::from_str_radix(v.trim_start_matches("0x"), 16).unwrap();
            }
        }

        Template {
            cpu_state,
            mem_path: dir.join("guest.mem"),
            mem_size,
            guest_base,
            memfd: std::cell::Cell::new(-1),
        }
    }

    /// Get or create a memfd backed by the snapshot memory.
    /// The memfd is created once and reused across forks (serve mode).
    fn get_or_create_memfd(&self) -> i32 {
        let fd = self.memfd.get();
        if fd >= 0 {
            return fd;
        }

        // Create memfd and copy snapshot memory into it
        let name = std::ffi::CString::new("laplace-snapshot").unwrap();
        let memfd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        assert!(memfd >= 0, "memfd_create failed: {}", std::io::Error::last_os_error());

        // Size the memfd
        assert_eq!(
            unsafe { libc::ftruncate(memfd, self.mem_size as i64) },
            0,
            "ftruncate memfd failed"
        );

        // Map memfd as shared, copy snapshot data in, then unmap
        let dst = unsafe {
            libc::mmap(
                ptr::null_mut(),
                self.mem_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                memfd,
                0,
            )
        };
        assert_ne!(dst, libc::MAP_FAILED, "mmap memfd for copy failed");

        // Read snapshot file into memfd
        let data = std::fs::read(&self.mem_path).expect("read guest.mem");
        assert_eq!(data.len(), self.mem_size, "guest.mem size mismatch");
        unsafe {
            ptr::copy_nonoverlapping(data.as_ptr(), dst as *mut u8, self.mem_size);
            libc::munmap(dst, self.mem_size);
        }

        self.memfd.set(memfd);
        memfd
    }

    /// Create a CoW memory mapping from the snapshot.
    /// Uses memfd (in-memory, no disk I/O after first load) with MAP_NORESERVE
    /// (don't reserve swap space for the CoW region — pages are allocated on demand).
    pub fn mmap_cow_memory(&self) -> *mut u8 {
        let fd = self.get_or_create_memfd();

        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                self.mem_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_NORESERVE,
                fd,
                0,
            )
        };
        assert_ne!(ptr, libc::MAP_FAILED, "mmap MAP_PRIVATE failed");
        ptr as *mut u8
    }
}

impl Drop for Template {
    fn drop(&mut self) {
        let fd = self.memfd.get();
        if fd >= 0 {
            unsafe { libc::close(fd); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cpu_state_roundtrip() {
        let state = CpuState {
            gpr: {
                let mut g = [0u64; 35];
                for i in 0..35 {
                    g[i] = (i as u64) * 0x1111;
                }
                g
            },
            sys_regs: {
                let len = SNAPSHOT_SYS_REGS.len();
                (0..len).map(|i| (i as u64) * 0x1111 + 0xAAAA).collect()
            },
            simd: {
                let mut s = [SimdReg::default(); 32];
                s[0] = SimdReg([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]);
                s
            },
        };

        let bytes = state.to_bytes();
        let restored = CpuState::from_bytes(&bytes);

        assert_eq!(state.gpr, restored.gpr);
        assert_eq!(state.sys_regs, restored.sys_regs);
        assert_eq!(state.simd, restored.simd);
    }
}
