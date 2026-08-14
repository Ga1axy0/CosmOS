//! Early boot information discovered from firmware device trees.

use core::cmp::{max, min};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::config::MAX_HARTS;

const MAX_MEMORY_REGIONS: usize = 8;
const MAX_RESERVED_REGIONS: usize = 16;
const MAX_MMIO_REGIONS: usize = 24;
const MAX_VIRTIO_MMIO_DEVICES: usize = 16;
const MAX_CLOCK_RESOURCES: usize = 16;
const PCI_INTX_ENTRIES: usize = 32 * 4;
const MAX_FDT_SIZE: usize = 16 * 1024 * 1024;
const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// One physical memory byte range.
#[derive(Clone, Copy, Debug, Default)]
pub struct PhysMemoryRegion {
    /// Inclusive physical start address.
    pub start: usize,
    /// Exclusive physical end address.
    pub end: usize,
}

/// One MMIO device resource described by firmware.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceResource {
    /// Physical register base.
    pub start: usize,
    /// Register window size.
    pub size: usize,
    /// First interrupt specifier cell, when present.
    pub irq: Option<u32>,
}

/// One firmware-described DesignWare GMAC controller.
#[derive(Clone, Copy, Debug)]
pub struct GmacResource {
    device: DeviceResource,
    mac_address: Option<[u8; 6]>,
}

impl GmacResource {
    /// Return the controller register and interrupt resource.
    pub fn device(self) -> DeviceResource {
        self.device
    }

    /// Return the firmware-provided station address, when valid.
    pub fn mac_address(self) -> Option<[u8; 6]> {
        self.mac_address
    }
}

