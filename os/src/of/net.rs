//! Open Firmware resources shared by Ethernet MAC and PHY drivers.

use crate::of::DeviceResource;

/// One firmware-described DesignWare-compatible GMAC controller.
#[derive(Clone, Copy, Debug)]
pub struct GmacResource {
    device: DeviceResource,
    mac_address: Option<[u8; 6]>,
    phy_mode: PhyInterfaceMode,
    phy_handle: Option<u32>,
    phy_addr: Option<u8>,
    pinctrl_default: Option<u32>,
}

impl GmacResource {
    /// Create a parsed GMAC resource.  Only OF enumeration code constructs it.
    pub(crate) const fn new(
        device: DeviceResource,
        mac_address: Option<[u8; 6]>,
        phy_mode: PhyInterfaceMode,
        phy_handle: Option<u32>,
        phy_addr: Option<u8>,
        pinctrl_default: Option<u32>,
    ) -> Self {
        Self {
            device,
            mac_address,
            phy_mode,
            phy_handle,
            phy_addr,
            pinctrl_default,
        }
    }

    /// Return the controller register and interrupt resource.
    pub fn device(self) -> DeviceResource { self.device }
    /// Return the firmware-provided station address, when valid.
    pub fn mac_address(self) -> Option<[u8; 6]> { self.mac_address }
    /// Return the FDT-selected MAC-to-PHY electrical interface.
    pub fn phy_mode(self) -> PhyInterfaceMode { self.phy_mode }
    /// Return the resolved PHY address, when firmware supplied one.
    pub fn phy_addr(self) -> Option<u8> { self.phy_addr }
    /// Return the raw `phy-handle` phandle supplied by firmware.
    pub fn phy_handle(self) -> Option<u32> { self.phy_handle }
    /// Return the default pinctrl-state phandle, when firmware supplies one.
    pub fn pinctrl_default(self) -> Option<u32> { self.pinctrl_default }

    /// Return a copy with a phandle resolved into a PHY address.
    pub(crate) const fn with_phy_addr(mut self, phy_addr: Option<u8>) -> Self {
        self.phy_addr = phy_addr;
        self
    }
}

/// MAC-to-PHY electrical interface selected by firmware.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PhyInterfaceMode {
    /// Firmware did not describe an interface mode.
    #[default]
    Unknown,
    /// Media Independent Interface.
    Mii,
    /// Reduced Media Independent Interface.
    Rmii,
    /// Reduced Gigabit Media Independent Interface.
    Rgmii,
    /// RGMII with both internal RX and TX delays.
    RgmiiId,
    /// RGMII with an internal RX delay.
    RgmiiRxId,
    /// RGMII with an internal TX delay.
    RgmiiTxId,
}

/// Decode the standard `phy-mode` string property.
pub fn parse_phy_mode(value: &[u8]) -> PhyInterfaceMode {
    match value.strip_suffix(&[0]).unwrap_or(value) {
        b"mii" => PhyInterfaceMode::Mii,
        b"rmii" => PhyInterfaceMode::Rmii,
        b"rgmii" => PhyInterfaceMode::Rgmii,
        b"rgmii-id" => PhyInterfaceMode::RgmiiId,
        b"rgmii-rxid" => PhyInterfaceMode::RgmiiRxId,
        b"rgmii-txid" => PhyInterfaceMode::RgmiiTxId,
        _ => PhyInterfaceMode::Unknown,
    }
}
