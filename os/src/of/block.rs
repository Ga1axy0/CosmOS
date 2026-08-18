//! Open Firmware resources for block-controller bindings.

use crate::of::DeviceResource;

/// One firmware-described SD/MMC controller and its slot policy.
#[derive(Clone, Copy, Debug)]
pub struct MmcResource {
    device: DeviceResource,
    bus_width: u32,
    no_sd: bool,
    no_mmc: bool,
    non_removable: bool,
    supports_1v8: bool,
}

impl MmcResource {
    /// Create a parsed MMC resource.  Only OF enumeration code constructs it.
    pub(crate) const fn new(
        device: DeviceResource,
        bus_width: u32,
        no_sd: bool,
        no_mmc: bool,
        non_removable: bool,
        supports_1v8: bool,
    ) -> Self {
        Self { device, bus_width, no_sd, no_mmc, non_removable, supports_1v8 }
    }

    /// Return the controller register and interrupt resource.
    pub fn device(self) -> DeviceResource { self.device }
    /// Return the maximum firmware-advertised data-bus width.
    pub fn bus_width(self) -> u32 { self.bus_width }
    /// Return whether firmware forbids probing an SD card.
    pub fn no_sd(self) -> bool { self.no_sd }
    /// Return whether firmware forbids probing an MMC/eMMC card.
    pub fn no_mmc(self) -> bool { self.no_mmc }
    /// Return whether the slot is soldered-down rather than removable.
    pub fn non_removable(self) -> bool { self.non_removable }
    /// Return whether firmware advertises a 1.8 V timing mode.
    pub fn supports_1v8(self) -> bool { self.supports_1v8 }
}