impl DeviceResource {
    const fn empty() -> Self {
        Self {
            start: 0,
            size: 0,
            irq: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ClockResource {
    phandle: u32,
    parent_phandle: u32,
    frequency: usize,
}

/// One PCI ECAM host bridge and its firmware-assigned MMIO aperture.
#[derive(Clone, Copy, Debug)]
pub struct PciHostResource {
    /// ECAM register window.
    pub ecam: DeviceResource,
    /// Inclusive first PCI bus number.
    pub bus_start: u8,
    /// Inclusive last PCI bus number.
    pub bus_end: u8,
    /// CPU physical base of the non-prefetchable PCI memory aperture.
    pub memory_start: usize,
    /// Size of the PCI memory aperture.
    pub memory_size: usize,
    intx_irqs: [u32; PCI_INTX_ENTRIES],
}

impl PciHostResource {
    const fn empty() -> Self {
        Self {
            ecam: DeviceResource::empty(),
            bus_start: 0,
            bus_end: 0,
            memory_start: 0,
            memory_size: 0,
            intx_irqs: [0; PCI_INTX_ENTRIES],
        }
    }

    /// Resolve a PCI slot and one-based INTx pin through `interrupt-map`.
    pub fn intx_irq(&self, slot: u8, pin: u8) -> Option<u32> {
        if pin == 0 || pin > 4 || slot >= 32 {
            return None;
        }
        let irq = self.intx_irqs[slot as usize * 4 + pin as usize - 1];
        (irq != 0).then_some(irq)
    }
}

impl PhysMemoryRegion {
    const fn empty() -> Self {
        Self { start: 0, end: 0 }
    }

    const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

/// Fixed-capacity boot information available before the heap is initialized.
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    memory_regions: [PhysMemoryRegion; MAX_MEMORY_REGIONS],
    memory_region_count: usize,
    reserved_regions: [PhysMemoryRegion; MAX_RESERVED_REGIONS],
    reserved_region_count: usize,
    hart_count: usize,
    timer_frequency: usize,
    uart: Option<DeviceResource>,
    rtc: Option<DeviceResource>,
    plic: Option<DeviceResource>,
    pch_pic: Option<DeviceResource>,
    eiointc: Option<DeviceResource>,
    pci_host: Option<PciHostResource>,
    ahci: Option<DeviceResource>,
    gmac: Option<GmacResource>,
    virtio_mmio: [DeviceResource; MAX_VIRTIO_MMIO_DEVICES],
    virtio_mmio_count: usize,
    mmio_regions: [PhysMemoryRegion; MAX_MMIO_REGIONS],
    mmio_region_count: usize,
    clocks: [ClockResource; MAX_CLOCK_RESOURCES],
    clock_count: usize,
    uart_clock_phandle: u32,
    fdt_ptr: usize,
    fdt_size: usize,
}

impl BootInfo {
    const fn empty() -> Self {
        Self {
            memory_regions: [PhysMemoryRegion::empty(); MAX_MEMORY_REGIONS],
            memory_region_count: 0,
            reserved_regions: [PhysMemoryRegion::empty(); MAX_RESERVED_REGIONS],
            reserved_region_count: 0,
            hart_count: 0,
            timer_frequency: 0,
            uart: None,
            rtc: None,
            plic: None,
            pch_pic: None,
            eiointc: None,
            pci_host: None,
            ahci: None,
            gmac: None,
            virtio_mmio: [DeviceResource::empty(); MAX_VIRTIO_MMIO_DEVICES],
            virtio_mmio_count: 0,
            mmio_regions: [PhysMemoryRegion::empty(); MAX_MMIO_REGIONS],
            mmio_region_count: 0,
            clocks: [ClockResource {
                phandle: 0,
                parent_phandle: 0,
                frequency: 0,
            }; MAX_CLOCK_RESOURCES],
            clock_count: 0,
            uart_clock_phandle: 0,
            fdt_ptr: 0,
            fdt_size: 0,
        }
    }

    fn push_memory_region(&mut self, start: usize, end: usize) {
        let start = firmware_address_to_phys(start);
        let end = firmware_address_to_phys(end);
        if start >= end || self.memory_region_count >= MAX_MEMORY_REGIONS {
            return;
        }
        self.memory_regions[self.memory_region_count] = PhysMemoryRegion::new(start, end);
        self.memory_region_count += 1;
    }

    fn push_reserved_region(&mut self, start: usize, end: usize) {
        let start = firmware_address_to_phys(start);
        let end = firmware_address_to_phys(end);
        if start >= end || self.reserved_region_count >= MAX_RESERVED_REGIONS {
            return;
        }
        self.reserved_regions[self.reserved_region_count] = PhysMemoryRegion::new(start, end);
        self.reserved_region_count += 1;
    }

    fn set_hart_count(&mut self, hart_count: usize) {
        self.hart_count = hart_count.clamp(1, MAX_HARTS);
    }

    fn push_mmio_region(&mut self, resource: DeviceResource) {
        if resource.size == 0 || self.mmio_region_count >= MAX_MMIO_REGIONS {
            return;
        }
        let end = resource.start.saturating_add(resource.size);
        if resource.start >= end {
            return;
        }
        self.mmio_regions[self.mmio_region_count] = PhysMemoryRegion::new(resource.start, end);
        self.mmio_region_count += 1;
    }

    fn push_virtio_mmio(&mut self, resource: DeviceResource) {
        if self.virtio_mmio_count >= MAX_VIRTIO_MMIO_DEVICES {
            return;
        }
        // DT node order is not an address-order guarantee (QEMU commonly
        // emits these nodes in reverse). Preserve the previous bus scan's
        // stable device naming by sorting transports by physical address.
        let mut index = self.virtio_mmio_count;
        while index != 0 && self.virtio_mmio[index - 1].start > resource.start {
            self.virtio_mmio[index] = self.virtio_mmio[index - 1];
            index -= 1;
        }
        self.virtio_mmio[index] = resource;
        self.virtio_mmio_count += 1;
        self.push_mmio_region(resource);
    }

    fn push_clock(&mut self, phandle: u32, parent_phandle: u32, frequency: usize) {
        if phandle == 0
            || (parent_phandle == 0 && frequency == 0)
            || self.clock_count >= MAX_CLOCK_RESOURCES
        {
            return;
        }
        self.clocks[self.clock_count] = ClockResource {
            phandle,
            parent_phandle,
            frequency,
        };
        self.clock_count += 1;
    }

    fn resolve_timer_frequency(&mut self) {
        if self.timer_frequency != 0 || self.uart_clock_phandle == 0 {
            return;
        }
        let mut phandle = self.uart_clock_phandle;
        // Clock providers may themselves consume a parent clock. Follow the
        // finite phandle chain until a provider supplies clock-frequency.
        for _ in 0..self.clock_count {
            let Some(clock) = self.clocks[..self.clock_count]
                .iter()
                .find(|clock| clock.phandle == phandle)
            else {
                break;
            };
            if clock.frequency != 0 {
                self.timer_frequency = clock.frequency;
                break;
            }
            if clock.parent_phandle == 0 || clock.parent_phandle == phandle {
                break;
            }
            phandle = clock.parent_phandle;
        }
    }

    /// Return the firmware RAM ranges.
    pub fn memory_regions(&self) -> &[PhysMemoryRegion] {
        &self.memory_regions[..self.memory_region_count]
    }

    /// Return physical regions reserved by firmware or by the boot protocol.
    pub fn reserved_regions(&self) -> &[PhysMemoryRegion] {
        &self.reserved_regions[..self.reserved_region_count]
    }

    /// Return the discovered hart count.
    pub fn hart_count(&self) -> usize {
        self.hart_count
    }

    /// Return the firmware timer frequency in ticks per second.
    pub fn timer_frequency(&self) -> usize {
        self.timer_frequency
    }

    /// Return the selected NS16550-compatible console resource.
    pub fn uart(&self) -> Option<DeviceResource> {
        self.uart
    }

    /// Return the selected RTC resource.
    pub fn rtc(&self) -> Option<DeviceResource> {
        self.rtc
    }

    /// Return the RISC-V PLIC resource.
    pub fn plic(&self) -> Option<DeviceResource> {
        self.plic
    }

    /// Return the Loongson PCH PIC resource.
    pub fn pch_pic(&self) -> Option<DeviceResource> {
        self.pch_pic
    }

    /// Return the Loongson EIOINTC IOCSR resource.
    pub fn eiointc(&self) -> Option<DeviceResource> {
        self.eiointc
    }

    /// Return the PCI host bridge description.
    pub fn pci_host(&self) -> Option<PciHostResource> {
        self.pci_host
    }

    /// Return the firmware-described AHCI controller resource.
    pub fn ahci(&self) -> Option<DeviceResource> {
        self.ahci
    }

    /// Return the first enabled DesignWare GMAC controller.
    pub fn gmac(&self) -> Option<GmacResource> {
        self.gmac
    }

    /// Return all enabled VirtIO-MMIO transports.
    pub fn virtio_mmio_devices(&self) -> &[DeviceResource] {
        &self.virtio_mmio[..self.virtio_mmio_count]
    }

    /// Return register windows which must be mapped before driver startup.
    pub fn mmio_regions(&self) -> &[PhysMemoryRegion] {
        &self.mmio_regions[..self.mmio_region_count]
    }

    /// Return the FDT virtual address and size used for discovery, when known.
    pub fn fdt_blob(&self) -> Option<(usize, usize)> {
        (self.fdt_ptr != 0 && self.fdt_size != 0).then_some((self.fdt_ptr, self.fdt_size))
    }
}

static READY: AtomicBool = AtomicBool::new(false);

/// The global boot information instance, initialized by the bootstrap hart and read by secondary harts.
pub static mut BOOT_INFO: BootInfo = BootInfo::empty();

/// Initialize global boot information from an optional FDT pointer.
pub fn init(fdt_ptr: usize) {
    let mut info = BootInfo::empty();
    let direct_source = FdtSource::from_ptr(crate::platform::boot_fdt_ptr(fdt_ptr));
    let found_direct_fdt = direct_source.is_some_and(|source| load_fdt(source, &mut info));

    #[cfg(target_arch = "loongarch64")]
    let found_fdt = found_direct_fdt
        || uboot_bootelf_fdt_source().is_some_and(|source| load_fdt(source, &mut info))
        || loongarch_efi_fdt_source().is_some_and(|source| load_fdt(source, &mut info));

    #[cfg(not(target_arch = "loongarch64"))]
    let found_fdt = found_direct_fdt;

    if !found_fdt {
        crate::platform::early_console_write("[bootinfo] invalid firmware FDT\r\n");
        panic!("no valid firmware FDT was supplied");
    }
    if info.memory_region_count == 0 {
        crate::platform::early_console_write("[bootinfo] FDT has no enabled memory region\r\n");
        panic!("firmware FDT contains no enabled memory region");
    }
    if info.hart_count == 0 {
        crate::platform::early_console_write("[bootinfo] FDT has no enabled CPU node\r\n");
        panic!("firmware FDT contains no enabled CPU node");
    }
    if info.timer_frequency == 0 {
        crate::platform::early_console_write("[bootinfo] FDT has no timer frequency\r\n");
        panic!("firmware FDT contains no usable timer frequency");
    }
    if info.uart.is_none() {
        crate::platform::early_console_write("[bootinfo] FDT has no NS16550 UART\r\n");
        panic!("firmware FDT contains no enabled NS16550 UART");
    }

    unsafe {
        ptr::write(ptr::addr_of_mut!(BOOT_INFO), info);
    }
    READY.store(true, Ordering::Release);
}

/// QEMU's firmware-less LoongArch boot ABI passes a compact EFI system table
/// in `a2`. Locate the Device Tree configuration table by GUID so neither the
/// FDT address nor QEMU's placement policy is compiled into the kernel.
#[cfg(target_arch = "loongarch64")]
fn loongarch_efi_fdt_source() -> Option<FdtSource> {
    const EFI_SYSTEM_TABLE_SIGNATURE: u64 = 0x5453_5953_2049_4249;
    const EFI_NR_TABLES_OFFSET: usize = 104;
    const EFI_TABLES_OFFSET: usize = 112;
    const EFI_CONFIG_TABLE_SIZE: usize = 24;
    const DEVICE_TREE_GUID: [u8; 16] = [
        0xd5, 0x21, 0xb6, 0xb1, 0x9c, 0xf1, 0xa5, 0x41, 0x83, 0x0b, 0xd9, 0x15, 0x2c, 0x69, 0xaa,
        0xe0,
    ];

    let args = crate::arch::loongarch64::firmware_boot_args();
    // The LoongArch EFI boot ABI supplies a physical, naturally aligned
    // system-table pointer. U-Boot's standalone-application ABI only defines
    // a0/a1; treating its unspecified a2 (observed as 3 on LS2K1000) as an
    // EFI pointer would itself raise an alignment exception.
    if args.arg2 == 0
        || args.arg2 & (core::mem::align_of::<u64>() - 1) != 0
        || args.arg2 >= crate::platform::KERNEL_ADDR_OFFSET
    {
        return None;
    }
    let system_table = crate::platform::direct_map_phys_to_virt(args.arg2);
    if unsafe { ptr::read_volatile(system_table as *const u64) } != EFI_SYSTEM_TABLE_SIGNATURE {
        return None;
    }
    let table_count =
        unsafe { ptr::read_volatile((system_table + EFI_NR_TABLES_OFFSET) as *const u64) as usize };
    if table_count == 0 || table_count > 32 {
        return None;
    }
    let tables_raw =
        unsafe { ptr::read_volatile((system_table + EFI_TABLES_OFFSET) as *const u64) as usize };
    let tables = early_loongarch_addr(tables_raw)?;

    for index in 0..table_count {
        let entry = tables.checked_add(index.checked_mul(EFI_CONFIG_TABLE_SIZE)?)?;
        let guid = bytes_at(entry, DEVICE_TREE_GUID.len())?;
        if guid == DEVICE_TREE_GUID {
            let fdt_ptr = unsafe { ptr::read_volatile((entry + 16) as *const usize) };
            return FdtSource::from_ptr(fdt_ptr);
        }
    }
    None
}

#[cfg(target_arch = "loongarch64")]
fn early_loongarch_addr(address: usize) -> Option<usize> {
    if address == 0 {
        return None;
    }
    Some(if address < crate::platform::KERNEL_ADDR_OFFSET {
        crate::platform::direct_map_phys_to_virt(address)
    } else {
        address
    })
}

fn load_fdt(source: FdtSource, info: &mut BootInfo) -> bool {
    let Some(fdt) = Fdt::new(source.ptr) else {
        return false;
    };
    fdt.fill_boot_info(info);
    info.fdt_ptr = source.ptr;
    info.fdt_size = fdt.total_size;
    if source.reserve_physical_blob {
        let fdt_pa = crate::platform::direct_map_virt_to_phys(source.ptr);
        info.push_reserved_region(fdt_pa, fdt_pa.saturating_add(fdt.total_size));
    }
    true
}

/// Return the currently discovered boot information.
pub fn get() -> &'static BootInfo {
    assert!(
        READY.load(Ordering::Acquire),
        "boot information accessed before FDT initialization"
    );
    unsafe { &*ptr::addr_of!(BOOT_INFO) }
}

/// Return initialized boot information without panicking during early output.
pub fn try_get() -> Option<&'static BootInfo> {
    if !READY.load(Ordering::Acquire) {
        return None;
    }
    Some(unsafe { &*core::ptr::addr_of!(BOOT_INFO) })
}

/// Return the discovered hart count.
pub fn hart_count() -> usize {
    get().hart_count()
}

/// Return the firmware timer frequency in ticks per second.
pub fn timer_frequency() -> usize {
    get().timer_frequency()
}

/// Iterate usable RAM ranges after subtracting reserved ranges and call `f`.
pub fn for_each_usable_memory_region(mut f: impl FnMut(PhysMemoryRegion)) {
    let info = get();
    for region in info.memory_regions() {
        let mut fragments = [PhysMemoryRegion::empty(); MAX_RESERVED_REGIONS + 1];
        let mut fragment_count = 1usize;
        fragments[0] = *region;

        for reserved in info.reserved_regions() {
            let mut next = [PhysMemoryRegion::empty(); MAX_RESERVED_REGIONS + 1];
            let mut next_count = 0usize;
            for fragment in fragments[..fragment_count].iter().copied() {
                subtract_region(fragment, *reserved, &mut next, &mut next_count);
            }
            fragments = next;
            fragment_count = next_count;
        }

        for fragment in fragments[..fragment_count].iter().copied() {
            if !fragment.is_empty() {
                f(fragment);
            }
        }
    }
}

fn subtract_region(
    region: PhysMemoryRegion,
    reserved: PhysMemoryRegion,
    out: &mut [PhysMemoryRegion],
    out_count: &mut usize,
) {
    let overlap_start = max(region.start, reserved.start);
    let overlap_end = min(region.end, reserved.end);
    if overlap_start >= overlap_end {
        push_temp_region(region, out, out_count);
        return;
    }
    push_temp_region(
        PhysMemoryRegion::new(region.start, overlap_start),
        out,
        out_count,
    );
    push_temp_region(
        PhysMemoryRegion::new(overlap_end, region.end),
        out,
        out_count,
    );
}

fn push_temp_region(region: PhysMemoryRegion, out: &mut [PhysMemoryRegion], out_count: &mut usize) {
    if region.is_empty() || *out_count >= out.len() {
        return;
    }
    out[*out_count] = region;
    *out_count += 1;
}

#[derive(Clone, Copy)]
struct FdtSource {
    ptr: usize,
    reserve_physical_blob: bool,
}

impl FdtSource {
    fn from_ptr(ptr: usize) -> Option<Self> {
        if ptr == 0 {
            return None;
        }

        #[cfg(any(target_arch = "riscv64", target_arch = "loongarch64"))]
        let ptr = if ptr < crate::platform::KERNEL_ADDR_OFFSET {
            crate::platform::direct_map_phys_to_virt(ptr)
        } else {
            ptr
        };

        (ptr != 0).then_some(Self {
            ptr,
            reserve_physical_blob: true,
        })
    }
}

/// U-Boot's `bootelf` and `go` commands enter an application as
/// `entry(argc, argv)`. They do not have an implicit FDT register argument,
/// so find the working FDT address in the bounded argument vector. Both the
/// conventional raw hexadecimal form and `fdt=<hex>` are accepted.
#[cfg(target_arch = "loongarch64")]
fn uboot_bootelf_fdt_source() -> Option<FdtSource> {
    let args = crate::arch::loongarch64::firmware_boot_args();
    const MAX_UBOOT_ARGS: usize = 8;
    if args.arg0 == 0
        || args.arg0 > MAX_UBOOT_ARGS
        || args.arg1 == 0
        || args.arg1 & (core::mem::align_of::<usize>() - 1) != 0
    {
        return None;
    }

    let argv = early_loongarch_addr(args.arg1)?;
    for index in 0..args.arg0 {
        let argument =
            unsafe { ptr::read_volatile((argv + index * size_of::<usize>()) as *const usize) };
        let Some(argument) = early_loongarch_addr(argument) else {
            continue;
        };
        let Some(fdt_ptr) = parse_uboot_fdt_argument(argument) else {
            continue;
        };
        if let Some(source) = FdtSource::from_ptr(fdt_ptr) {
            return Some(source);
        }
    }
    None
}

#[cfg(target_arch = "loongarch64")]
fn parse_uboot_fdt_argument(argument: usize) -> Option<usize> {
    const PREFIX: &[u8] = b"fdt=";
    if argument == 0 {
        return None;
    }
    let mut has_prefix = true;
    for (offset, expected) in PREFIX.iter().copied().enumerate() {
        let actual = unsafe { ptr::read_volatile::<u8>((argument + offset) as *const u8) };
        if actual != expected {
            has_prefix = false;
            break;
        }
    }
    let mut cursor = argument + if has_prefix { PREFIX.len() } else { 0 };
    if unsafe { ptr::read_volatile(cursor as *const u8) } == b'0'
        && unsafe { ptr::read_volatile((cursor + 1) as *const u8) } == b'x'
    {
        cursor += 2;
    }

    let mut value = 0usize;
    let mut digits = 0usize;
    loop {
        let byte = unsafe { ptr::read_volatile((cursor + digits) as *const u8) };
        if byte == 0 {
            break;
        }
        let digit = match byte {
            b'0'..=b'9' => (byte - b'0') as usize,
            b'a'..=b'f' => (byte - b'a' + 10) as usize,
            b'A'..=b'F' => (byte - b'A' + 10) as usize,
            _ => return None,
        };
        if digits >= usize::BITS as usize / 4 {
            return None;
        }
        value = value.checked_mul(16)?.checked_add(digit)?;
        digits += 1;
    }

    (digits != 0 && value != 0).then_some(value)
}

struct Fdt {
    base: usize,
    total_size: usize,
    off_mem_rsvmap: usize,
    off_dt_struct: usize,
    size_dt_struct: usize,
    off_dt_strings: usize,
    size_dt_strings: usize,
}

impl Fdt {
    fn new(base: usize) -> Option<Self> {
        let magic = read_be_u32_at(base)?;
        if magic != FDT_MAGIC {
            return None;
        }
        let total_size = read_be_u32_at(base + 4)? as usize;
        let off_dt_struct = read_be_u32_at(base + 8)? as usize;
        let off_dt_strings = read_be_u32_at(base + 12)? as usize;
        let off_mem_rsvmap = read_be_u32_at(base + 16)? as usize;
        let version = read_be_u32_at(base + 20)?;
        let last_compatible_version = read_be_u32_at(base + 24)?;
        let size_dt_strings = read_be_u32_at(base + 32)? as usize;
        let size_dt_struct = read_be_u32_at(base + 36)? as usize;
        if !(40..=MAX_FDT_SIZE).contains(&total_size)
            || version < 17
            || last_compatible_version > 17
            || off_mem_rsvmap < 40
            || off_mem_rsvmap >= total_size
            || off_dt_struct >= total_size
            || off_dt_struct.saturating_add(size_dt_struct) > total_size
            || off_dt_strings >= total_size
            || off_dt_strings.saturating_add(size_dt_strings) > total_size
        {
            return None;
        }
        Some(Self {
            base,
            total_size,
            off_mem_rsvmap,
            off_dt_struct,
            size_dt_struct,
            off_dt_strings,
            size_dt_strings,
        })
    }

