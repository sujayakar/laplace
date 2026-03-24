//! Cross-platform hypervisor abstraction.
//!
//! Selects the appropriate backend at compile time:
//! - macOS: Hypervisor.framework (HVF)
//! - Linux: KVM

pub mod types;

#[cfg(target_os = "macos")]
#[allow(dead_code)]
pub(crate) mod hvf;

#[cfg(target_os = "linux")]
#[allow(dead_code)]
mod kvm;

// Re-export the selected backend's types
#[cfg(target_os = "macos")]
pub use hvf::{GicHandle, VcpuHandle, VmHandle};

#[cfg(target_os = "linux")]
pub use kvm::{GicHandle, VcpuHandle, VmHandle};

// Always re-export shared types
pub use types::*;
