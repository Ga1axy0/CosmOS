//! Early binding-aware scan layered over the generic FDT token walker.

use crate::boot::context::BootContext;
use crate::boot::source::FdtSource;
use crate::of::{FdtBlob, FdtVisitor};
use crate::of::node::NodeState;

/// Scan one validated FDT into an unpublished boot context.
pub(crate) fn scan_fdt(source: FdtSource, context: &mut BootContext) -> bool {
    // SAFETY: the FDT source is direct-mapped firmware memory and the parser
    // validates its header before walking the token stream.
    let Ok(blob) = (unsafe { FdtBlob::from_virt(source.ptr) }) else { return false; };
    let mut visitor = EarlyBootVisitor::new(context);
    if blob.walk(&mut visitor).is_err() { return false; }
    visitor.finish();
    context.set_fdt(blob);
    if source.reserve_physical_blob {
        let physical = crate::platform::direct_map_virt_to_phys(source.ptr);
        context.push_reserved_region(physical, physical.saturating_add(blob.total_size()));
    }
    true
}

struct EarlyBootVisitor<'a> {
    context: &'a mut BootContext,
    current: NodeState,
    stack: [NodeState; 16],
    depth: usize,
}

impl<'a> EarlyBootVisitor<'a> {
    fn new(context: &'a mut BootContext) -> Self {
        Self { context, current: NodeState::default(), stack: [NodeState::default(); 16], depth: 0 }
    }
    fn finish(&mut self) {
        self.context.resolve_gmac_phys();
        self.context.resolve_timer_frequency();
    }
}

impl FdtVisitor for EarlyBootVisitor<'_> {
    fn reserve_entry(&mut self, address: u64, size: u64) {
        self.context.push_reserved_region(address as usize, (address as usize).saturating_add(size as usize));
    }
    fn begin_node(&mut self, name: &[u8]) {
        if self.depth < self.stack.len() { self.stack[self.depth] = self.current; }
        self.current = NodeState::for_child(self.stack.get(self.depth).copied().unwrap_or_default(), name);
        self.depth += 1;
    }
    fn property(&mut self, name: &[u8], value: &[u8]) { self.current.apply_property(name, value); }
    fn end_node(&mut self) {
        self.current.finish(self.context);
        self.depth = self.depth.saturating_sub(1);
        self.current = self.stack.get(self.depth).copied().unwrap_or_default();
    }
}