    fn fill_boot_info(&self, info: &mut BootInfo) {
        self.parse_mem_reserve(info);
        let mut cursor = self.base + self.off_dt_struct;
        let end = self.base + self.off_dt_struct + self.size_dt_struct;
        let mut depth = 0usize;
        let mut current = NodeState::default();
        let mut stack = [NodeState::default(); 16];

        while cursor + 4 <= end {
            let Some(token) = read_be_u32_at(cursor) else {
                break;
            };
            cursor += 4;
            match token {
                FDT_BEGIN_NODE => {
                    if depth < stack.len() {
                        stack[depth] = current;
                    }
                    let name_start = cursor;
                    while cursor < end && read_u8_at(cursor) != Some(0) {
                        cursor += 1;
                    }
                    let name =
                        bytes_at(name_start, cursor.saturating_sub(name_start)).unwrap_or(&[]);
                    cursor = align4(cursor.saturating_add(1));
                    current =
                        NodeState::for_child(stack.get(depth).copied().unwrap_or_default(), name);
                    depth += 1;
                }
                FDT_END_NODE => {
                    current.finish(info);
                    depth = depth.saturating_sub(1);
                    current = stack.get(depth).copied().unwrap_or_default();
                }
                FDT_PROP => {
                    if cursor + 8 > end {
                        break;
                    }
                    let len = read_be_u32_at(cursor).unwrap_or(0) as usize;
                    let nameoff = read_be_u32_at(cursor + 4).unwrap_or(usize::MAX as u32) as usize;
                    cursor += 8;
                    let Some(prop_name) = self.string(nameoff) else {
                        cursor = align4(cursor.saturating_add(len));
                        continue;
                    };
                    let value = bytes_at(cursor, len).unwrap_or(&[]);
                    current.apply_property(prop_name, value);
                    cursor = align4(cursor.saturating_add(len));
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }
        info.resolve_timer_frequency();
    }

    fn parse_mem_reserve(&self, info: &mut BootInfo) {
        let mut cursor = self.base + self.off_mem_rsvmap;
        let end = self.base + self.total_size;
        while cursor.saturating_add(16) <= end {
            let Some(address) = read_be_u64_at(cursor) else {
                break;
            };
            let Some(size) = read_be_u64_at(cursor + 8) else {
                break;
            };
            cursor += 16;
            if address == 0 && size == 0 {
                break;
            }
            let start = address as usize;
            let end = start.saturating_add(size as usize);
            info.push_reserved_region(start, end);
        }
    }

    fn string(&self, offset: usize) -> Option<&'static [u8]> {
        if offset >= self.size_dt_strings {
            return None;
        }
        let start = self.base + self.off_dt_strings + offset;
        let limit = self.base + self.off_dt_strings + self.size_dt_strings;
        let mut end = start;
        while end < limit && read_u8_at(end) != Some(0) {
            end += 1;
        }
        bytes_at(start, end.saturating_sub(start))
    }
}

#[derive(Clone, Copy, Default)]
struct NodeState {
    parent_is_cpus: bool,
    is_cpus: bool,
    is_cpu: bool,
    is_memory: bool,
    is_reserved_memory: bool,
    is_uart: bool,
    is_rtc: bool,
    is_plic: bool,
    is_virtio_mmio: bool,
    is_pch_pic: bool,
    is_eiointc: bool,
    is_pci_host: bool,
    is_ahci: bool,
    is_gmac: bool,
    address_cells: usize,
    size_cells: usize,
    child_address_cells: usize,
    child_size_cells: usize,
    status_ok: bool,
    timebase_frequency: usize,
    clock_frequency: usize,
    phandle: u32,
    clock_phandle: u32,
    irq: Option<u32>,
    mac_address: [u8; 6],
    has_mac_address: bool,
    bus_start: u8,
    bus_end: u8,
    ranges_ptr: usize,
    ranges_len: usize,
    interrupt_map_ptr: usize,
    interrupt_map_len: usize,
    reg_regions: [PhysMemoryRegion; 4],
    reg_region_count: usize,
}

impl NodeState {
    fn for_child(parent: Self, name: &[u8]) -> Self {
        let is_cpus = name == b"cpus";
        let is_cpu = parent.is_cpus && starts_with(name, b"cpu@");
        let is_memory = name == b"memory" || starts_with(name, b"memory@");
        let is_reserved_memory = parent.is_reserved_memory || name == b"reserved-memory";
        Self {
            parent_is_cpus: parent.is_cpus,
            is_cpus,
            is_cpu,
            is_memory,
            is_reserved_memory,
            is_uart: false,
            is_rtc: false,
            is_plic: false,
            is_virtio_mmio: false,
            is_pch_pic: false,
            is_eiointc: false,
            is_pci_host: false,
            is_ahci: false,
            is_gmac: false,
            address_cells: parent.child_address_cells.max(1),
            size_cells: parent.child_size_cells.max(1),
            child_address_cells: 2,
            child_size_cells: 1,
            status_ok: true,
            timebase_frequency: 0,
            clock_frequency: 0,
            phandle: 0,
            clock_phandle: 0,
            irq: None,
            mac_address: [0; 6],
            has_mac_address: false,
            bus_start: 0,
            bus_end: 0,
            ranges_ptr: 0,
            ranges_len: 0,
            interrupt_map_ptr: 0,
            interrupt_map_len: 0,
            reg_regions: [PhysMemoryRegion::empty(); 4],
            reg_region_count: 0,
        }
    }

