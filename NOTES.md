# Development notes

## Review process

- Read through all the code carefully, using GPT-5.4 xhigh in the background as a second opinion for review.
- Write any missing tests, iterate until fixed
- See if there are any opportunities to use high quality 3rd party crates rather than rolling from scratch (e.g. file format parsers)
- Go over code organization / duplication
- Use GPT-5.4 xhigh (`codex exec -m gpt-5.4 -c model_reasoning_effort="xhigh"`) for second opinions on tricky issues

## Known limitations (macOS HVF)

- Per-process single VM (`hv_vm_create` returns `HV_BUSY` on second call)
- No EL2 access on M1 (timer trapping via CNTHCTL_EL2 not available — uses real-time timer)
- M4 may expose EL2 timer trapping
- CoW mmap of 512 MiB takes ~11ms — dominates fork latency
- SIMD FFI alignment: `HvSimdFpUchar16` must be `#[repr(C, align(16))]`, not a type alias

## Bugs found and fixed (review log)

- **SIMD alignment**: HvSimdFpUchar16 was `[u8; 16]` (align=1) but SDK type has align=16
- **SRT==31 MMIO**: Register 31 in data-abort syndrome means XZR, not PC
- **MMIO sign extension**: LDRSB/LDRSH/LDRSW loads need sign extension, not zero extension
- **dup3 old==new**: Returns EINVAL; use fcntl(F_SETFD, 0) to clear O_CLOEXEC instead
- **kmsg fd leak**: /dev/kmsg opened without O_CLOEXEC leaked to runner via execve
- **READY spin**: Comparing 1 byte could match JS starting with 'C'; now compares 8 bytes
- **write_all short writes**: Pipe writes can be partial; loop until complete
- **console_log panic**: V8 toString() can throw; use match not unwrap
- **Boa stale mmap**: Runner-js was reading mailbox via /dev/mem; switched to stdin pipe
- **Boa ready signal timing**: Signaled before Context creation; moved after
- **Watchdog join latency**: 100ms sleep meant 100ms join; added fast variant (1ms)
- **V8 snapshot timing**: Init wrote READY before V8 finished; added ready pipe protocol
- **TVAL sign extension**: `as i32 as u64` zero-extends; need `as i32 as i64 as u64`
- **Initrd placement**: Kernel image_size > file size; BSS overwrote initrd
- **Spectre HVC**: Kernel patched exception return with HVC; return SMCCC_RET_NOT_REQUIRED
- **SP_EL1 KVM encoding**: SP_EL1 is a core register on KVM (kvm_regs.sp_el1), not a sysreg
- **SP_EL0 KVM encoding**: SP_EL0 is user_pt_regs.sp (core register), not a sysreg
- **KVM vCPU init order**: Must call KVM_ARM_VCPU_INIT before any register get/set
- **KVM GIC init order**: vCPU must be created BEFORE GIC CTRL_INIT
- **KVM MMIO read-back**: Must write response to kvm_run.mmio.data before next KVM_RUN
- **PL011 ID registers**: Linux driver checks PeriphID/CellID at 0xFE0-0xFFC; must return correct values
- **Firecracker kernel lacks PL011**: Use Cloud Hypervisor kernel for PL011 consistency across platforms
- **KVM HVC not forwarded**: Non-PSCI HVCs handled in-kernel, not forwarded to userspace; use BRK instead
- **BRK HSR extraction**: KVM_EXIT_DEBUG provides ESR in hsr field; bits 15:0 = BRK imm16. No guest memory read needed.
- **MMIO patching doesn't work**: Guest LDR/STR uses VA not GPA; would need kernel linear map VA
- **VA→GPA at snapshot fails**: Snapshot PC may be in userspace (init), not kernel text
- **HVF double PC advance**: hvf.rs run() advances PC for MMIO, linux_boot.rs must NOT advance again
- **SPSR_EL1/ELR_EL1 on KVM**: Core registers (kvm_regs struct), not sysregs; same as SP_EL0/SP_EL1
