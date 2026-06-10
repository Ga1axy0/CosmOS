//! Userspace socket ABI helpers.

/// UNIX domain address family.
pub const AF_UNIX: usize = 1;
/// IPv4 address family.
pub const AF_INET: usize = 2;
/// Stream socket type.
pub const SOCK_STREAM: usize = 1;
/// Datagram socket type.
pub const SOCK_DGRAM: usize = 2;
/// shutdown read side.
pub const SHUT_RD: usize = 0;
/// shutdown write side.
pub const SHUT_WR: usize = 1;
/// shutdown both sides.
pub const SHUT_RDWR: usize = 2;

/// socket level for control messages.
pub const SOL_SOCKET: i32 = 1;
/// pass file descriptors as ancillary data.
pub const SCM_RIGHTS: i32 = 1;
/// pass credentials as ancillary data.
pub const SCM_CREDENTIALS: i32 = 2;

/// `recvmsg` flag: set `FD_CLOEXEC` on received fds.
pub const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;

/// Userspace iovec.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct IoVec {
    pub iov_base: usize,
    pub iov_len: usize,
}

impl IoVec {
    pub fn from_slice(buf: &[u8]) -> Self {
        Self {
            iov_base: buf.as_ptr() as usize,
            iov_len: buf.len(),
        }
    }

    pub fn from_mut_slice(buf: &mut [u8]) -> Self {
        Self {
            iov_base: buf.as_mut_ptr() as usize,
            iov_len: buf.len(),
        }
    }
}

/// Userspace msghdr.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct MsgHdr {
    pub msg_name: usize,
    pub msg_namelen: usize,
    pub msg_iov: usize,
    pub msg_iovlen: usize,
    pub msg_control: usize,
    pub msg_controllen: usize,
    pub msg_flags: i32,
}

/// Userspace cmsghdr.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct CmsgHdr {
    pub cmsg_len: usize,
    pub cmsg_level: i32,
    pub cmsg_type: i32,
}

/// Credential payload for `SCM_CREDENTIALS`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub struct Ucred {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

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
            // Store the integer so the raw struct bytes match network order.
            sin_addr: u32::from_ne_bytes(ip),
            sin_zero: [0; 8],
        }
    }

    /// Return IPv4 octets in host byte order.
    pub fn ipv4(&self) -> [u8; 4] {
        self.sin_addr.to_ne_bytes()
    }

    /// Return port in host byte order.
    pub fn port(&self) -> u16 {
        u16::from_be(self.sin_port)
    }
}