    fn apply_property(&mut self, name: &[u8], value: &[u8]) {
        match name {
            b"#address-cells" => self.child_address_cells = read_cells_usize(value, 1).unwrap_or(2),
            b"#size-cells" => self.child_size_cells = read_cells_usize(value, 1).unwrap_or(1),
            b"status" => {
                self.status_ok = value == b"okay\0" || value == b"ok\0" || value.is_empty()
            }
            b"device_type" if self.parent_is_cpus && value == b"cpu\0" => self.is_cpu = true,
            b"device_type" if value == b"memory\0" => self.is_memory = true,
            b"compatible" => {
                self.is_uart = compatible_contains(value, b"ns16550a")
                    || compatible_contains(value, b"ns16550");
                self.is_rtc = compatible_contains(value, b"google,goldfish-rtc")
                    || compatible_contains(value, b"loongson,ls7a-rtc")
                    || compatible_contains(value, b"loongson,ls2k-rtc")
                    || compatible_contains(value, b"loongson,ls2k1000-rtc")
                    || compatible_contains(value, b"loongson,ls-rtc");
                self.is_plic = compatible_contains(value, b"riscv,plic0")
                    || compatible_contains(value, b"sifive,plic-1.0.0");
                self.is_virtio_mmio = compatible_contains(value, b"virtio,mmio");
                self.is_pch_pic = compatible_contains(value, b"loongson,pch-pic-1.0");
                self.is_eiointc = compatible_contains(value, b"loongson,ls2k2000-eiointc");
                self.is_pci_host = compatible_contains(value, b"pci-host-ecam-generic");
                self.is_ahci = compatible_contains(value, b"snps,spear-ahci")
                    || compatible_contains(value, b"loongson,ls-ahci")
                    || compatible_contains(value, b"loongson,ls2k1000-ahci")
                    || compatible_contains(value, b"loongson,2k1000-ahci")
                    || compatible_contains(value, b"generic-ahci")
                    || compatible_contains(value, b"snps,dwc-ahci");
                self.is_gmac = compatible_contains(value, b"snps,dwmac-3.70a")
                    || compatible_contains(value, b"snps,arc-dwmac-3.70a")
                    || compatible_contains(value, b"ls,ls-gmac");
            }
            b"timebase-frequency" => {
                self.timebase_frequency = read_cells_usize(value, 1).unwrap_or(0)
            }
            b"clock-frequency" => self.clock_frequency = read_cells_usize(value, 1).unwrap_or(0),
            b"phandle" | b"linux,phandle" if value.len() >= 4 => {
                self.phandle = read_be_u32(&value[..4]).unwrap_or(0)
            }
            b"clocks" if value.len() >= 4 => {
                self.clock_phandle = read_be_u32(&value[..4]).unwrap_or(0)
            }
            b"interrupts" if value.len() >= 4 => self.irq = read_be_u32(&value[..4]),
            b"local-mac-address" | b"mac-address" if value.len() >= 6 => {
                let mut mac = [0u8; 6];
                mac.copy_from_slice(&value[..6]);
                if valid_unicast_mac(mac) {
                    self.mac_address = mac;
                    self.has_mac_address = true;
                }
            }
            b"bus-range" if value.len() >= 8 => {
                self.bus_start = read_be_u32(&value[..4]).unwrap_or(0) as u8;
                self.bus_end = read_be_u32(&value[4..8]).unwrap_or(0) as u8;
            }
            b"ranges" => {
                self.ranges_ptr = value.as_ptr() as usize;
                self.ranges_len = value.len();
            }
            b"interrupt-map" => {
                self.interrupt_map_ptr = value.as_ptr() as usize;
                self.interrupt_map_len = value.len();
            }
            b"reg" => {
                parse_reg(value, self.address_cells, self.size_cells, |start, size| {
                    self.push_reg_region(start, size);
                });
            }
            _ => {}
        }
    }

