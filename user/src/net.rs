//! Userspace socket ABI helpers.

/// IPv4 address family.
pub const AF_INET: usize = 2;
/// Stream socket type.
pub const SOCK_STREAM: usize = 1;
/// Datagram socket type.
pub const SOCK_DGRAM: usize = 2;

/// IPv4 socket address compatible with kernel `SockAddrIn`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct SockAddrIn {
    /// Address family.
    pub sin_family: u16,
    /// Port in network byte order.
    pub sin_port: u16,
    /// IPv4 address in network byte order.
    pub sin_addr: u32,
    /// Padding.
    pub sin_zero: [u8; 8],
}

impl SockAddrIn {
    /// Build from IPv4 octets and host-endian port.
    pub fn from_ipv4_port(ip: [u8; 4], port: u16) -> Self {
        Self {
            sin_family: AF_INET as u16,
            sin_port: port.to_be(),
            sin_addr: u32::from_be_bytes(ip),
            sin_zero: [0; 8],
        }
    }

    /// Return IPv4 octets in host byte order.
    pub fn ipv4(&self) -> [u8; 4] {
        self.sin_addr.to_be_bytes()
    }

    /// Return port in host byte order.
    pub fn port(&self) -> u16 {
        u16::from_be(self.sin_port)
    }
}
