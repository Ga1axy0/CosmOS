//! Early boot information discovered from firmware device trees.

use core::cmp::{max, min};
use core::ptr;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::config::MAX_HARTS;
use crate::console::print;

const MAX_MEMORY_REGIONS: usize = 8;
const MAX_RESERVED_REGIONS: usize = 16;
const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// One physical memory byte range.
#[derive(Clone, Copy, Debug)]
pub struct PhysMemoryRegion {
    /// Inclusive physical start address.
    pub start: usize,
    /// Exclusive physical end address.
    pub end: usize,
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
            fdt_ptr: 0,
            fdt_size: 0,
        }
    }

    fn push_memory_region(&mut self, start: usize, end: usize) {
        if start >= end || self.memory_region_count >= MAX_MEMORY_REGIONS {
            return;
        }
        self.memory_regions[self.memory_region_count] = PhysMemoryRegion::new(start, end);
        self.memory_region_count += 1;
    }

    fn push_reserved_region(&mut self, start: usize, end: usize) {
        if start >= end || self.reserved_region_count >= MAX_RESERVED_REGIONS {
            return;
        }
        self.reserved_regions[self.reserved_region_count] = PhysMemoryRegion::new(start, end);
        self.reserved_region_count += 1;
    }

    fn set_hart_count(&mut self, hart_count: usize) {
        self.hart_count = hart_count.clamp(1, MAX_HARTS);
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
    let mut source = FdtSource::from_ptr(fdt_ptr);

    if source.is_none() {
        source = platform_fdt_source();
    }

    if let Some(source) = source {
        if let Some(fdt) = Fdt::new(source.ptr) {
            fdt.fill_boot_info(&mut info);
            info.fdt_ptr = source.ptr;
            info.fdt_size = fdt.total_size;
            if source.reserve_physical_blob {
                let fdt_pa = crate::platform::direct_map_virt_to_phys(source.ptr);
                info.push_reserved_region(fdt_pa, fdt_pa.saturating_add(fdt.total_size));
            }
        }
    }

    if info.memory_region_count == 0 {
        fallback_memory_regions(&mut info);
    }
    platform_reserved_regions(&mut info);
    if info.hart_count == 0 {
        info.hart_count = 1;
    }

    unsafe {
        ptr::write(ptr::addr_of_mut!(BOOT_INFO), info);
    }
    READY.store(true, Ordering::Release);
}

/// Return the currently discovered boot information.
pub fn get() -> &'static BootInfo {
    if !READY.load(Ordering::Acquire) {
        return fallback_boot_info();
    }
    unsafe { &*ptr::addr_of!(BOOT_INFO) }
}

/// Return the discovered hart count.
pub fn hart_count() -> usize {
    get().hart_count()
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
    push_temp_region(PhysMemoryRegion::new(region.start, overlap_start), out, out_count);
    push_temp_region(PhysMemoryRegion::new(overlap_end, region.end), out, out_count);
}

fn push_temp_region(region: PhysMemoryRegion, out: &mut [PhysMemoryRegion], out_count: &mut usize) {
    if region.is_empty() || *out_count >= out.len() {
        return;
    }
    out[*out_count] = region;
    *out_count += 1;
}

fn fallback_boot_info() -> &'static BootInfo {
    static mut FALLBACK: BootInfo = BootInfo::empty();
    static FALLBACK_READY: AtomicBool = AtomicBool::new(false);
    if !FALLBACK_READY.load(Ordering::Acquire) {
        let mut info = BootInfo::empty();
        fallback_memory_regions(&mut info);
        platform_reserved_regions(&mut info);
        info.hart_count = 1;
        unsafe {
            ptr::write(ptr::addr_of_mut!(FALLBACK), info);
        }
        FALLBACK_READY.store(true, Ordering::Release);
    }
    unsafe { &*ptr::addr_of!(FALLBACK) }
}

fn fallback_memory_regions(info: &mut BootInfo) {
    info.push_memory_region(fallback_memory_start(), crate::config::MEMORY_END);
}

#[cfg(target_arch = "riscv64")]
fn platform_reserved_regions(info: &mut BootInfo) {
    // RustSBI/OpenSBI occupies the low part of QEMU virt RAM and protects it
    // with PMP. It is usually not described as reserved in the payload DTB, so
    // do not let the S-mode frame allocator write freelist metadata there.
    info.push_reserved_region(0x8000_0000, 0x8020_0000);
}

#[cfg(target_arch = "loongarch64")]
fn platform_reserved_regions(_info: &mut BootInfo) {}

#[cfg(target_arch = "riscv64")]
fn fallback_memory_start() -> usize {
    0x8000_0000
}