    fn finish(&self, info: &mut BootInfo) {
        if self.is_cpu && self.status_ok {
            info.set_hart_count(info.hart_count.saturating_add(1));
        }
        if !self.status_ok {
            return;
        }
        if self.is_cpus && self.timebase_frequency != 0 {
            info.timer_frequency = self.timebase_frequency;
        }
        if self.is_cpu && info.timer_frequency == 0 && self.clock_frequency != 0 {
            info.timer_frequency = self.clock_frequency;
        }
        if self.phandle != 0 && (self.clock_phandle != 0 || self.clock_frequency != 0) {
            info.push_clock(self.phandle, self.clock_phandle, self.clock_frequency);
        }
        for region in self.reg_regions[..self.reg_region_count].iter().copied() {
            if self.is_memory {
                info.push_memory_region(region.start, region.end);
            } else if self.is_reserved_memory {
                info.push_reserved_region(region.start, region.end);
            }
        }
        let Some(region) = self.reg_regions.first().copied() else {
            return;
        };
        if region.is_empty() {
            return;
        }
        let resource = DeviceResource {
            start: region.start,
            size: region.end - region.start,
            irq: self.irq,
        };
        if self.is_uart && info.uart.is_none() {
            info.uart = Some(resource);
            info.uart_clock_phandle = self.clock_phandle;
            info.push_mmio_region(resource);
            // LoongArch QEMU and LS2K firmware describe the constant timer
            // clock on the UART node instead of /cpus/timebase-frequency.
            if info.timer_frequency == 0 && self.clock_frequency != 0 {
                info.timer_frequency = self.clock_frequency;
            }
        } else if self.is_rtc && info.rtc.is_none() {
            info.rtc = Some(resource);
            info.push_mmio_region(resource);
        } else if self.is_plic && info.plic.is_none() {
            info.plic = Some(resource);
            info.push_mmio_region(resource);
        } else if self.is_virtio_mmio {
            info.push_virtio_mmio(resource);
        } else if self.is_pch_pic && info.pch_pic.is_none() {
            info.pch_pic = Some(resource);
            info.push_mmio_region(resource);
        } else if self.is_eiointc && info.eiointc.is_none() {
            info.eiointc = Some(resource);
        } else if self.is_pci_host && info.pci_host.is_none() {
            let mut host = PciHostResource::empty();
            host.ecam = resource;
            host.bus_start = self.bus_start;
            host.bus_end = self.bus_end;
            self.parse_pci_ranges(&mut host);
            self.parse_pci_interrupt_map(&mut host);
            info.pci_host = Some(host);
            info.push_mmio_region(resource);
        } else if self.is_ahci && info.ahci.is_none() {
            info.ahci = Some(resource);
            info.push_mmio_region(resource);
        } else if self.is_gmac && info.gmac.is_none() {
            info.gmac = Some(GmacResource {
                device: resource,
                mac_address: self.has_mac_address.then_some(self.mac_address),
            });
            info.push_mmio_region(resource);
        }
    }

