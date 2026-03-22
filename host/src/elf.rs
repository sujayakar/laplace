use goblin::elf::Elf;
use goblin::elf::program_header::PT_LOAD;

pub struct LoadSegment<'a> {
    pub vaddr: u64,
    pub data: &'a [u8],
    pub flags: u32,
}

impl LoadSegment<'_> {
    pub fn flags_str(&self) -> String {
        let r = if self.flags & 4 != 0 { "R" } else { "-" };
        let w = if self.flags & 2 != 0 { "W" } else { "-" };
        let x = if self.flags & 1 != 0 { "X" } else { "-" };
        format!("{}{}{}", r, w, x)
    }
}

pub struct GuestElf<'a> {
    pub entry: u64,
    pub loads: Vec<LoadSegment<'a>>,
}

impl<'a> GuestElf<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, String> {
        let elf = Elf::parse(data).map_err(|e| format!("ELF parse error: {}", e))?;

        if elf.header.e_machine != goblin::elf::header::EM_AARCH64 {
            return Err(format!(
                "Not an aarch64 ELF (e_machine={})",
                elf.header.e_machine
            ));
        }

        let mut loads = Vec::new();
        for ph in &elf.program_headers {
            if ph.p_type != PT_LOAD {
                continue;
            }

            let file_offset = ph.p_offset as usize;
            let file_size = ph.p_filesz as usize;
            if file_offset + file_size > data.len() {
                return Err(format!(
                    "LOAD segment at 0x{:x}: file data extends past end of file",
                    ph.p_vaddr
                ));
            }

            loads.push(LoadSegment {
                vaddr: ph.p_vaddr,
                data: &data[file_offset..file_offset + file_size],
                flags: ph.p_flags,
            });
        }

        if loads.is_empty() {
            return Err("No PT_LOAD segments found".into());
        }

        Ok(GuestElf {
            entry: elf.entry,
            loads,
        })
    }
}
