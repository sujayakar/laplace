//! Device tree blob (DTB) generation for booting Linux in the VM.
//!
//! Builds a minimal DTB describing: 1 CPU, memory, GICv3, PL011 UART,
//! timer, PSCI, and chosen node with boot parameters.

use vm_fdt::FdtWriter;

use crate::vtimer::COUNTER_FREQ_HZ;

/// GIC addresses — must match what we pass to hv_gic_config.
pub const GICD_BASE: u64 = 0x0800_0000;
pub const GICD_SIZE: u64 = 0x0001_0000;
pub const GICR_BASE: u64 = 0x080A_0000;
pub const GICR_SIZE: u64 = 0x0020_0000;

/// PL011 UART address — matches QEMU virt convention.
pub const UART_BASE: u64 = 0x0900_0000;
pub const UART_SIZE: u64 = 0x0000_1000;
/// UART interrupt: SPI 1 (intid 33 = 32 + 1)
pub const UART_SPI: u32 = 1;

/// Build a minimal device tree for our Linux VM.
///
/// `mem_base` and `mem_size`: guest RAM region.
/// `initrd_start`, `initrd_end`: optional initramfs location in guest memory.
pub fn build_dtb(
    mem_base: u64,
    mem_size: u64,
    initrd_start: Option<u64>,
    initrd_end: Option<u64>,
) -> Vec<u8> {
    let mut fdt = FdtWriter::new().expect("FdtWriter::new");

    // Root node
    let root = fdt.begin_node("").expect("begin root");
    fdt.property_string("compatible", "linux,dummy-virt")
        .expect("compatible");
    fdt.property_u32("#address-cells", 2).expect("#address-cells");
    fdt.property_u32("#size-cells", 2).expect("#size-cells");

    // CPU node
    {
        let cpus = fdt.begin_node("cpus").expect("begin cpus");
        fdt.property_u32("#address-cells", 1).expect("cpu #addr");
        fdt.property_u32("#size-cells", 0).expect("cpu #size");

        let cpu0 = fdt.begin_node("cpu@0").expect("begin cpu@0");
        fdt.property_string("device_type", "cpu").expect("cpu device_type");
        fdt.property_string("compatible", "arm,arm-v8")
            .expect("cpu compatible");
        fdt.property_u32("reg", 0).expect("cpu reg");
        fdt.property_string("enable-method", "psci")
            .expect("enable-method");
        fdt.end_node(cpu0).expect("end cpu@0");
        fdt.end_node(cpus).expect("end cpus");
    }

    // Memory node
    {
        let mem = fdt.begin_node(&format!("memory@{:x}", mem_base)).expect("begin memory");
        fdt.property_string("device_type", "memory").expect("mem device_type");
        fdt.property_array_u64("reg", &[mem_base, mem_size])
            .expect("mem reg");
        fdt.end_node(mem).expect("end memory");
    }

    // PSCI node
    {
        let psci = fdt.begin_node("psci").expect("begin psci");
        fdt.property_string("compatible", "arm,psci-1.0")
            .expect("psci compatible");
        fdt.property_string("method", "hvc").expect("psci method");
        fdt.end_node(psci).expect("end psci");
    }

    // Timer node — ARM architected timer
    // Interrupt cells: [type, number, flags]
    // type: 1 = PPI
    // flags: 0x304 = level-sensitive, active-low (standard for timer PPIs)
    {
        let timer = fdt.begin_node("timer").expect("begin timer");
        fdt.property_string("compatible", "arm,armv8-timer")
            .expect("timer compatible");
        // Secure EL1 phys (PPI 13), Non-secure EL1 phys (PPI 14),
        // Virtual (PPI 11), EL2 phys (PPI 10)
        fdt.property_array_u32(
            "interrupts",
            &[
                1, 13, 0x304, // secure phys timer
                1, 14, 0x304, // non-secure phys timer
                1, 11, 0x304, // virtual timer
                1, 10, 0x304, // hyp timer
            ],
        )
        .expect("timer interrupts");
        fdt.property_null("always-on").expect("always-on");
        fdt.property_u32("clock-frequency", COUNTER_FREQ_HZ as u32)
            .expect("clock-frequency");
        fdt.property_u32("interrupt-parent", 1)
            .expect("timer interrupt-parent");
        fdt.end_node(timer).expect("end timer");
    }

    // GICv3 node
    {
        let gic = fdt
            .begin_node(&format!("intc@{:x}", GICD_BASE))
            .expect("begin intc");
        fdt.property_string("compatible", "arm,gic-v3")
            .expect("gic compatible");
        fdt.property_null("interrupt-controller")
            .expect("interrupt-controller");
        fdt.property_u32("#interrupt-cells", 3)
            .expect("#interrupt-cells");
        fdt.property_u32("phandle", 1).expect("gic phandle");
        fdt.property_array_u64(
            "reg",
            &[
                GICD_BASE, GICD_SIZE, // Distributor
                GICR_BASE, GICR_SIZE, // Redistributor
            ],
        )
        .expect("gic reg");
        fdt.end_node(gic).expect("end intc");
    }

    // PL011 UART node
    {
        let uart = fdt
            .begin_node(&format!("pl011@{:x}", UART_BASE))
            .expect("begin pl011");
        fdt.property_string_list("compatible", vec!["arm,pl011".into(), "arm,primecell".into()])
            .expect("uart compatible");
        fdt.property_array_u64("reg", &[UART_BASE, UART_SIZE])
            .expect("uart reg");
        // SPI type=0, number=UART_SPI, flags=edge-triggered
        fdt.property_array_u32("interrupts", &[0, UART_SPI, 4])
            .expect("uart interrupts");
        fdt.property_u32("interrupt-parent", 1)
            .expect("uart interrupt-parent");
        fdt.property_string_list("clock-names", vec!["uartclk".into(), "apb_pclk".into()])
            .expect("uart clock-names");
        // Dummy clock phandles — Linux PL011 driver needs these but we don't
        // have a real clock provider. Use phandle 0xFFFF as a placeholder.
        // Actually, let's just not include clocks and rely on earlycon
        // which bypasses the full driver.
        fdt.end_node(uart).expect("end pl011");
    }

    // Chosen node — boot args + stdout
    {
        let chosen = fdt.begin_node("chosen").expect("begin chosen");
        fdt.property_string(
            "bootargs",
            "earlycon=pl011,mmio32,0x09000000 \
             nokaslr \
             norandmaps \
             random.trust_cpu=on \
             nosmp \
             clocksource=arch_sys_counter \
             nohz=off \
             console=ttyAMA0 \
             lpj=50000",
        )
        .expect("bootargs");
        fdt.property_string("stdout-path", &format!("/pl011@{:x}", UART_BASE))
            .expect("stdout-path");
        if let (Some(start), Some(end)) = (initrd_start, initrd_end) {
            fdt.property_u64("linux,initrd-start", start)
                .expect("initrd-start");
            fdt.property_u64("linux,initrd-end", end)
                .expect("initrd-end");
        }
        fdt.end_node(chosen).expect("end chosen");
    }

    fdt.end_node(root).expect("end root");
    fdt.finish().expect("finish FDT")
}