    fn push_reg_region(&mut self, start: usize, size: usize) {
        if size == 0 || self.reg_region_count >= self.reg_regions.len() {
            return;
        }
        let start = firmware_address_to_phys(start);
        let end = start.saturating_add(size);
        if start >= end {
            return;
        }
        self.reg_regions[self.reg_region_count] = PhysMemoryRegion::new(start, end);
        self.reg_region_count += 1;
    }

    fn parse_pci_ranges(&self, host: &mut PciHostResource) {
        let Some(mut value) = bytes_at(self.ranges_ptr, self.ranges_len) else {
            return;
        };
        let child_address_cells = self.child_address_cells;
        let parent_address_cells = self.address_cells;
        let size_cells = self.child_size_cells;
        let stride = (child_address_cells + parent_address_cells + size_cells) * 4;
        while child_address_cells >= 3 && size_cells != 0 && value.len() >= stride {
            let flags = read_be_u32(&value[..4]).unwrap_or(0);
            let parent_offset = child_address_cells * 4;
            let size_offset = parent_offset + parent_address_cells * 4;
            let parent = read_cells_usize(&value[parent_offset..size_offset], parent_address_cells);
            let size = read_cells_usize(&value[size_offset..stride], size_cells);
            // PCI range type 0b10 is non-prefetchable memory.
            if (flags >> 24) & 0x03 == 0x02 {
                if let (Some(parent), Some(size)) = (parent, size) {
                    host.memory_start = firmware_address_to_phys(parent);
                    host.memory_size = size;
                    return;
                }
            }
            value = &value[stride..];
        }
    }