#[cfg(target_arch = "loongarch64")]
fn fallback_memory_start() -> usize {
    0x8000_0000
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

        #[cfg(target_arch = "loongarch64")]
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

#[cfg(target_arch = "loongarch64")]
fn platform_fdt_source() -> Option<FdtSource> {
    fw_cfg::load_fdt()
}

#[cfg(not(target_arch = "loongarch64"))]
fn platform_fdt_source() -> Option<FdtSource> {
    None
}

struct Fdt {
    base: usize,
    total_size: usize,
    off_dt_struct: usize,
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
        let size_dt_strings = read_be_u32_at(base + 36)? as usize;
        if total_size < 40
            || off_dt_struct >= total_size
            || off_dt_strings >= total_size
            || off_dt_strings.saturating_add(size_dt_strings) > total_size
        {
            return None;
        }
        Some(Self {
            base,
            total_size,
            off_dt_struct,
            off_dt_strings,
            size_dt_strings,
        })
    }

    fn fill_boot_info(&self, info: &mut BootInfo) {
        self.parse_mem_reserve(info);
        let mut cursor = self.base + self.off_dt_struct;
        let end = self.base + self.total_size;
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
                    let name = bytes_at(name_start, cursor.saturating_sub(name_start)).unwrap_or(&[]);
                    cursor = align4(cursor.saturating_add(1));
                    current = NodeState::for_child(stack.get(depth).copied().unwrap_or_default(), name);
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
                    current.apply_property(prop_name, value, info);
                    cursor = align4(cursor.saturating_add(len));
                }
                FDT_NOP => {}
                FDT_END => break,
                _ => break,
            }
        }
    }

    fn parse_mem_reserve(&self, info: &mut BootInfo) {
        let mut cursor = self.base + 40;
        loop {
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
    address_cells: usize,
    size_cells: usize,
    child_address_cells: usize,
    child_size_cells: usize,
    status_ok: bool,
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
            address_cells: parent.child_address_cells.max(1),
            size_cells: parent.child_size_cells.max(1),
            child_address_cells: 2,
            child_size_cells: 1,
            status_ok: true,
        }
    }

    fn apply_property(&mut self, name: &[u8], value: &[u8], info: &mut BootInfo) {
        match name {
            b"#address-cells" => self.child_address_cells = read_cells_usize(value, 1).unwrap_or(2),
            b"#size-cells" => self.child_size_cells = read_cells_usize(value, 1).unwrap_or(1),
            b"status" => self.status_ok = value == b"okay\0" || value == b"ok\0" || value.is_empty(),
            b"device_type" if self.parent_is_cpus && value == b"cpu\0" => self.is_cpu = true,
            b"device_type" if value == b"memory\0" => self.is_memory = true,
            b"reg" if self.is_memory && self.status_ok => {
                parse_reg(value, self.address_cells, self.size_cells, |start, size| {
                    info.push_memory_region(start, start.saturating_add(size));
                });
            }
            b"reg" if self.is_reserved_memory && !self.is_memory => {
                parse_reg(value, self.address_cells, self.size_cells, |start, size| {
                    info.push_reserved_region(start, start.saturating_add(size));
                });
            }
            _ => {}
        }
    }

    fn finish(&self, info: &mut BootInfo) {
        if self.is_cpu && self.status_ok {
            info.set_hart_count(info.hart_count.saturating_add(1));
        }
    }
}

fn parse_reg(mut value: &[u8], address_cells: usize, size_cells: usize, mut f: impl FnMut(usize, usize)) {
    let stride = (address_cells + size_cells) * 4;
    if stride == 0 {
        return;
    }
    while value.len() >= stride {
        let Some(start) = read_cells_usize(&value[..address_cells * 4], address_cells) else {
            break;
        };
        let size_offset = address_cells * 4;
        let Some(size) = read_cells_usize(&value[size_offset..size_offset + size_cells * 4], size_cells) else {
            break;
        };
        if size != 0 {
            f(start, size);
        }
        value = &value[stride..];
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

#[cfg(target_arch = "loongarch64")]
mod fw_cfg {
    use core::ptr;

    use super::FdtSource;

    const FW_CFG_BASE: usize = 0x1e02_0000;
    const FW_CFG_DATA: usize = 0x00;
    const FW_CFG_SELECTOR: usize = 0x08;
    const FW_CFG_FILE_DIR: u16 = 0x0019;
    const FW_CFG_MAX_FILE: usize = 2 * 1024 * 1024;
    const FW_CFG_FILE_NAME_LEN: usize = 56;

    static mut FDT_BUF: [u8; FW_CFG_MAX_FILE] = [0; FW_CFG_MAX_FILE];

    pub(super) fn load_fdt() -> Option<FdtSource> {
        select(FW_CFG_FILE_DIR);
        let count = read_be_u32()? as usize;
        for _ in 0..count {
            let size = read_be_u32()? as usize;
            let select_id = read_be_u16()?;
            let _reserved = read_be_u16()?;
            let mut name = [0u8; FW_CFG_FILE_NAME_LEN];
            for byte in &mut name {
                *byte = read_u8();
            }
            if is_name(&name, b"etc/fdt") {
                if size == 0 || size > FW_CFG_MAX_FILE {
                    return None;
                }
                select(select_id);
                let buf = ptr::addr_of_mut!(FDT_BUF) as *mut u8;
                for idx in 0..size {
                    unsafe {
                        ptr::write_volatile(buf.add(idx), read_u8());
                    }
                }
                return Some(FdtSource {
                    ptr: buf as usize,
                    reserve_physical_blob: false,
                });
            }
        }
        None
    }

    fn is_name(name: &[u8], expected: &[u8]) -> bool {
        let len = name.iter().position(|byte| *byte == 0).unwrap_or(name.len());
        &name[..len] == expected
    }

    fn select(selector: u16) {
        let selector_addr = crate::platform::mmio_phys_to_virt(FW_CFG_BASE + FW_CFG_SELECTOR);
        unsafe {
            ptr::write_volatile(selector_addr as *mut u16, selector.to_be());
        }
    }

    fn data_addr() -> usize {
        crate::platform::mmio_phys_to_virt(FW_CFG_BASE + FW_CFG_DATA)
    }

    fn read_u8() -> u8 {
        unsafe { ptr::read_volatile(data_addr() as *const u8) }
    }

    fn read_be_u16() -> Option<u16> {
        Some(u16::from_be_bytes([read_u8(), read_u8()]))
    }

    fn read_be_u32() -> Option<u32> {
        Some(u32::from_be_bytes([read_u8(), read_u8(), read_u8(), read_u8()]))
    }
}
