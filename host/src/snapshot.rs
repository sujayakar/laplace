use std::io::Write;
use std::path::Path;
use std::ptr;

use crate::hvf;
use hvf::{check_hv, HvSimdFpUchar16};

/// Captured CPU state for snapshot/restore.
pub struct CpuState {
    /// x0-x30, PC, FPCR, FPSR, CPSR (35 registers)
    pub gpr: [u64; 35],
    /// System registers (indexed same as SNAPSHOT_SYS_REGS)
    pub sys_regs: Vec<u64>,
    /// SIMD/FP registers Q0-Q31
    pub simd: [HvSimdFpUchar16; 32],
}

impl CpuState {
    /// Capture all CPU state from a vCPU.
    pub unsafe fn capture(vcpu: u64) -> Self {
        let mut gpr = [0u64; 35];
        for i in 0..35 {
            gpr[i] = hvf::vcpu_get_reg(vcpu, i as u32);
        }

        let mut sys_regs = Vec::with_capacity(hvf::SNAPSHOT_SYS_REGS.len());
        for &reg_id in hvf::SNAPSHOT_SYS_REGS {
            sys_regs.push(hvf::vcpu_get_sys_reg(vcpu, reg_id));
        }

        let mut simd = [[0u8; 16]; 32];
        for i in 0..32 {
            check_hv(
                hvf::hv_vcpu_get_simd_fp_reg(vcpu, i as u32, &mut simd[i]),
                "get simd reg",
            );
        }

        CpuState {
            gpr,
            sys_regs,
            simd,
        }
    }

    /// Restore all CPU state to a vCPU.
    pub unsafe fn restore(&self, vcpu: u64) {
        for i in 0..35 {
            check_hv(
                hvf::hv_vcpu_set_reg(vcpu, i as u32, self.gpr[i]),
                "set gpr",
            );
        }

        for (idx, &reg_id) in hvf::SNAPSHOT_SYS_REGS.iter().enumerate() {
            check_hv(
                hvf::hv_vcpu_set_sys_reg(vcpu, reg_id, self.sys_regs[idx]),
                "set sys reg",
            );
        }

        for i in 0..32 {
            check_hv(
                hvf::hv_vcpu_set_simd_fp_reg(vcpu, i as u32, &self.simd[i]),
                "set simd reg",
            );
        }
    }

    /// Serialize to bytes (simple format: raw gpr + sys_reg count + sys_regs + simd).
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        // GPRs: 35 * 8 = 280 bytes
        for &val in &self.gpr {
            buf.extend_from_slice(&val.to_le_bytes());
        }
        // Sys reg count + values
        let count = self.sys_regs.len() as u32;
        buf.extend_from_slice(&count.to_le_bytes());
        for &val in &self.sys_regs {
            buf.extend_from_slice(&val.to_le_bytes());
        }
        // SIMD: 32 * 16 = 512 bytes
        for reg in &self.simd {
            buf.extend_from_slice(reg);
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

        let mut simd = [[0u8; 16]; 32];
        for reg in &mut simd {
            reg.copy_from_slice(&data[off..off + 16]);
            off += 16;
        }

        CpuState {
            gpr,
            sys_regs,
            simd,
        }
    }
}

/// A saved VM template: CPU state + memory file path.
pub struct Template {
    pub cpu_state: CpuState,
    pub mem_path: std::path::PathBuf,
    pub mem_size: usize,
    pub guest_base: u64,
}

impl Template {
    /// Save a template to directory.
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

    /// Load a template from directory.
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

        // Memory file is always "guest.mem" in the template directory
        Template {
            cpu_state,
            mem_path: dir.join("guest.mem"),
            mem_size,
            guest_base,
        }
    }

    /// Fork a new VM from this template. Returns (host_mem_ptr, mem_size).
    /// The caller must create the VM and vCPU, then call cpu_state.restore().
    pub fn mmap_cow_memory(&self) -> *mut u8 {
        let fd = unsafe {
            let c_path = std::ffi::CString::new(self.mem_path.to_str().unwrap()).unwrap();
            libc::open(c_path.as_ptr(), libc::O_RDONLY)
        };
        assert!(fd >= 0, "open template memory file failed");

        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                self.mem_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE,
                fd,
                0,
            )
        };
        unsafe { libc::close(fd); }

        assert_ne!(ptr, libc::MAP_FAILED, "mmap MAP_PRIVATE failed");
        ptr as *mut u8
    }
}