    fn parse_pci_interrupt_map(&self, host: &mut PciHostResource) {
        let Some(mut value) = bytes_at(self.interrupt_map_ptr, self.interrupt_map_len) else {
            return;
        };
        // The supported Loongson PCH PIC binding uses two interrupt cells.
        // Each map row is child address (3), child IRQ (1), phandle (1),
        // then PCH IRQ and flags (2).
        const ROW_CELLS: usize = 7;
        const ROW_BYTES: usize = ROW_CELLS * 4;
        while value.len() >= ROW_BYTES {
            let address_hi = read_be_u32(&value[..4]).unwrap_or(0);
            let pin = read_be_u32(&value[12..16]).unwrap_or(0) as usize;
            let irq = read_be_u32(&value[20..24]).unwrap_or(0);
            let slot = ((address_hi >> 11) & 0x1f) as usize;
            if (1..=4).contains(&pin) {
                host.intx_irqs[slot * 4 + pin - 1] = irq;
            }
            value = &value[ROW_BYTES..];
        }
    }
}

fn valid_unicast_mac(mac: [u8; 6]) -> bool {
    mac != [0; 6] && mac != [0xff; 6] && mac[0] & 1 == 0
}

fn compatible_contains(mut value: &[u8], needle: &[u8]) -> bool {
    while !value.is_empty() {
        let end = value
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(value.len());
        if &value[..end] == needle {
            return true;
        }
        if end == value.len() {
            break;
        }
        value = &value[end + 1..];
    }
    false
}

fn parse_reg(
    mut value: &[u8],
    address_cells: usize,
    size_cells: usize,
    mut f: impl FnMut(usize, usize),
) {
    let stride = (address_cells + size_cells) * 4;
    if stride == 0 {
        return;
    }
    while value.len() >= stride {
        let Some(start) = read_cells_usize(&value[..address_cells * 4], address_cells) else {
            break;
        };
        let size_offset = address_cells * 4;
        let Some(size) = read_cells_usize(
            &value[size_offset..size_offset + size_cells * 4],
            size_cells,
        ) else {
            break;
        };
        if size != 0 {
            f(start, size);
        }
        value = &value[stride..];
    }
}

/// Convert a firmware-described CPU address into the physical-address form
/// used internally. Some LoongArch U-Boot trees describe RAM and MMIO through
/// a DMW alias; QEMU and RISC-V trees already contain physical addresses.
fn firmware_address_to_phys(address: usize) -> usize {
    #[cfg(target_arch = "loongarch64")]
    {
        crate::platform::translate_direct_mapped_kernel_va(address).unwrap_or(address)
    }

    #[cfg(not(target_arch = "loongarch64"))]
    {
        address
    }
}

fn read_cells_usize(value: &[u8], cells: usize) -> Option<usize> {
    if cells == 0 || cells > 2 || value.len() < cells * 4 {
        return None;
    }
    let mut out = 0usize;
    for cell in 0..cells {
        out = (out << 32) | read_be_u32(value.get(cell * 4..cell * 4 + 4)?)? as usize;
    }
    Some(out)
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

fn starts_with(value: &[u8], prefix: &[u8]) -> bool {
    value.len() >= prefix.len() && &value[..prefix.len()] == prefix
}

fn bytes_at(addr: usize, len: usize) -> Option<&'static [u8]> {
    if addr == 0 {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(addr as *const u8, len) })
}

fn read_u8_at(addr: usize) -> Option<u8> {
    if addr == 0 {
        return None;
    }
    Some(unsafe { ptr::read_volatile(addr as *const u8) })
}

fn read_be_u32_at(addr: usize) -> Option<u32> {
    let bytes = [
        read_u8_at(addr)?,
        read_u8_at(addr + 1)?,
        read_u8_at(addr + 2)?,
        read_u8_at(addr + 3)?,
    ];
    Some(u32::from_be_bytes(bytes))
}

fn read_be_u64_at(addr: usize) -> Option<u64> {
    let bytes = [
        read_u8_at(addr)?,
        read_u8_at(addr + 1)?,
        read_u8_at(addr + 2)?,
        read_u8_at(addr + 3)?,
        read_u8_at(addr + 4)?,
        read_u8_at(addr + 5)?,
        read_u8_at(addr + 6)?,
        read_u8_at(addr + 7)?,
    ];
    Some(u64::from_be_bytes(bytes))
}

fn read_be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}
