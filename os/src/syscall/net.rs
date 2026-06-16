use alloc::{sync::Arc, vec::Vec};
use strum_macros::FromRepr;
use core::{mem::size_of, slice};
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address, Ipv6Address};
use crate::syscall::times::TimeVal;
use crate::fs::{
    make_pipe, AccessMode, File, FileDescription, FileStatusFlags, SocketSpec,
};
use crate::mm::{translated_ref, PageFaultAccess, UserBuffer};
use crate::net::{
    create_alg_socket_file, create_netlink_route_socket_file, create_packet_socket_file,
    create_raw_ipv6_socket_file,
    create_tcp_socket_file, create_udp_socket_file, create_unix_datagram_socket_file,
    create_unix_stream_socket_file, unix_stream_listener,
    AlgRequestFile, AlgSendMsgParams, AlgSocketFile, CompatIfreqSocketFile,
    In6PktInfo, NetlinkRouteSocketFile, PacketSocketFile, RawIpv6ControlMessage,
    RawIpv6SendMeta, RawIpv6SocketFile, SCM_CREDENTIALS, SCM_RIGHTS, SockAddrIn, SockAddrIn6,
    SockAddrLl, SocketLevel, TcpSocketFile, UdpSocketFile, UnixSocketAncillaryData,
    UnixDatagramSocketFile, UnixSocketPairEnd, UnixUcred, AF_ALG, AF_INET6, ALG_OP_DECRYPT, ALG_OP_ENCRYPT,
    ALG_SET_AEAD_ASSOCLEN, ALG_SET_IV, ALG_SET_OP, ICMP6_FILTER, IPPROTO_ICMPV6,
    IPV6_2292DSTOPTS, IPV6_2292HOPOPTS, IPV6_2292HOPLIMIT, IPV6_2292PKTINFO,
    IPV6_2292RTHDR, IPV6_CHECKSUM, IPV6_HOPLIMIT, IPV6_PKTINFO, IPV6_RECVDSTOPTS,
    IPV6_RECVHOPOPTS, IPV6_RECVHOPLIMIT, IPV6_RECVPKTINFO, IPV6_RECVRTHDR,
    IPV6_RECVTCLASS, IPV6_TCLASS, SOCK_SEQPACKET, SOL_ALG, SOL_IPV6,
};
use crate::syscall::{read_pod_from_user, translated_byte_buffer_with_access, write_bytes_to_user, write_pod_to_user, Pod};
use crate::syscall::errno::{ERRNO, OrErrno};
use crate::syscall_body;
use crate::task::{current_process, current_user_token, FdEntry, FdFlags};

const AF_UNIX: i32 = 1;
const AF_INET: u16 = 2;
const AF_NETLINK: i32 = 16;
const AF_PACKET: i32 = 17;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
const SOCK_RAW: i32 = 3;
const SOCK_TYPE_MASK: i32 = 0x0f;
const SOCK_NONBLOCK: i32 = 0x800;
const SOCK_CLOEXEC: i32 = 0x80000;
const SHUT_RD: i32 = 0;
const SHUT_WR: i32 = 1;
const SHUT_RDWR: i32 = 2;
const NETLINK_ROUTE: i32 = 0;
const MSG_PEEK: u32 = 0x0002;
const MSG_DONTWAIT: u32 = 0x0040;
const IPPROTO_TCP: i32 = 6;
const IPPROTO_UDP: i32 = 17;
const IPPROTO_SCTP: i32 = 132;
const IPPROTO_UDPLITE: i32 = 136;
const IPV6_V6ONLY: i32 = 26;

// IP-level (SOL_IP) multicast group membership options. These use a
// `struct group_req { __u32 gr_interface; struct sockaddr_storage gr_group; }`
// payload; on a 64-bit ABI `gr_group` is 8-byte aligned, so it begins at
// offset 8 and the embedded `sockaddr_in` puts the group address at offset 12.
const MCAST_JOIN_GROUP: i32 = 42;
const MCAST_LEAVE_GROUP: i32 = 45;
const GROUP_REQ_FAMILY_OFFSET: usize = 8;
const GROUP_REQ_ADDR_OFFSET: usize = 12;
const GROUP_REQ_MIN_LEN: usize = GROUP_REQ_ADDR_OFFSET + 4;


#[repr(i32)]
#[derive(FromRepr)]
#[allow(clippy::enum_variant_names)]
enum PosixSocketOption {
    SoType = 3,
    SoError = 4,
    SoSndBuf = 7,
    SoRcvBuf = 8,
    SoPassCred = 16,
    SoRecvTimeo = 20,
    SoSndTimeo = 21,
    SoAcceptConn = 30,
    SoAttachBpf = 50,
}

#[repr(i32)]
#[derive(FromRepr)]
enum PosixTcpSocketOption {
    NoDelay = 1,
    MaxSeg = 2,
    Info = 11,
    Congestion = 13,
}

const MSG_CTRUNC: i32 = 0x0008;
const MSG_CMSG_CLOEXEC: u32 = 0x4000_0000;

const MAX_MSG_IOV: usize = 1024;
const MAX_MSG_CONTROL: usize = 16 * 1024;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct LinuxTcpInfo {
    tcpi_state: u8,
    tcpi_ca_state: u8,
    tcpi_retransmits: u8,
    tcpi_probes: u8,
    tcpi_backoff: u8,
    tcpi_options: u8,
    tcpi_snd_rcv_wscale: u8,
    tcpi_delivery_rate_app_limited: u8,
    tcpi_rto: u32,
    tcpi_ato: u32,
    tcpi_snd_mss: u32,
    tcpi_rcv_mss: u32,
    tcpi_unacked: u32,
    tcpi_sacked: u32,
    tcpi_lost: u32,
    tcpi_retrans: u32,
    tcpi_fackets: u32,
    tcpi_last_data_sent: u32,
    tcpi_last_ack_sent: u32,
    tcpi_last_data_recv: u32,
    tcpi_last_ack_recv: u32,
    tcpi_pmtu: u32,
    tcpi_rcv_ssthresh: u32,
    tcpi_rtt: u32,
    tcpi_rttvar: u32,
    tcpi_snd_ssthresh: u32,
    tcpi_snd_cwnd: u32,
    tcpi_advmss: u32,
    tcpi_reordering: u32,
    tcpi_rcv_rtt: u32,
    tcpi_rcv_space: u32,
    tcpi_total_retrans: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MsgHdr {
    pub msg_name: usize,
    pub msg_namelen: usize,
    pub msg_iov: usize,
    pub msg_iovlen: usize,
    pub msg_control: usize,
    pub msg_controllen: usize,
    pub msg_flags: i32,
}

// 允许 socket syscall 将该 C ABI 消息头整体写回用户空间。
impl Pod for MsgHdr {}

// 允许 socket syscall 将 IPv4 地址结构整体写回用户空间。
impl Pod for SockAddrIn {}
impl Pod for SockAddrIn6 {}

// 允许 socket syscall 解析 `struct sockaddr_alg`。
impl Pod for SockAddrAlg {}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct IoVec {
    iov_base: usize,
    iov_len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CmsgHdr {
    cmsg_len: usize,
    cmsg_level: i32,
    cmsg_type: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct SockAddrAlg {
    salg_family: u16,
    salg_type: [u8; 14],
    salg_feat: u32,
    salg_mask: u32,
    salg_name: [u8; 64],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct SockAddrNl {
    nl_family: u16,
    nl_pad: u16,
    nl_pid: u32,
    nl_groups: u32,
}

fn get_file_description(fd: usize) -> Result<Arc<FileDescription>, ERRNO> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return Err(ERRNO::EBADF);
    }
    let desc = inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.desc.clone();
    Ok(desc)
}

fn with_unix_socket<R>(
    fd: usize,
    f: impl FnOnce(&UnixSocketPairEnd) -> Result<R, ERRNO>,
) -> Result<R, ERRNO> {
    let desc = get_file_description(fd)?;
    if let Some(unix) = desc.as_any().downcast_ref::<UnixSocketPairEnd>() {
        return f(unix);
    }
    if desc.as_any().downcast_ref::<UdpSocketFile>().is_some()
        || desc.as_any().downcast_ref::<TcpSocketFile>().is_some()
        || desc.as_any().downcast_ref::<AlgSocketFile>().is_some()
        || desc.as_any().downcast_ref::<AlgRequestFile>().is_some()
    {
        return Err(ERRNO::EOPNOTSUPP);
    }
    Err(ERRNO::ENOTSOCK)
}

fn with_alg_socket<R>(
    fd: usize,
    f: impl FnOnce(&AlgSocketFile) -> Result<R, ERRNO>,
) -> Result<R, ERRNO> {
    let desc = get_file_description(fd)?;
    if let Some(alg) = desc.as_any().downcast_ref::<AlgSocketFile>() {
        return f(alg);
    }
    if desc.as_any().downcast_ref::<AlgRequestFile>().is_some() {
        return Err(ERRNO::EINVAL);
    }
    if desc.as_any().downcast_ref::<UdpSocketFile>().is_some()
        || desc.as_any().downcast_ref::<TcpSocketFile>().is_some()
        || desc.as_any().downcast_ref::<UnixSocketPairEnd>().is_some()
    {
        return Err(ERRNO::EOPNOTSUPP);
    }
    Err(ERRNO::ENOTSOCK)
}

fn with_alg_request<R>(
    fd: usize,
    f: impl FnOnce(&AlgRequestFile) -> Result<R, ERRNO>,
) -> Result<R, ERRNO> {
    let desc = get_file_description(fd)?;
    if let Some(alg) = desc.as_any().downcast_ref::<AlgRequestFile>() {
        return f(alg);
    }
    if desc.as_any().downcast_ref::<AlgSocketFile>().is_some() {
        return Err(ERRNO::EINVAL);
    }
    if desc.as_any().downcast_ref::<UdpSocketFile>().is_some()
        || desc.as_any().downcast_ref::<TcpSocketFile>().is_some()
        || desc.as_any().downcast_ref::<UnixSocketPairEnd>().is_some()
    {
        return Err(ERRNO::EOPNOTSUPP);
    }
    Err(ERRNO::ENOTSOCK)
}

fn copy_user_bytes(_token: usize, ptr: *const u8, len: usize) -> Result<Vec<u8>, ERRNO> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let chunks = translated_byte_buffer_with_access(ptr, len, PageFaultAccess::Read)?;
    let mut out = Vec::new();
    out.try_reserve_exact(len).map_err(|_| ERRNO::ENOMEM)?;
    for chunk in chunks {
        out.extend_from_slice(chunk);
    }
    if out.len() != len {
        return Err(ERRNO::EFAULT);
    }
    Ok(out)
}

fn copy_user_iovecs(_token: usize, iov_ptr: *const IoVec, iovcnt: usize) -> Result<Vec<IoVec>, ERRNO> {
    if iovcnt == 0 {
        return Ok(Vec::new());
    }
    if iovcnt > MAX_MSG_IOV {
        return Err(ERRNO::EINVAL);
    }
    if iov_ptr.is_null() {
        return Err(ERRNO::EFAULT);
    }
    let bytes_len = size_of::<IoVec>()
        .checked_mul(iovcnt)
        .ok_or(ERRNO::EINVAL)?;
    let chunks = translated_byte_buffer_with_access(
        iov_ptr as *const u8,
        bytes_len,
        PageFaultAccess::Read,
    )?;

    let mut iovecs = Vec::new();
    iovecs.try_reserve_exact(iovcnt).map_err(|_| ERRNO::ENOMEM)?;

    let mut scratch = [0u8; size_of::<IoVec>()];
    let mut scratch_len = 0usize;
    for chunk in chunks {
        let mut off = 0usize;
        while off < chunk.len() {
            let copy_len = (size_of::<IoVec>() - scratch_len).min(chunk.len() - off);
            scratch[scratch_len..scratch_len + copy_len]
                .copy_from_slice(&chunk[off..off + copy_len]);
            scratch_len += copy_len;
            off += copy_len;
            if scratch_len == size_of::<IoVec>() {
                let iov = unsafe { core::ptr::read_unaligned(scratch.as_ptr() as *const IoVec) };
                iovecs.push(iov);
                scratch_len = 0;
                if iovecs.len() == iovcnt {
                    break;
                }
            }
        }
        if iovecs.len() == iovcnt {
            break;
        }
    }

    if scratch_len != 0 || iovecs.len() != iovcnt {
        return Err(ERRNO::EFAULT);
    }

    Ok(iovecs)
}

fn iovecs_total_len(iovecs: &[IoVec]) -> Result<usize, ERRNO> {
    let mut total = 0usize;
    for iov in iovecs {
        total = total
            .checked_add(iov.iov_len)
            .ok_or(ERRNO::EINVAL)?;
    }
    Ok(total)
}

fn iovecs_to_user_buffer(
    _token: usize,
    iovecs: &[IoVec],
    access: PageFaultAccess,
) -> Result<UserBuffer, ERRNO> {
    let mut buffers = Vec::new();
    for iov in iovecs {
        if iov.iov_len == 0 {
            continue;
        }
        let mut parts = translated_byte_buffer_with_access(
            iov.iov_base as *const u8,
            iov.iov_len,
            access,
        )?;
        buffers.append(&mut parts);
    }
    Ok(UserBuffer::new(buffers))
}

#[inline]
fn cmsg_align(len: usize) -> usize {
    let align = size_of::<usize>();
    (len + align - 1) & !(align - 1)
}

fn append_cmsg(buf: &mut Vec<u8>, level: i32, ty: i32, payload: &[u8]) {
    let hdr_len = size_of::<CmsgHdr>();
    let cmsg_len = hdr_len + payload.len();
    let cmsg_space = cmsg_align(cmsg_len);
    let start = buf.len();
    buf.resize(start + cmsg_space, 0);

    let hdr = CmsgHdr {
        cmsg_len,
        cmsg_level: level,
        cmsg_type: ty,
    };
    let hdr_bytes = unsafe {
        core::slice::from_raw_parts((&hdr as *const CmsgHdr) as *const u8, hdr_len)
    };
    buf[start..start + hdr_len].copy_from_slice(hdr_bytes);
    buf[start + hdr_len..start + hdr_len + payload.len()].copy_from_slice(payload);
}

fn parse_rights_payload(payload: &[u8]) -> Result<Vec<Arc<FileDescription>>, ERRNO> {
    if !payload.len() % size_of::<i32>() == 0 {
        return Err(ERRNO::EINVAL);
    }

    let process = current_process();
    let inner = process.inner_exclusive_access();
    let mut rights = Vec::new();
    rights
        .try_reserve_exact(payload.len() / size_of::<i32>())
        .map_err(|_| ERRNO::ENOMEM)?;

    for raw in payload.chunks_exact(size_of::<i32>()) {
        let fd = i32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]);
        if fd < 0 {
            return Err(ERRNO::EBADF);
        }
        let fd = fd as usize;
        let desc = inner
            .fd_table
            .get(fd)
            .and_then(|entry| entry.as_ref())
            .ok_or(ERRNO::EBADF)?
            .desc
            .clone();
        rights.push(desc);
    }

    Ok(rights)
}

fn parse_send_ancillary(control_bytes: &[u8]) -> Result<UnixSocketAncillaryData, ERRNO> {
    if control_bytes.len() > MAX_MSG_CONTROL {
        return Err(ERRNO::EINVAL);
    }

    let mut ancillary = UnixSocketAncillaryData::default();
    let mut off = 0usize;
    while off + size_of::<CmsgHdr>() <= control_bytes.len() {
        let hdr = unsafe {
            core::ptr::read_unaligned(control_bytes[off..].as_ptr() as *const CmsgHdr)
        };
        if hdr.cmsg_len < size_of::<CmsgHdr>() {
            return Err(ERRNO::EINVAL);
        }
        let end = off.checked_add(hdr.cmsg_len).ok_or(ERRNO::EINVAL)?;
        if end > control_bytes.len() {
            return Err(ERRNO::EINVAL);
        }

        let payload = &control_bytes[off + size_of::<CmsgHdr>()..end];
        match (SocketLevel::from_repr(hdr.cmsg_level), hdr.cmsg_type) {
            (Some(SocketLevel::SolSocket), SCM_RIGHTS) => {
                ancillary.rights.extend(parse_rights_payload(payload)?);
            }
            (Some(SocketLevel::SolSocket), SCM_CREDENTIALS) => {
                if payload.len() < size_of::<UnixUcred>() {
                    return Err(ERRNO::EINVAL);
                }
                let process = current_process();
                ancillary.credentials = Some(UnixUcred {
                    pid: process.getpid() as i32,
                    uid: process.getuid(),
                    gid: process.getgid(),
                });
            }
            _ => return Err(ERRNO::EOPNOTSUPP),
        }

        off = off.checked_add(cmsg_align(hdr.cmsg_len)).ok_or(ERRNO::EINVAL)?;
    }

    Ok(ancillary)
}

fn parse_raw_ipv6_send_meta(control_bytes: &[u8]) -> Result<RawIpv6SendMeta, ERRNO> {
    if control_bytes.len() > MAX_MSG_CONTROL {
        return Err(ERRNO::EINVAL);
    }

    let mut meta = RawIpv6SendMeta::default();
    let mut off = 0usize;
    while off + size_of::<CmsgHdr>() <= control_bytes.len() {
        let hdr = unsafe {
            core::ptr::read_unaligned(control_bytes[off..].as_ptr() as *const CmsgHdr)
        };
        if hdr.cmsg_len < size_of::<CmsgHdr>() {
            return Err(ERRNO::EINVAL);
        }
        let end = off.checked_add(hdr.cmsg_len).ok_or(ERRNO::EINVAL)?;
        if end > control_bytes.len() {
            return Err(ERRNO::EINVAL);
        }
        let payload = &control_bytes[off + size_of::<CmsgHdr>()..end];
        if hdr.cmsg_level != SOL_IPV6 || payload.len() < size_of::<i32>() {
            return Err(ERRNO::EOPNOTSUPP);
        }
        let value = i32::from_ne_bytes([payload[0], payload[1], payload[2], payload[3]]);
        match hdr.cmsg_type {
            IPV6_HOPLIMIT => meta.hoplimit = Some(value),
            IPV6_TCLASS => meta.tclass = Some(value),
            _ => return Err(ERRNO::EOPNOTSUPP),
        }
        off = off.checked_add(cmsg_align(hdr.cmsg_len)).ok_or(ERRNO::EINVAL)?;
    }
    Ok(meta)
}

fn install_received_rights(rights: Vec<Arc<FileDescription>>, cloexec: bool) -> Result<Vec<i32>, ERRNO> {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let mut out = Vec::with_capacity(rights.len());
    inner.ensure_fd_capacity(rights.len())?;

    for desc in rights {
        let fd = inner.alloc_fd()?;
        let mut entry = FdEntry::new(desc);
        if cloexec {
            entry.flags |= FdFlags::CLOEXEC;
        }
        inner.fd_table[fd] = Some(entry);
        out.push(fd as i32);
    }
    Ok(out)
}

fn with_udp_socket<R>(fd: usize, f: impl FnOnce(&UdpSocketFile) -> Result<R, ERRNO>) -> Result<R, ERRNO> {
    let desc = get_file_description(fd)?;
    let udp = desc
        .as_any()
        .downcast_ref::<UdpSocketFile>()
        .ok_or(ERRNO::ENOTSOCK)?;
    f(udp)
}

fn with_tcp_socket<R>(fd: usize, f: impl FnOnce(&TcpSocketFile) -> Result<R, ERRNO>) -> Result<R, ERRNO> {
    let desc = get_file_description(fd)?;
    let tcp = desc
        .as_any()
        .downcast_ref::<TcpSocketFile>()
        .ok_or(ERRNO::ENOTSOCK)?;
    f(tcp)
}

fn with_netlink_route_socket<R>(
    fd: usize,
    f: impl FnOnce(&NetlinkRouteSocketFile) -> Result<R, ERRNO>,
) -> Result<R, ERRNO> {
    let desc = get_file_description(fd)?;
    let netlink = desc
        .as_any()
        .downcast_ref::<NetlinkRouteSocketFile>()
        .ok_or(ERRNO::ENOTSOCK)?;
    f(netlink)
}

fn with_packet_socket<R>(
    fd: usize,
    f: impl FnOnce(&PacketSocketFile) -> Result<R, ERRNO>,
) -> Result<R, ERRNO> {
    let desc = get_file_description(fd)?;
    let packet = desc
        .as_any()
        .downcast_ref::<PacketSocketFile>()
        .ok_or(ERRNO::ENOTSOCK)?;
    f(packet)
}

fn with_raw_ipv6_socket<R>(
    fd: usize,
    f: impl FnOnce(&RawIpv6SocketFile) -> Result<R, ERRNO>,
) -> Result<R, ERRNO> {
    let desc = get_file_description(fd)?;
    let raw = desc
        .as_any()
        .downcast_ref::<RawIpv6SocketFile>()
        .ok_or(ERRNO::ENOTSOCK)?;
    f(raw)
}

fn sockaddr_to_endpoint(addr: &SockAddrIn) -> Result<IpEndpoint, ERRNO> {
    if addr.sin_family != AF_INET && addr.sin_family != 0 {
        return Err(ERRNO::EAFNOSUPPORT);
    }
    let port = u16::from_be(addr.sin_port);
    // sin_addr is stored in network byte order in user memory. When it is
    // read into a native-endian `u32` field, using `to_ne_bytes` yields the
    // correct sequence of address octets across endiannesses.
    let ip_b = addr.sin_addr.to_ne_bytes();
    let ip = Ipv4Address::new(ip_b[0], ip_b[1], ip_b[2], ip_b[3]);
    Ok(IpEndpoint::new(IpAddress::Ipv4(ip), port))
}

#[inline]
fn unspecified_endpoint_for_family(family: i32) -> IpEndpoint {
    if family == AF_INET6 as i32 {
        IpEndpoint::new(IpAddress::Ipv6(Ipv6Address::UNSPECIFIED), 0)
    } else {
        IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::new(0, 0, 0, 0)), 0)
    }
}

fn endpoint_to_sockaddr(ep: IpEndpoint) -> SockAddrIn {
    let (sin_addr, sin_port) = match ep.addr {
        IpAddress::Ipv4(v4) => {
            let b = v4.octets();
            // Construct the `u32` such that the in-memory bytes of the
            // `SockAddrIn` match the network-order octets expected by C
            // programs. `from_ne_bytes` makes this correct on both little
            // and big endian hosts.
            (u32::from_ne_bytes([b[0], b[1], b[2], b[3]]), ep.port.to_be())
        }
        IpAddress::Ipv6(_) => (u32::from_ne_bytes([0, 0, 0, 0]), ep.port.to_be()),
    };
    SockAddrIn {
        sin_family: AF_INET,
        sin_port,
        sin_addr,
        sin_zero: [0; 8],
    }
}

#[inline]
fn ipv4_mapped_ipv6(v4: Ipv4Address) -> [u8; 16] {
    let octets = v4.octets();
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, octets[0], octets[1], octets[2], octets[3],
    ]
}

#[inline]
fn ipv6_mapped_to_ipv4(bytes: [u8; 16]) -> Option<Ipv4Address> {
    if bytes[..10] == [0; 10] && bytes[10] == 0xff && bytes[11] == 0xff {
        Some(Ipv4Address::new(bytes[12], bytes[13], bytes[14], bytes[15]))
    } else {
        None
    }
}

fn sockaddr_in6_to_endpoint(addr: &SockAddrIn6) -> Result<IpEndpoint, ERRNO> {
    if addr.sin6_family != AF_INET6 {
        return Err(ERRNO::EAFNOSUPPORT);
    }
    if addr.sin6_scope_id != 0 {
        return Err(ERRNO::EOPNOTSUPP);
    }
    let port = u16::from_be(addr.sin6_port);
    if let Some(v4) = ipv6_mapped_to_ipv4(addr.sin6_addr) {
        return Ok(IpEndpoint::new(IpAddress::Ipv4(v4), port));
    }
    let ip = Ipv6Address::from(addr.sin6_addr);
    Ok(IpEndpoint::new(IpAddress::Ipv6(ip), port))
}

fn endpoint_to_sockaddr_in6(ep: IpEndpoint) -> SockAddrIn6 {
    let addr = match ep.addr {
        IpAddress::Ipv4(v4) => ipv4_mapped_ipv6(v4),
        IpAddress::Ipv6(v6) => v6.octets(),
    };
    SockAddrIn6 {
        sin6_family: AF_INET6,
        sin6_port: ep.port.to_be(),
        sin6_addr: addr,
        ..Default::default()
    }
}

fn sockaddr_to_socket_endpoint(
    spec: SocketSpec,
    addr: *const SockAddrIn,
    addrlen: usize,
) -> Result<IpEndpoint, ERRNO> {
    if spec.family == AF_INET6 as i32 {
        let raw = read_sockaddr_in6(addr as *const u8, addrlen)?;
        sockaddr_in6_to_endpoint(&raw)
    } else {
        if addr.is_null() || addrlen < size_of::<SockAddrIn>() {
            return Err(ERRNO::EINVAL);
        }
        let token = current_user_token();
        let uaddr = translated_ref(token, addr).or_errno(ERRNO::EFAULT)?;
        sockaddr_to_endpoint(uaddr)
    }
}

fn copy_endpoint_to_socket_user(
    spec: SocketSpec,
    addr: *mut SockAddrIn,
    addrlen: *mut i32,
    ep: IpEndpoint,
) -> Result<(), ERRNO> {
    if spec.family == AF_INET6 as i32 {
        copy_sockaddr_in6_to_user(addr as *mut u8, addrlen, &endpoint_to_sockaddr_in6(ep))
    } else {
        copy_sockaddr_to_user(addr, addrlen, &endpoint_to_sockaddr(ep))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SocketBackendKind {
    Udp,
    Tcp,
    RawIpv6,
    UnixStream,
    CompatIfreq,
    Packet,
    NetlinkRoute,
    AlgSocket,
    AlgRequest,
    UnixDatagram,
}

fn socket_backend(fd: usize) -> Result<SocketBackendKind, ERRNO> {
    let desc = get_file_description(fd)?;
    if desc.as_any().downcast_ref::<UdpSocketFile>().is_some() {
        return Ok(SocketBackendKind::Udp);
    }
    if desc.as_any().downcast_ref::<TcpSocketFile>().is_some() {
        return Ok(SocketBackendKind::Tcp);
    }
    if desc.as_any().downcast_ref::<RawIpv6SocketFile>().is_some() {
        return Ok(SocketBackendKind::RawIpv6);
    }
    if desc.as_any().downcast_ref::<UnixSocketPairEnd>().is_some() {
        return Ok(SocketBackendKind::UnixStream);
    }
    if desc.as_any().downcast_ref::<UnixDatagramSocketFile>().is_some() {
        return Ok(SocketBackendKind::UnixDatagram);
    }
    if desc.as_any().downcast_ref::<CompatIfreqSocketFile>().is_some() {
        return Ok(SocketBackendKind::CompatIfreq);
    }
    if desc.as_any().downcast_ref::<PacketSocketFile>().is_some() {
        return Ok(SocketBackendKind::Packet);
    }
    if desc.as_any().downcast_ref::<NetlinkRouteSocketFile>().is_some() {
        return Ok(SocketBackendKind::NetlinkRoute);
    }
    if desc.as_any().downcast_ref::<AlgSocketFile>().is_some() {
        return Ok(SocketBackendKind::AlgSocket);
    }
    if desc.as_any().downcast_ref::<AlgRequestFile>().is_some() {
        return Ok(SocketBackendKind::AlgRequest);
    }
    Err(ERRNO::ENOTSOCK)
}

fn with_unix_dgram_socket<R>(
    fd: usize,
    f: impl FnOnce(&UnixDatagramSocketFile) -> Result<R, ERRNO>,
) -> Result<R, ERRNO> {
    let desc = get_file_description(fd)?;
    let sock = desc
        .as_any()
        .downcast_ref::<UnixDatagramSocketFile>()
        .ok_or(ERRNO::ENOTSOCK)?;
    f(sock)
}

fn replace_fd_socket(
    fd: usize,
    file: Arc<dyn File + Send + Sync>,
    spec: SocketSpec,
) -> Result<(), ERRNO> {
    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let entry = inner
        .fd_table
        .get_mut(fd)
        .and_then(|entry| entry.as_mut())
        .ok_or(ERRNO::EBADF)?;
    let status_flags = entry.desc.status_flags();
    let fd_flags = entry.flags;
    entry.desc = Arc::new(FileDescription::new_socket(
        file,
        AccessMode::ReadWrite,
        status_flags,
        0,
        spec,
    ));
    entry.flags = fd_flags;
    Ok(())
}

fn socket_spec(fd: usize) -> Result<SocketSpec, ERRNO> {
    get_file_description(fd)?
        .socket_spec()
        .ok_or(ERRNO::ENOTSOCK)
}

fn parse_sockaddr_alg_field(bytes: &[u8]) -> Result<&str, ERRNO> {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..len]).map_err(|_| ERRNO::EINVAL)
}

fn read_sockaddr_alg(addr: *const u8, addrlen: usize) -> Result<SockAddrAlg, ERRNO> {
    if addr.is_null() || addrlen < size_of::<SockAddrAlg>() {
        return Err(ERRNO::EINVAL);
    }
    read_pod_from_user(addr as *const SockAddrAlg)
}

fn parse_alg_sendmsg(control_bytes: &[u8]) -> Result<AlgSendMsgParams, ERRNO> {
    if control_bytes.len() > MAX_MSG_CONTROL {
        return Err(ERRNO::EINVAL);
    }

    let mut params = AlgSendMsgParams::default();
    let mut off = 0usize;
    while off + size_of::<CmsgHdr>() <= control_bytes.len() {
        let hdr = unsafe {
            core::ptr::read_unaligned(control_bytes[off..].as_ptr() as *const CmsgHdr)
        };
        if hdr.cmsg_len < size_of::<CmsgHdr>() {
            return Err(ERRNO::EINVAL);
        }
        let end = off.checked_add(hdr.cmsg_len).ok_or(ERRNO::EINVAL)?;
        if end > control_bytes.len() {
            return Err(ERRNO::EINVAL);
        }
        if hdr.cmsg_level != SOL_ALG {
            return Err(ERRNO::EOPNOTSUPP);
        }

        let payload = &control_bytes[off + size_of::<CmsgHdr>()..end];
        match hdr.cmsg_type {
            ALG_SET_OP => {
                if payload.len() < size_of::<u32>() {
                    return Err(ERRNO::EINVAL);
                }
                let op = unsafe { core::ptr::read_unaligned(payload.as_ptr() as *const u32) };
                if !matches!(op, ALG_OP_DECRYPT | ALG_OP_ENCRYPT) {
                    return Err(ERRNO::EINVAL);
                }
                params.op = Some(op);
            }
            ALG_SET_IV => {
                if payload.len() < size_of::<u32>() {
                    return Err(ERRNO::EINVAL);
                }
                let iv_len = unsafe { core::ptr::read_unaligned(payload.as_ptr() as *const u32) } as usize;
                if payload.len() < size_of::<u32>() + iv_len {
                    return Err(ERRNO::EINVAL);
                }
                params.iv_len = iv_len;
            }
            ALG_SET_AEAD_ASSOCLEN => {
                if payload.len() < size_of::<u32>() {
                    return Err(ERRNO::EINVAL);
                }
                let assoclen = unsafe { core::ptr::read_unaligned(payload.as_ptr() as *const u32) };
                params.assoclen = Some(assoclen);
            }
            _ => return Err(ERRNO::EOPNOTSUPP),
        }

        off = off.checked_add(cmsg_align(hdr.cmsg_len)).ok_or(ERRNO::EINVAL)?;
    }
    Ok(params)
}

fn read_sockaddr_family(addr: *const u8, addrlen: usize) -> Result<u16, ERRNO> {
    if addr.is_null() || addrlen < size_of::<u16>() {
        return Err(ERRNO::EINVAL);
    }
    let token = current_user_token();
    let family_bytes = copy_user_bytes(token, addr, size_of::<u16>())?;
    Ok(u16::from_ne_bytes([family_bytes[0], family_bytes[1]]))
}

fn read_sockaddr_un_addr(addr: *const u8, addrlen: usize) -> Result<(Vec<u8>, bool), ERRNO> {
    if addr.is_null() || addrlen < size_of::<u16>() {
        return Err(ERRNO::EINVAL);
    }
    let family = read_sockaddr_family(addr, addrlen)?;
    if family != AF_UNIX as u16 {
        return Err(ERRNO::EAFNOSUPPORT);
    }
    let path_len = addrlen.saturating_sub(size_of::<u16>()).min(108);
    if path_len == 0 {
        return Err(ERRNO::EINVAL);
    }
    let token = current_user_token();
    let raw = copy_user_bytes(token, unsafe { addr.add(size_of::<u16>()) }, path_len)?;
    let is_abstract = raw.first().copied() == Some(0);
    let name = if is_abstract {
        raw
    } else {
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        if end == 0 {
            return Err(ERRNO::EINVAL);
        }
        raw[..end].to_vec()
    };
    Ok((name, !is_abstract))
}

fn sockaddr_un_bytes(addr: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(AF_UNIX as u16).to_ne_bytes());
    if addr.is_empty() {
        out.push(0);
        return out;
    }
    out.extend_from_slice(addr);
    if addr.first().copied() != Some(0) {
        out.push(0);
    }
    out
}

fn is_local_bind_addr(spec: SocketSpec, addr: IpAddress) -> bool {
    match (spec.family, addr) {
        (x, IpAddress::Ipv4(v4)) if x == AF_INET as i32 => {
            let octets = v4.octets();
            octets == [0, 0, 0, 0] || octets[0] == 127
        }
        (x, IpAddress::Ipv6(v6)) if x == AF_INET6 as i32 => v6.is_unspecified() || v6.is_loopback(),
        _ => false,
    }
}

fn read_sockaddr_in6(addr: *const u8, addrlen: usize) -> Result<SockAddrIn6, ERRNO> {
    if addr.is_null() || addrlen < size_of::<SockAddrIn6>() {
        return Err(ERRNO::EINVAL);
    }
    read_pod_from_user(addr as *const SockAddrIn6)
}

fn parse_socket_type_flags(socket_type: i32) -> Result<(i32, FileStatusFlags, bool), ERRNO> {
    let extra_flags = socket_type & !(SOCK_TYPE_MASK | SOCK_NONBLOCK | SOCK_CLOEXEC);
    if extra_flags != 0 {
        return Err(ERRNO::EINVAL);
    }
    let status_flags = if (socket_type & SOCK_NONBLOCK) != 0 {
        FileStatusFlags::NONBLOCK
    } else {
        FileStatusFlags::empty()
    };
    let cloexec = (socket_type & SOCK_CLOEXEC) != 0;
    Ok((socket_type & SOCK_TYPE_MASK, status_flags, cloexec))
}

fn read_sockopt_i32(token: usize, optval: *const u8, optlen: i32) -> Result<i32, ERRNO> {
    if optlen < size_of::<i32>() as i32 {
        return Err(ERRNO::EINVAL);
    }
    if optval.is_null() {
        return Err(ERRNO::EFAULT);
    }
    let raw = copy_user_bytes(token, optval, size_of::<i32>())?;
    Ok(i32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

/// Parse a `struct group_req` from userspace and return the IPv4 multicast
/// group address it carries. Validates the length, the address family and that
/// the address is actually a multicast (224.0.0.0/4) address, mirroring the
/// checks Linux performs in `ip_mc_join_group`.
fn parse_group_req(token: usize, optval: *const u8, optlen: i32) -> Result<Ipv4Address, ERRNO> {
    if optlen < GROUP_REQ_MIN_LEN as i32 {
        return Err(ERRNO::EINVAL);
    }
    if optval.is_null() {
        return Err(ERRNO::EFAULT);
    }
    let raw = copy_user_bytes(token, optval, GROUP_REQ_MIN_LEN)?;
    let family = u16::from_ne_bytes([
        raw[GROUP_REQ_FAMILY_OFFSET],
        raw[GROUP_REQ_FAMILY_OFFSET + 1],
    ]);
    if family != AF_INET {
        return Err(ERRNO::EINVAL);
    }
    // sin_addr is stored in network byte order.
    let addr = Ipv4Address::new(
        raw[GROUP_REQ_ADDR_OFFSET],
        raw[GROUP_REQ_ADDR_OFFSET + 1],
        raw[GROUP_REQ_ADDR_OFFSET + 2],
        raw[GROUP_REQ_ADDR_OFFSET + 3],
    );
    // 224.0.0.0/4: the top four bits are 1110.
    if !(224..=239).contains(&addr.octets()[0]) {
        return Err(ERRNO::EINVAL);
    }
    Ok(addr)
}

fn read_sockopt_timeval(optval: *const u8, optlen: i32) -> Result<TimeVal, ERRNO> {
    if optlen < size_of::<TimeVal>() as i32 {
        return Err(ERRNO::EINVAL);
    }
    if optval.is_null() {
        return Err(ERRNO::EFAULT);
    }
    read_pod_from_user(optval as *const TimeVal)
}

fn timeval_to_ns(timeval: &TimeVal) -> Result<u64, ERRNO> {
    if timeval.usec >= 1_000_000 {
        return Err(ERRNO::EINVAL);
    }
    let sec_ns = (timeval.sec as u128) * 1_000_000_000u128;
    let usec_ns = (timeval.usec as u128) * 1_000u128;
    let total = sec_ns.saturating_add(usec_ns);
    if total > u64::MAX as u128 {
        return Err(ERRNO::EINVAL);
    }
    Ok(total as u64)
}

fn timeval_from_ns(ns: u64) -> TimeVal {
    TimeVal {
        sec: (ns / 1_000_000_000) as usize,
        usec: ((ns % 1_000_000_000) / 1_000) as usize,
    }
}

fn write_getsockopt_value(token: usize, optval: *mut u8, optlen: *mut i32, val: &[u8]) -> Result<(), ERRNO> {
    if optlen.is_null() {
        return Err(ERRNO::EFAULT);
    }
    let cap_i32 = *translated_ref(token, optlen as *const i32).or_errno(ERRNO::EFAULT)?;
    if cap_i32 < 0 {
        return Err(ERRNO::EINVAL);
    }
    let cap = cap_i32 as usize;
    let copy_len = core::cmp::min(cap, val.len());
    if copy_len > 0 {
        if optval.is_null() {
            return Err(ERRNO::EFAULT);
        }
        write_bytes_to_user(optval, &val[..copy_len])?;
    }
    write_pod_to_user(optlen, &(val.len() as i32))?;
    Ok(())
}

fn write_getsockopt_i32(token: usize, optval: *mut u8, optlen: *mut i32, v: i32) -> Result<(), ERRNO> {
    write_getsockopt_value(token, optval, optlen, &v.to_ne_bytes())
}

fn copy_sockaddr_to_user(addr: *mut SockAddrIn, addrlen: *mut i32, sockaddr: &SockAddrIn) -> Result<(), ERRNO> {
    if addr.is_null() || addrlen.is_null() {
        return Err(ERRNO::EFAULT);
    }
    let cap = read_pod_from_user(addrlen as *const i32)?;
    if cap < 0 {
        return Err(ERRNO::EINVAL);
    }

    let copy_len = (cap as usize).min(size_of::<SockAddrIn>());
    if copy_len > 0 {
        let src = unsafe {
            core::slice::from_raw_parts((sockaddr as *const SockAddrIn) as *const u8, copy_len)
        };
        write_bytes_to_user(addr as *mut u8, src)?;
    }
    write_pod_to_user(addrlen, &(size_of::<SockAddrIn>() as i32))?;
    Ok(())
}

fn copy_sockaddr_in6_to_user(
    addr: *mut u8,
    addrlen: *mut i32,
    sockaddr: &SockAddrIn6,
) -> Result<(), ERRNO> {
    let bytes = unsafe {
        core::slice::from_raw_parts(
            (sockaddr as *const SockAddrIn6) as *const u8,
            size_of::<SockAddrIn6>(),
        )
    };
    copy_raw_sockaddr_to_user(addr, addrlen, bytes)
}

fn copy_raw_sockaddr_to_user(addr: *mut u8, addrlen: *mut i32, sockaddr: &[u8]) -> Result<(), ERRNO> {
    if addr.is_null() || addrlen.is_null() {
        return Err(ERRNO::EFAULT);
    }
    let cap = read_pod_from_user(addrlen as *const i32)?;
    if cap < 0 {
        return Err(ERRNO::EINVAL);
    }

    let copy_len = (cap as usize).min(sockaddr.len());
    if copy_len > 0 {
        write_bytes_to_user(addr, &sockaddr[..copy_len])?;
    }
    write_pod_to_user(addrlen, &(sockaddr.len() as i32))?;
    Ok(())
}

fn accept_common(
    fd: i32,
    addr: *mut SockAddrIn,
    addrlen: *mut i32,
    flags: i32,
) -> Result<isize, ERRNO> {
    let supported_flags = SOCK_NONBLOCK | SOCK_CLOEXEC;
    if flags & !supported_flags != 0 {
        return Err(ERRNO::EINVAL);
    }

    let fd = fd as usize;
    // O_PATH 描述符不关联可操作的文件对象；套接字系统调用通过
    // `fdget(FMODE_PATH)` 将其视为不存在，故以 EBADF 拒绝（accept03）。
    if get_file_description(fd)?.is_path() {
        return Err(ERRNO::EBADF);
    }
    let parent_spec = socket_spec(fd)?;
    let (accepted_file, peer): (Arc<dyn File + Send + Sync>, _) = match socket_backend(fd)? {
        SocketBackendKind::Tcp => {
            let (accepted, peer) = with_tcp_socket(fd, |tcp| tcp.accept())?;
            (accepted as Arc<dyn File + Send + Sync>, peer)
        }
        SocketBackendKind::UnixStream => {
            let accepted = loop {
                if let Some(socket) = with_unix_socket(fd, |unix| Ok(unix.pop_pending()))? {
                    break socket;
                }
                crate::task::yield_current_and_run_next();
                if crate::signal::has_unmasked_pending_signal() {
                    return Err(ERRNO::EINTR);
                }
            };
            (accepted as Arc<dyn File + Send + Sync>, None)
        }
        SocketBackendKind::AlgSocket => (
            with_alg_socket(fd, |alg| Ok(alg.accept()? as Arc<dyn File + Send + Sync>))?,
            None,
        ),
        SocketBackendKind::Udp
        | SocketBackendKind::RawIpv6
        | SocketBackendKind::UnixDatagram
        | SocketBackendKind::CompatIfreq
        | SocketBackendKind::Packet
        | SocketBackendKind::NetlinkRoute
        | SocketBackendKind::AlgRequest => return Err(ERRNO::EOPNOTSUPP),
    };

    let status_flags = if (flags & SOCK_NONBLOCK) != 0 {
        FileStatusFlags::NONBLOCK
    } else {
        FileStatusFlags::empty()
    };
    let cloexec = (flags & SOCK_CLOEXEC) != 0;

    let accepted_desc = Arc::new(FileDescription::new_socket(
        accepted_file,
        AccessMode::ReadWrite,
        status_flags,
        0,
        parent_spec,
    ));

    let process = current_process();
    let mut inner = process.inner_exclusive_access();
    let new_fd = inner.alloc_fd()?;
    let mut entry = FdEntry::new(accepted_desc);
    if cloexec {
        entry.flags |= FdFlags::CLOEXEC;
    }
    inner.fd_table[new_fd] = Some(entry);
    drop(inner);

    if !addr.is_null() {
        if let Some(ep) = peer {
            copy_endpoint_to_socket_user(parent_spec, addr, addrlen, ep)?;
        } else if addrlen.is_null() {
            return Err(ERRNO::EFAULT);
        } else {
            let len = if parent_spec.family == AF_INET6 as i32 {
                size_of::<SockAddrIn6>()
            } else {
                size_of::<SockAddrIn>()
            };
            write_pod_to_user(addrlen, &(len as i32))?;
        }
    }

    Ok(new_fd as isize)
}

pub fn sys_socket(domain: i32, socket_type: i32, protocol: i32) -> isize {
    syscall_body!({
        let (base_type, status_flags, cloexec) = parse_socket_type_flags(socket_type)?;
        let (file, spec): (Arc<dyn File + Send + Sync>, SocketSpec) = match domain {
            x if x == AF_INET as i32 => match base_type {
                SOCK_DGRAM => (
                    create_udp_socket_file(domain)
                        .map(|f| f as Arc<dyn File + Send + Sync>)
                        .ok_or(ERRNO::ENETDOWN)?,
                    SocketSpec {
                        family: domain,
                        socket_type: SOCK_DGRAM,
                        protocol: 0,
                    },
                ),
                SOCK_STREAM => (
                    create_tcp_socket_file(domain)
                        .map(|f| f as Arc<dyn File + Send + Sync>)
                        .ok_or(ERRNO::ENETDOWN)?,
                    SocketSpec {
                        family: domain,
                        socket_type: SOCK_STREAM,
                        protocol: 0,
                    },
                ),
                _ => return Err(ERRNO::ESOCKTNOSUPPORT),
            },
            x if x == AF_INET6 as i32 => match base_type {
                SOCK_DGRAM => {
                    if protocol != 0 && protocol != IPPROTO_UDP && protocol != IPPROTO_UDPLITE {
                        return Err(ERRNO::EPROTONOSUPPORT);
                    }
                    (
                        create_udp_socket_file(domain)
                            .map(|f| f as Arc<dyn File + Send + Sync>)
                            .ok_or(ERRNO::ENETDOWN)?,
                        SocketSpec {
                            family: domain,
                            socket_type: SOCK_DGRAM,
                            protocol,
                        },
                    )
                }
                SOCK_STREAM => {
                    if protocol != 0 && protocol != IPPROTO_TCP && protocol != IPPROTO_SCTP {
                        return Err(ERRNO::EPROTONOSUPPORT);
                    }
                    (
                        create_tcp_socket_file(domain)
                            .map(|f| f as Arc<dyn File + Send + Sync>)
                            .ok_or(ERRNO::ENETDOWN)?,
                        SocketSpec {
                            family: domain,
                            socket_type: SOCK_STREAM,
                            protocol,
                        },
                    )
                }
                SOCK_RAW => {
                    if !(0..=255).contains(&protocol) {
                        return Err(ERRNO::EPROTONOSUPPORT);
                    }
                    (
                        create_raw_ipv6_socket_file(protocol) as Arc<dyn File + Send + Sync>,
                        SocketSpec {
                            family: domain,
                            socket_type: SOCK_RAW,
                            protocol,
                        },
                    )
                }
                _ => return Err(ERRNO::ESOCKTNOSUPPORT),
            },
            AF_UNIX => {
                if protocol != 0 {
                    return Err(ERRNO::EPROTONOSUPPORT);
                }
                match base_type {
                    SOCK_STREAM | SOCK_SEQPACKET => (
                        create_unix_stream_socket_file() as Arc<dyn File + Send + Sync>,
                        SocketSpec {
                            family: domain,
                            socket_type: base_type,
                            protocol: 0,
                        },
                    ),
                    SOCK_DGRAM => (
                        create_unix_datagram_socket_file() as Arc<dyn File + Send + Sync>,
                        SocketSpec {
                            family: domain,
                            socket_type: SOCK_DGRAM,
                            protocol: 0,
                        },
                    ),
                    _ => return Err(ERRNO::ESOCKTNOSUPPORT),
                }
            }
            AF_ALG => {
                if protocol != 0 {
                    return Err(ERRNO::EPROTONOSUPPORT);
                }
                if base_type != SOCK_SEQPACKET {
                    return Err(ERRNO::ESOCKTNOSUPPORT);
                }
                (
                    create_alg_socket_file() as Arc<dyn File + Send + Sync>,
                    SocketSpec {
                        family: domain,
                        socket_type: SOCK_SEQPACKET,
                        protocol: 0,
                    },
                )
            }
            AF_PACKET => {
                if base_type != SOCK_RAW && base_type != SOCK_DGRAM {
                    return Err(ERRNO::ESOCKTNOSUPPORT);
                }
                (
                    create_packet_socket_file(base_type) as Arc<dyn File + Send + Sync>,
                    SocketSpec {
                        family: domain,
                        socket_type: base_type,
                        protocol,
                    },
                )
            }
            AF_NETLINK => {
                if base_type != SOCK_RAW && base_type != SOCK_DGRAM {
                    return Err(ERRNO::ESOCKTNOSUPPORT);
                }
                if protocol != NETLINK_ROUTE {
                    return Err(ERRNO::EPROTONOSUPPORT);
                }
                (
                    create_netlink_route_socket_file() as Arc<dyn File + Send + Sync>,
                    SocketSpec {
                        family: domain,
                        socket_type: base_type,
                        protocol,
                    },
                )
            }
            _ => return Err(ERRNO::EAFNOSUPPORT),
        };

        let desc = Arc::new(FileDescription::new_socket(
            file,
            AccessMode::ReadWrite,
            status_flags,
            0,
            spec,
        ));

        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        let fd = inner.alloc_fd()?;
        let mut entry = FdEntry::new(desc);
        if cloexec {
            entry.flags |= FdFlags::CLOEXEC;
        }
        inner.fd_table[fd] = Some(entry);
        Ok(fd as isize)
    })
}

pub fn sys_socketpair(domain: i32, socket_type: i32, protocol: i32, sv: *mut i32) -> isize {
    syscall_body!({
        if sv.is_null() {
            return Err(ERRNO::EFAULT);
        }
        if domain != AF_UNIX {
            return Err(ERRNO::EAFNOSUPPORT);
        }
        if protocol != 0 {
            return Err(ERRNO::EPROTONOSUPPORT);
        }

        let (base_type, status_flags, cloexec) = parse_socket_type_flags(socket_type)?;
        if base_type != SOCK_STREAM && base_type != SOCK_DGRAM {
            return Err(ERRNO::ESOCKTNOSUPPORT);
        }

        let (ab_read, ab_write) = make_pipe();
        let (ba_read, ba_write) = make_pipe();

        let (end0_raw, end1_raw) = UnixSocketPairEnd::new_pair(ba_read, ab_write, ab_read, ba_write);
        let end0: Arc<dyn File + Send + Sync> = end0_raw;
        let end1: Arc<dyn File + Send + Sync> = end1_raw;
        let spec = SocketSpec {
            family: domain,
            socket_type: base_type,
            protocol: 0,
        };

        let desc0 = Arc::new(FileDescription::new_socket(
            end0,
            AccessMode::ReadWrite,
            status_flags,
            0,
            spec,
        ));
        let desc1 = Arc::new(FileDescription::new_socket(
            end1,
            AccessMode::ReadWrite,
            status_flags,
            0,
            spec,
        ));

        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        inner.ensure_fd_capacity(2)?;
        let fd0 = inner.alloc_fd()?;
        let mut entry0 = FdEntry::new(desc0);
        if cloexec {
            entry0.flags |= FdFlags::CLOEXEC;
        }
        inner.fd_table[fd0] = Some(entry0);

        let fd1 = inner.alloc_fd()?;
        let mut entry1 = FdEntry::new(desc1);
        if cloexec {
            entry1.flags |= FdFlags::CLOEXEC;
        }
        inner.fd_table[fd1] = Some(entry1);
        drop(inner);

        write_pod_to_user(sv, &(fd0 as i32))?;
        write_pod_to_user(unsafe { sv.add(1) }, &(fd1 as i32))?;
        Ok(0)
    })
}

pub fn sys_bind(fd: i32, addr: *const SockAddrIn, addrlen: i32) -> isize {
    syscall_body!({
        if addrlen < 0 {
            return Err(ERRNO::EINVAL);
        }
        let fd = fd as usize;
        match socket_backend(fd)? {
            SocketBackendKind::Udp | SocketBackendKind::Tcp => {
                if addr.is_null() {
                    return Err(ERRNO::EINVAL);
                }
                let spec = socket_spec(fd)?;
                let ep = sockaddr_to_socket_endpoint(spec, addr, addrlen as usize)?;
                if ep.port < 1024 && ep.port != 0 && current_process().geteuid() != 0 {
                    return Err(ERRNO::EACCES);
                }
                if !is_local_bind_addr(spec, ep.addr) {
                    return Err(ERRNO::EADDRNOTAVAIL);
                }
                match socket_backend(fd)? {
                    SocketBackendKind::Udp => with_udp_socket(fd, |udp| udp.bind(ep))?,
                    SocketBackendKind::Tcp => with_tcp_socket(fd, |tcp| tcp.bind(ep))?,
                    _ => unreachable!(),
                }
            }
            SocketBackendKind::RawIpv6 => {
                let raw = read_sockaddr_in6(addr as *const u8, addrlen as usize)?;
                with_raw_ipv6_socket(fd, |socket| socket.bind(&raw))?;
            }
            SocketBackendKind::AlgSocket => {
                let raw = read_sockaddr_alg(addr as *const u8, addrlen as usize)?;
                if raw.salg_family as i32 != AF_ALG {
                    return Err(ERRNO::EAFNOSUPPORT);
                }
                let algtype = parse_sockaddr_alg_field(raw.salg_type.as_slice())?;
                let algname = parse_sockaddr_alg_field(raw.salg_name.as_slice())?;
                with_alg_socket(fd, |alg| alg.bind(algtype, algname))?;
            }
            SocketBackendKind::NetlinkRoute | SocketBackendKind::Packet => {
                if addr.is_null() || addrlen < 0 {
                    return Err(ERRNO::EINVAL);
                }
                if matches!(socket_backend(fd)?, SocketBackendKind::Packet) {
                    let token = current_user_token();
                    let raw = copy_user_bytes(token, addr as *const u8, addrlen as usize)?;
                    with_packet_socket(fd, |packet| packet.bind_raw(raw.as_slice()))?;
                }
            }
            SocketBackendKind::UnixStream => {
                let (addr, create_path) = read_sockaddr_un_addr(addr as *const u8, addrlen as usize)?;
                with_unix_socket(fd, |unix| unix.bind_addr(addr, create_path))?;
            }
            SocketBackendKind::UnixDatagram => {
                let (addr, create_path) = read_sockaddr_un_addr(addr as *const u8, addrlen as usize)?;
                with_unix_dgram_socket(fd, |unix| unix.bind_addr(addr, create_path))?;
            }
            SocketBackendKind::CompatIfreq | SocketBackendKind::AlgRequest => return Err(ERRNO::ENOTSOCK),
        }
        Ok(0)
    })
}

pub fn sys_connect(fd: i32, addr: *const SockAddrIn, addrlen: i32) -> isize {
    syscall_body!({
        if addrlen < 0 {
            return Err(ERRNO::EINVAL);
        }
        let addrlen = addrlen as usize;
        if addr.is_null() || addrlen < size_of::<u16>() {
            return Err(ERRNO::EINVAL);
        }

        let fd = fd as usize;
        match socket_backend(fd)? {
            SocketBackendKind::Udp | SocketBackendKind::Tcp => {
                let spec = socket_spec(fd)?;
                let ep = sockaddr_to_socket_endpoint(spec, addr, addrlen)?;
                match socket_backend(fd)? {
                    SocketBackendKind::Udp => with_udp_socket(fd, |udp| udp.connect(ep))?,
                    SocketBackendKind::Tcp => with_tcp_socket(fd, |tcp| tcp.connect(ep))?,
                    SocketBackendKind::RawIpv6 | SocketBackendKind::UnixStream | SocketBackendKind::UnixDatagram | SocketBackendKind::CompatIfreq | SocketBackendKind::Packet | SocketBackendKind::NetlinkRoute | SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => unreachable!(),
                }
            }
            SocketBackendKind::UnixStream => {
                let family = read_sockaddr_family(addr as *const u8, addrlen)?;
                if family != AF_UNIX as u16 {
                    return Err(ERRNO::EAFNOSUPPORT);
                }
                let (unix_addr, _) = read_sockaddr_un_addr(addr as *const u8, addrlen)?;
                let listener = unix_stream_listener(&unix_addr).ok_or(ERRNO::ENOENT)?;
                let (client, server) = {
                    let (ab_read, ab_write) = make_pipe();
                    let (ba_read, ba_write) = make_pipe();
                    UnixSocketPairEnd::new_pair(ba_read, ab_write, ab_read, ba_write)
                };
                listener.push_pending(server)?;
                let spec = socket_spec(fd)?;
                replace_fd_socket(fd, client as Arc<dyn File + Send + Sync>, spec)?;
            }
            SocketBackendKind::UnixDatagram => {
                let (unix_addr, _) = read_sockaddr_un_addr(addr as *const u8, addrlen)?;
                with_unix_dgram_socket(fd, |socket| socket.connect_addr(unix_addr))?;
            }
            SocketBackendKind::RawIpv6 | SocketBackendKind::CompatIfreq | SocketBackendKind::Packet | SocketBackendKind::NetlinkRoute | SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => return Err(ERRNO::EOPNOTSUPP),
        }
        Ok(0)
    })
}

pub fn sys_listen(fd: i32, backlog: i32) -> isize {
    syscall_body!({
        let fd = fd as usize;
        match socket_backend(fd)? {
            SocketBackendKind::Tcp => {
                with_tcp_socket(fd, |tcp| tcp.listen(backlog as usize))?;
                Ok(0)
            }
            SocketBackendKind::UnixStream => {
                with_unix_socket(fd, |unix| unix.listen())?;
                Ok(0)
            }
            SocketBackendKind::Udp | SocketBackendKind::RawIpv6 | SocketBackendKind::UnixDatagram | SocketBackendKind::CompatIfreq | SocketBackendKind::Packet | SocketBackendKind::NetlinkRoute => Err(ERRNO::ENOTSOCK),
            SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => Err(ERRNO::EOPNOTSUPP),
        }
    })
}

pub fn sys_accept(fd: i32, addr: *mut SockAddrIn, addrlen: *mut i32) -> isize {
    syscall_body!({
        accept_common(fd, addr, addrlen, 0)
    })
}

pub fn sys_accept4(fd: i32, addr: *mut SockAddrIn, addrlen: *mut i32, flags: i32) -> isize {
    syscall_body!({
        accept_common(fd, addr, addrlen, flags)
    })
}

pub fn sys_getsockname(fd: i32, addr: *mut SockAddrIn, addrlen: *mut i32) -> isize {
    syscall_body!({
        let fd = fd as usize;

        match socket_backend(fd)? {
            SocketBackendKind::Udp => {
                with_udp_socket(fd, |udp| {
                    let ep = udp
                        .local_endpoint()
                        .unwrap_or(unspecified_endpoint_for_family(socket_spec(fd)?.family));
                    copy_endpoint_to_socket_user(socket_spec(fd)?, addr, addrlen, ep)?;
                    Ok(())
                })?;
            }
            SocketBackendKind::Tcp => {
                with_tcp_socket(fd, |tcp| {
                    let ep = tcp
                        .local_endpoint()
                        .unwrap_or(unspecified_endpoint_for_family(socket_spec(fd)?.family));
                    copy_endpoint_to_socket_user(socket_spec(fd)?, addr, addrlen, ep)?;
                    Ok(())
                })?;
            }
            SocketBackendKind::RawIpv6 => {
                let sockaddr = with_raw_ipv6_socket(fd, |raw| Ok(raw.local_addr()))?;
                copy_sockaddr_in6_to_user(addr as *mut u8, addrlen, &sockaddr)?;
            }
            SocketBackendKind::Packet => {
                let sockaddr = with_packet_socket(fd, |packet| packet.getsockname_raw())?;
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&sockaddr as *const SockAddrLl) as *const u8,
                        size_of::<SockAddrLl>(),
                    )
                };
                copy_raw_sockaddr_to_user(addr as *mut u8, addrlen, bytes)?;
            }
            SocketBackendKind::NetlinkRoute => {
                let sockaddr = SockAddrNl {
                    nl_family: AF_NETLINK as u16,
                    nl_pad: 0,
                    nl_pid: 0,
                    nl_groups: 0,
                };
                let bytes = unsafe {
                    core::slice::from_raw_parts(
                        (&sockaddr as *const SockAddrNl) as *const u8,
                        size_of::<SockAddrNl>(),
                    )
                };
                copy_raw_sockaddr_to_user(addr as *mut u8, addrlen, bytes)?;
            }
            SocketBackendKind::UnixStream => {
                let unix_addr = with_unix_socket(fd, |unix| Ok(unix.bound_addr()))?.unwrap_or_default();
                let sockaddr = sockaddr_un_bytes(unix_addr.as_slice());
                copy_raw_sockaddr_to_user(addr as *mut u8, addrlen, sockaddr.as_slice())?;
            }
            SocketBackendKind::UnixDatagram => {
                let unix_addr = with_unix_dgram_socket(fd, |unix| Ok(unix.bound_addr()))?.unwrap_or_default();
                let sockaddr = sockaddr_un_bytes(unix_addr.as_slice());
                copy_raw_sockaddr_to_user(addr as *mut u8, addrlen, sockaddr.as_slice())?;
            }
            SocketBackendKind::CompatIfreq => return Err(ERRNO::ENOTSOCK),
            SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => return Err(ERRNO::EOPNOTSUPP),
        }

        Ok(0)
    })
}

pub fn sys_getpeername(fd: i32, addr: *mut SockAddrIn, addrlen: *mut i32) -> isize {
    syscall_body!({
        let fd = fd as usize;

        match socket_backend(fd)? {
            SocketBackendKind::Udp => {
                with_udp_socket(fd, |udp| {
                    let ep = udp.peer_endpoint().ok_or(ERRNO::ENOTCONN)?;
                    copy_endpoint_to_socket_user(socket_spec(fd)?, addr, addrlen, ep)?;
                    Ok(())
                })?;
            }
            SocketBackendKind::Tcp => {
                with_tcp_socket(fd, |tcp| {
                    if let Some(ep) = tcp.remote_endpoint() {
                        copy_endpoint_to_socket_user(socket_spec(fd)?, addr, addrlen, ep)?;
                        Ok(())
                    } else {
                        Err(ERRNO::ENOTCONN)
                    }
                })?;
            }
            SocketBackendKind::RawIpv6 | SocketBackendKind::UnixStream | SocketBackendKind::UnixDatagram | SocketBackendKind::CompatIfreq | SocketBackendKind::Packet | SocketBackendKind::NetlinkRoute => return Err(ERRNO::ENOTSOCK),
            SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => return Err(ERRNO::EOPNOTSUPP),
        }

        Ok(0)
    })
}

pub fn sys_sendto(
    fd: i32,
    buf: *const u8,
    len: usize,
    flags: u32,
    addr: *const SockAddrIn,
    addrlen: i32,
) -> isize {
    syscall_body!({
        if flags != 0 {
            return Err(ERRNO::EOPNOTSUPP);
        }
        if len == 0 {
            return Ok(0);
        }
        if buf.is_null() {
            return Err(ERRNO::EFAULT);
        }

        let token = current_user_token();
        let ubuf = UserBuffer::new(
            translated_byte_buffer_with_access(buf, len, PageFaultAccess::Read)?,
        );

        let fd = fd as usize;
        let n = match socket_backend(fd)? {
            SocketBackendKind::Udp => {
                if addr.is_null() {
                    with_udp_socket(fd, |udp| udp.send_user_buffer(&ubuf))?
                } else {
                    if addrlen < 0 {
                        return Err(ERRNO::EINVAL);
                    }
                    let ep = sockaddr_to_socket_endpoint(socket_spec(fd)?, addr, addrlen as usize)?;
                    with_udp_socket(fd, |udp| udp.send_user_buffer_to(&ubuf, ep))?
                }
            }
            SocketBackendKind::Tcp => {
                if addr.is_null() {
                    with_tcp_socket(fd, |tcp| tcp.send_from_user_buffer(&ubuf))?
                } else {
                    return Err(ERRNO::ENOTSOCK);
                }
            }
            SocketBackendKind::RawIpv6 => {
                if addr.is_null() {
                    return Err(ERRNO::EDESTADDRREQ);
                }
                let sockaddr = read_sockaddr_in6(addr as *const u8, addrlen as usize)?;
                with_raw_ipv6_socket(fd, |raw| {
                    raw.send_user_buffer_to(&ubuf, &sockaddr, &RawIpv6SendMeta::default())
                })?
            }
            SocketBackendKind::NetlinkRoute => {
                if !addr.is_null() {
                    return Err(ERRNO::EOPNOTSUPP);
                }
                with_netlink_route_socket(fd, |netlink| netlink.send_user_buffer(&ubuf))?
            }
            SocketBackendKind::Packet => {
                if addrlen < 0 {
                    return Err(ERRNO::EINVAL);
                }
                let raw_addr = if addr.is_null() {
                    None
                } else {
                    Some(copy_user_bytes(token, addr as *const u8, addrlen as usize)?)
                };
                with_packet_socket(fd, |packet| {
                    packet.send_user_buffer_to(&ubuf, raw_addr.as_deref())
                })?
            }
            SocketBackendKind::UnixDatagram => {
                let mut data = Vec::new();
                for byte_ref in ubuf.into_iter() {
                    data.push(unsafe { *byte_ref });
                }
                let unix_addr = if addr.is_null() {
                    None
                } else {
                    Some(read_sockaddr_un_addr(addr as *const u8, addrlen as usize)?.0)
                };
                with_unix_dgram_socket(fd, |socket| socket.send_to(data.as_slice(), unix_addr))?
            }
            SocketBackendKind::UnixStream | SocketBackendKind::CompatIfreq => return Err(ERRNO::ENOTSOCK),
            SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => return Err(ERRNO::EOPNOTSUPP),
        };

        Ok(n as isize)
    })
}

pub fn sys_recvfrom(
    fd: i32,
    buf: *mut u8,
    len: usize,
    flags: u32,
    addr: *mut SockAddrIn,
    addrlen: *mut i32,
) -> isize {
    syscall_body!({
        if len == 0 {
            return Ok(0);
        }
        if buf.is_null() {
            return Err(ERRNO::EFAULT);
        }
        if !addr.is_null() && addrlen.is_null() {
            return Err(ERRNO::EFAULT);
        }

        let mut ubuf = UserBuffer::new(
            translated_byte_buffer_with_access(buf as *const u8, len, PageFaultAccess::Write)?,
        );

        let fd = fd as usize;
        let backend = socket_backend(fd)?;
        let allowed_flags = match backend {
            SocketBackendKind::NetlinkRoute => MSG_PEEK | MSG_DONTWAIT,
            _ => 0,
        };
        if flags & !allowed_flags != 0 {
            return Err(ERRNO::EOPNOTSUPP);
        }

        let (n, ep) = match backend {
            SocketBackendKind::Udp => with_udp_socket(fd, |udp| udp.recv_from_user_buffer(&mut ubuf))?,
            SocketBackendKind::Tcp => {
                let n = with_tcp_socket(fd, |tcp| tcp.recv_into_user_buffer(&mut ubuf))?;
                let ep = if addr.is_null() {
                    unspecified_endpoint_for_family(socket_spec(fd)?.family)
                } else {
                    with_tcp_socket(fd, |tcp| Ok(tcp.remote_endpoint()))?
                        .ok_or(ERRNO::ENOTCONN)?
                };
                (n, ep)
            }
            SocketBackendKind::NetlinkRoute => (
                with_netlink_route_socket(fd, |netlink| {
                    netlink.recv_into_user_buffer(&mut ubuf, (flags & MSG_PEEK) != 0)
                })?,
                unspecified_endpoint_for_family(AF_INET as i32),
            ),
            SocketBackendKind::Packet => {
                let (n, sockaddr) =
                    with_packet_socket(fd, |packet| packet.recv_into_user_buffer(&mut ubuf))?;
                if !addr.is_null() {
                    let bytes = unsafe {
                        core::slice::from_raw_parts(
                            (&sockaddr as *const SockAddrLl) as *const u8,
                            size_of::<SockAddrLl>(),
                        )
                    };
                    copy_raw_sockaddr_to_user(addr as *mut u8, addrlen, bytes)?;
                }
                return Ok(n as isize);
            }
            SocketBackendKind::RawIpv6 => {
                let packet = with_raw_ipv6_socket(fd, |raw| raw.recv_into_user_buffer(&mut ubuf))?;
                if !addr.is_null() {
                    let sockaddr = SockAddrIn6 {
                        sin6_family: AF_INET6,
                        sin6_addr: packet
                            .control
                            .iter()
                            .find(|cmsg| cmsg.cmsg_type == IPV6_PKTINFO || cmsg.cmsg_type == IPV6_2292PKTINFO)
                            .and_then(|cmsg| {
                                if cmsg.data.len() < size_of::<In6PktInfo>() {
                                    return None;
                                }
                                let pktinfo = unsafe {
                                    core::ptr::read_unaligned(cmsg.data.as_ptr() as *const In6PktInfo)
                                };
                                Some(pktinfo.ipi6_addr)
                            })
                            .unwrap_or([0; 16]),
                        ..Default::default()
                    };
                    copy_sockaddr_in6_to_user(addr as *mut u8, addrlen, &sockaddr)?;
                }
                return Ok(packet.data.len() as isize);
            }
            SocketBackendKind::UnixDatagram => {
                let (n, from) = with_unix_dgram_socket(fd, |socket| socket.recv_from(ubuf))?;
                if !addr.is_null() {
                    let sockaddr = sockaddr_un_bytes(from.as_deref().unwrap_or(&[]));
                    copy_raw_sockaddr_to_user(addr as *mut u8, addrlen, sockaddr.as_slice())?;
                }
                return Ok(n as isize);
            }
            SocketBackendKind::UnixStream | SocketBackendKind::CompatIfreq => return Err(ERRNO::ENOTSOCK),
            SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => return Err(ERRNO::EOPNOTSUPP),
        };

        if !addr.is_null() {
            copy_endpoint_to_socket_user(socket_spec(fd)?, addr, addrlen, ep)?;
        }

        Ok(n as isize)
    })
}

pub fn sys_shutdown(fd: i32, how: i32) -> isize {
    syscall_body!({
        if !matches!(how, SHUT_RD | SHUT_WR | SHUT_RDWR) {
            return Err(ERRNO::EINVAL);
        }
        let fd = fd as usize;
        match socket_backend(fd)? {
            SocketBackendKind::UnixStream => {
                with_unix_socket(fd, |unix| {
                    unix.shutdown(how)?;
                    Ok(())
                })?;
                Ok(0)
            }
            SocketBackendKind::Tcp => {
                with_tcp_socket(fd, |tcp| tcp.shutdown(how))?;
                Ok(0)
            }
            SocketBackendKind::Udp | SocketBackendKind::RawIpv6 | SocketBackendKind::UnixDatagram | SocketBackendKind::CompatIfreq | SocketBackendKind::Packet | SocketBackendKind::NetlinkRoute => Err(ERRNO::ENOTSOCK),
            SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => Err(ERRNO::EOPNOTSUPP),
        }
    })
}

#[allow(unused_variables)]
pub fn sys_setsockopt(fd: i32, level: i32, optname: i32, optval: *const u8, optlen: i32) -> isize {
    syscall_body!({
        let fd = fd as usize;
        let backend = socket_backend(fd)?;
        let spec = socket_spec(fd)?;

        if level == SOL_ALG {
            if optval.is_null() || optlen < 0 {
                return Err(ERRNO::EINVAL);
            }
            let token = current_user_token();
            let payload = copy_user_bytes(token, optval, optlen as usize)?;
            return match backend {
                SocketBackendKind::AlgSocket => {
                    with_alg_socket(fd, |alg| alg.set_key(optname, payload.as_slice()))?;
                    Ok(0)
                }
                SocketBackendKind::AlgRequest => Err(ERRNO::EINVAL),
                _ => Err(ERRNO::ENOPROTOOPT),
            };
        }

        if level == IPPROTO_ICMPV6 {
            if spec.family != AF_INET6 as i32 || spec.socket_type != SOCK_RAW || spec.protocol != IPPROTO_ICMPV6 {
                return Err(ERRNO::ENOPROTOOPT);
            }
            if optval.is_null() || optlen < 0 {
                return Err(ERRNO::EINVAL);
            }
            let token = current_user_token();
            let payload = copy_user_bytes(token, optval, optlen as usize)?;
            return match optname {
                ICMP6_FILTER => {
                    with_raw_ipv6_socket(fd, |raw| raw.set_icmp6_filter(payload.as_slice()))?;
                    Ok(0)
                }
                _ => Err(ERRNO::ENOPROTOOPT),
            };
        }

        match SocketLevel::from_repr(level) {
            Some(SocketLevel::SolSocket) => match PosixSocketOption::from_repr(optname) {
                Some(PosixSocketOption::SoPassCred) => {
                    if spec.family != AF_UNIX {
                        warn!(
                            "setsockopt(fd={}, level={}, optname={}) unsupported on non-UNIX socket, ignored",
                            fd, level, optname
                        );
                        return Ok(0);
                    }
                    let token = current_user_token();
                    let enabled = read_sockopt_i32(token, optval, optlen)? != 0;
                    with_unix_socket(fd, |unix| {
                        unix.set_passcred(enabled);
                        Ok(())
                    })?;
                    Ok(0)
                }
                Some(PosixSocketOption::SoAttachBpf) => {
                    if spec.family != AF_UNIX {
                        return Err(ERRNO::ENOPROTOOPT);
                    }
                    if optval.is_null() || optlen < size_of::<i32>() as i32 {
                        return Err(ERRNO::EINVAL);
                    }
                    let prog_fd = read_pod_from_user(optval as *const i32)?;
                    if prog_fd < 0 {
                        return Err(ERRNO::EBADF);
                    }
                    crate::syscall::bpf_prog_is_socket_filter(prog_fd as u32)?;
                    with_unix_socket(fd, |unix| {
                        unix.attach_bpf_prog_fd(prog_fd as u32);
                        Ok(())
                    })?;
                    Ok(0)
                }
                Some(PosixSocketOption::SoRecvTimeo) => {
                    let timeval = read_sockopt_timeval(optval, optlen)?;
                    let timeout_ns = timeval_to_ns(&timeval)?;
                    match backend {
                        SocketBackendKind::Udp => with_udp_socket(fd, |udp| {
                            udp.set_recv_timeout_ns(timeout_ns);
                            Ok(())
                        })?,
                        SocketBackendKind::Tcp => with_tcp_socket(fd, |tcp| {
                            tcp.set_recv_timeout_ns(timeout_ns);
                            Ok(())
                        })?,
                        SocketBackendKind::UnixStream => {
                            warn!(
                                "setsockopt(fd={}, level={}, optname={}) unsupported on UNIX socket, ignored",
                                fd,
                                level,
                                optname
                            );
                        }
                        SocketBackendKind::RawIpv6 | SocketBackendKind::UnixDatagram | SocketBackendKind::CompatIfreq | SocketBackendKind::Packet | SocketBackendKind::NetlinkRoute | SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => return Err(ERRNO::EOPNOTSUPP),
                    }
                    Ok(0)
                }
                Some(PosixSocketOption::SoSndTimeo) => {
                    let timeval = read_sockopt_timeval(optval, optlen)?;
                    let timeout_ns = timeval_to_ns(&timeval)?;
                    match backend {
                        SocketBackendKind::Udp => with_udp_socket(fd, |udp| {
                            udp.set_send_timeout_ns(timeout_ns);
                            Ok(())
                        })?,
                        SocketBackendKind::Tcp => with_tcp_socket(fd, |tcp| {
                            tcp.set_send_timeout_ns(timeout_ns);
                            Ok(())
                        })?,
                        SocketBackendKind::UnixStream => {
                            warn!(
                                "setsockopt(fd={}, level={}, optname={}) unsupported on UNIX socket, ignored",
                                fd,
                                level,
                                optname
                            );
                        }
                        SocketBackendKind::RawIpv6 | SocketBackendKind::UnixDatagram | SocketBackendKind::CompatIfreq | SocketBackendKind::Packet | SocketBackendKind::NetlinkRoute | SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => return Err(ERRNO::EOPNOTSUPP),
                    }
                    Ok(0)
                }
                _ => {
                    warn!("setsockopt(fd={}, level={}, optname={}) not implemented for SOL_SOCKET, ignored", fd, level, optname);
                    Ok(0)
                }
            },
            Some(SocketLevel::IpProtoIp) => match optname {
                MCAST_JOIN_GROUP => {
                    if backend != SocketBackendKind::Tcp {
                        // Multicast membership is only modelled for TCP sockets;
                        // accept silently elsewhere to preserve prior behaviour.
                        return Ok(0);
                    }
                    let token = current_user_token();
                    let group = parse_group_req(token, optval, optlen)?;
                    let joined = with_tcp_socket(fd, |tcp| Ok(tcp.join_mcast_group(group)))?;
                    if joined {
                        Ok(0)
                    } else {
                        Err(ERRNO::EADDRINUSE)
                    }
                }
                MCAST_LEAVE_GROUP => {
                    if backend != SocketBackendKind::Tcp {
                        return Err(ERRNO::EADDRNOTAVAIL);
                    }
                    let token = current_user_token();
                    let group = parse_group_req(token, optval, optlen)?;
                    let left = with_tcp_socket(fd, |tcp| Ok(tcp.leave_mcast_group(group)))?;
                    if left {
                        Ok(0)
                    } else {
                        Err(ERRNO::EADDRNOTAVAIL)
                    }
                }
                _ => {
                    warn!("setsockopt(fd={}, level={}, optname={}) not implemented for SOL_IP, ignored", fd, level, optname);
                    Ok(0)
                }
            },
            Some(SocketLevel::IpProtoIpv6) => {
                if spec.family != AF_INET6 as i32 {
                    return Err(ERRNO::ENOPROTOOPT);
                }
                match optname {
                    IPV6_V6ONLY => {
                        let token = current_user_token();
                        let enabled = read_sockopt_i32(token, optval, optlen)? != 0;
                        match backend {
                            SocketBackendKind::Udp => {
                                with_udp_socket(fd, |udp| {
                                    udp.set_ipv6_only(enabled);
                                    Ok(())
                                })?;
                            }
                            SocketBackendKind::Tcp => {
                                with_tcp_socket(fd, |tcp| {
                                    tcp.set_ipv6_only(enabled);
                                    Ok(())
                                })?;
                            }
                            SocketBackendKind::RawIpv6 => {}
                            _ => return Err(ERRNO::ENOPROTOOPT),
                        }
                        Ok(0)
                    }
                    IPV6_CHECKSUM => {
                        if backend != SocketBackendKind::RawIpv6 {
                            return Err(ERRNO::ENOPROTOOPT);
                        }
                        let token = current_user_token();
                        let offset = read_sockopt_i32(token, optval, optlen)?;
                        with_raw_ipv6_socket(fd, |raw| raw.set_checksum_offset(offset))?;
                        Ok(0)
                    }
                    IPV6_RECVPKTINFO | IPV6_RECVHOPLIMIT | IPV6_RECVRTHDR | IPV6_RECVHOPOPTS
                    | IPV6_RECVDSTOPTS | IPV6_RECVTCLASS | IPV6_2292PKTINFO
                    | IPV6_2292HOPLIMIT | IPV6_2292RTHDR | IPV6_2292HOPOPTS
                    | IPV6_2292DSTOPTS => {
                        if backend != SocketBackendKind::RawIpv6 {
                            return Err(ERRNO::ENOPROTOOPT);
                        }
                        let token = current_user_token();
                        let enabled = read_sockopt_i32(token, optval, optlen)? != 0;
                        with_raw_ipv6_socket(fd, |raw| raw.set_bool_option(optname, enabled))?;
                        Ok(0)
                    }
                    _ => Err(ERRNO::ENOPROTOOPT),
                }
            }
            _ => {
                warn!("setsockopt(fd={}, level={}, optname={}) not implemented, ignored", fd, level, optname);
                Ok(0)
            }
        }
    })
}

pub fn sys_getsockopt(fd: i32, level: i32, optname: i32, optval: *mut u8, optlen: *mut i32) -> isize {
    syscall_body!({
        let fd = fd as usize;
        let backend = socket_backend(fd)?;
        let spec = socket_spec(fd)?;
        let token = current_user_token();

        if level == IPPROTO_ICMPV6 {
            if spec.family != AF_INET6 as i32 || spec.socket_type != SOCK_RAW || spec.protocol != IPPROTO_ICMPV6 {
                return Err(ERRNO::ENOPROTOOPT);
            }
            return match optname {
                ICMP6_FILTER => {
                    let bytes = with_raw_ipv6_socket(fd, |raw| Ok(raw.icmp6_filter_bytes()))?;
                    write_getsockopt_value(token, optval, optlen, bytes.as_slice())?;
                    Ok(0)
                }
                _ => Err(ERRNO::ENOPROTOOPT),
            };
        }

        match SocketLevel::from_repr(level) {
            Some(SocketLevel::SolSocket) => match PosixSocketOption::from_repr(optname) {
                Some(PosixSocketOption::SoType) => {
                    write_getsockopt_i32(token, optval, optlen, spec.socket_type)?;
                    Ok(0)
                }
                Some(PosixSocketOption::SoAcceptConn) => {
                    let mut acceptconn = 0i32;
                    if backend == SocketBackendKind::Tcp {
                        with_tcp_socket(fd, |tcp| {
                            acceptconn = if tcp.is_listening() { 1 } else { 0 };
                            Ok(())
                        })?;
                    }
                    write_getsockopt_i32(token, optval, optlen, acceptconn)?;
                    Ok(0)
                }
                Some(PosixSocketOption::SoRcvBuf) => {
                    let mut size = 0i32;
                    if backend == SocketBackendKind::Udp {
                        with_udp_socket(fd, |udp| {
                            size = udp.recv_buffer_size() as i32;
                            Ok(())
                        })?;
                    } else if backend == SocketBackendKind::Tcp {
                        with_tcp_socket(fd, |tcp| {
                            size = tcp.recv_buffer_size() as i32;
                            Ok(())
                        })?;
                    } else {
                        warn!(
                            "getsockopt(fd={}, level={}, optname={}) not implemented on UNIX socket, using default=0",
                            fd,
                            level,
                            optname
                        );
                        // Provide a deterministic default value (0) instead of
                        // leaving user memory uninitialized.
                        write_getsockopt_i32(token, optval, optlen, 0)?;
                        return Ok(0);
                    }
                    write_getsockopt_i32(token, optval, optlen, size)?;
                    Ok(0)
                }
                Some(PosixSocketOption::SoSndBuf) => {
                    let mut size = 0i32;
                    if backend == SocketBackendKind::Udp {
                        with_udp_socket(fd, |udp| {
                            size = udp.send_buffer_size() as i32;
                            Ok(())
                        })?;
                    } else if backend == SocketBackendKind::Tcp {
                        with_tcp_socket(fd, |tcp| {
                            size = tcp.send_buffer_size() as i32;
                            Ok(())
                        })?;
                    } else {
                        warn!(
                            "getsockopt(fd={}, level={}, optname={}) not implemented on UNIX socket, using default=0",
                            fd,
                            level,
                            optname
                        );
                        // Provide deterministic default
                        write_getsockopt_i32(token, optval, optlen, 0)?;
                        return Ok(0);
                    }
                    write_getsockopt_i32(token, optval, optlen, size)?;
                    Ok(0)
                }
                Some(PosixSocketOption::SoRecvTimeo) => {
                    let timeout_ns = match backend {
                        SocketBackendKind::Udp => {
                            let mut ns = 0u64;
                            with_udp_socket(fd, |udp| { ns = udp.recv_timeout_ns(); Ok(()) })?;
                            ns
                        }
                        SocketBackendKind::Tcp => {
                            let mut ns = 0u64;
                            with_tcp_socket(fd, |tcp| { ns = tcp.recv_timeout_ns(); Ok(()) })?;
                            ns
                        }
                        SocketBackendKind::RawIpv6 | SocketBackendKind::UnixStream | SocketBackendKind::UnixDatagram | SocketBackendKind::CompatIfreq | SocketBackendKind::Packet | SocketBackendKind::NetlinkRoute | SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => 0,
                    };
                    let timeval = timeval_from_ns(timeout_ns);
                    let bytes = unsafe {
                        slice::from_raw_parts(
                            (&timeval as *const TimeVal) as *const u8,
                            size_of::<TimeVal>(),
                        )
                    };
                    write_getsockopt_value(token, optval, optlen, bytes)?;
                    Ok(0)
                }
                Some(PosixSocketOption::SoSndTimeo) => {
                    let timeout_ns = match backend {
                        SocketBackendKind::Udp => {
                            let mut ns = 0u64;
                            with_udp_socket(fd, |udp| { ns = udp.send_timeout_ns(); Ok(())})?;
                            ns
                        }
                        SocketBackendKind::Tcp => {
                            let mut ns = 0u64;
                            with_tcp_socket(fd, |tcp| { ns = tcp.send_timeout_ns(); Ok(())})?;
                            ns
                        }
                        SocketBackendKind::RawIpv6 | SocketBackendKind::UnixStream | SocketBackendKind::UnixDatagram | SocketBackendKind::CompatIfreq | SocketBackendKind::Packet | SocketBackendKind::NetlinkRoute | SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => 0,
                    };
                    let timeval = timeval_from_ns(timeout_ns);
                    let bytes = unsafe {
                        slice::from_raw_parts(
                            (&timeval as *const TimeVal) as *const u8, size_of::<TimeVal>(),
                        )
                    };
                    write_getsockopt_value(token, optval, optlen, bytes)?;
                    Ok(0)
                }
                Some(PosixSocketOption::SoError) => {
                    write_getsockopt_i32(token, optval, optlen, 0)?;
                    Ok(0)
                }
                Some(PosixSocketOption::SoPassCred) => {
                    if spec.family != AF_UNIX {
                        warn!(
                            "getsockopt(fd={}, level={}, optname={}) unsupported on non-UNIX socket, ignored",
                            fd,
                            level,
                            optname
                        );
                        return Ok(0);
                    }
                    let mut enabled = 0i32;
                    with_unix_socket(fd, |unix| {
                        enabled = if unix.passcred_enabled() { 1 } else { 0 };
                        Ok(())
                    })?;
                    write_getsockopt_i32(token, optval, optlen, enabled)?;
                    Ok(0)
                }
                _ => {
                    warn!(
                        "getsockopt(fd={}, level={}, optname={}) not implemented for SOL_SOCKET, ignored",
                        fd,
                        level,
                        optname
                    );
                    write_getsockopt_i32(token, optval, optlen, 0)?;
                    Ok(0)
                }
            },
            Some(SocketLevel::IpProtoIp) | Some(SocketLevel::IpProtoTcp) => match PosixTcpSocketOption::from_repr(optname) {
                Some(PosixTcpSocketOption::NoDelay) => {
                    write_getsockopt_i32(token, optval, optlen, 0)?;
                    Ok(0)
                }
                Some(PosixTcpSocketOption::MaxSeg) => {
                    const MAX_SEGMENT_SIZE: i32 = 1666;
                    write_getsockopt_i32(token, optval, optlen, MAX_SEGMENT_SIZE)?;
                    Ok(0)
                }
                Some(PosixTcpSocketOption::Congestion) => {
                    const CONGESTION: &str = "reno";
                    write_getsockopt_value(token, optval, optlen, CONGESTION.as_bytes())?;
                    Ok(0)
                },
                Some(PosixTcpSocketOption::Info) => {
                    let info = LinuxTcpInfo::default();
                    let bytes = unsafe {
                        core::slice::from_raw_parts(
                            &info as *const _ as *const u8,
                            core::mem::size_of::<LinuxTcpInfo>(),
                        )
                    };
                    write_getsockopt_value(token, optval, optlen, bytes)?;
                    Ok(0)
                }
                _ => {
                    warn!(
                        "getsockopt(fd={}, level={}, optname={}) not implemented for IP/TCP, ignored",
                        fd,
                        level,
                        optname
                    );
                    // Provide a deterministic fallback instead of leaving user memory
                    // uninitialized or attempting complex async operations here.
                    write_getsockopt_i32(token, optval, optlen, 0)?;
                    Ok(0)
                }
            },
            Some(SocketLevel::IpProtoIpv6) => {
                if spec.family != AF_INET6 as i32 {
                    return Err(ERRNO::ENOPROTOOPT);
                }
                match optname {
                    IPV6_V6ONLY => {
                        let value = match backend {
                            SocketBackendKind::Udp => {
                                let mut value = 0i32;
                                with_udp_socket(fd, |udp| {
                                    value = if udp.ipv6_only() { 1 } else { 0 };
                                    Ok(())
                                })?;
                                value
                            }
                            SocketBackendKind::Tcp => {
                                let mut value = 0i32;
                                with_tcp_socket(fd, |tcp| {
                                    value = if tcp.ipv6_only() { 1 } else { 0 };
                                    Ok(())
                                })?;
                                value
                            }
                            SocketBackendKind::RawIpv6 => 1,
                            _ => return Err(ERRNO::ENOPROTOOPT),
                        };
                        write_getsockopt_i32(token, optval, optlen, value)?;
                        Ok(0)
                    }
                    IPV6_CHECKSUM => {
                        if backend != SocketBackendKind::RawIpv6 {
                            return Err(ERRNO::ENOPROTOOPT);
                        }
                        let value = with_raw_ipv6_socket(fd, |raw| Ok(raw.checksum_offset()))?;
                        write_getsockopt_i32(token, optval, optlen, value)?;
                        Ok(0)
                    }
                    IPV6_RECVPKTINFO | IPV6_RECVHOPLIMIT | IPV6_RECVRTHDR | IPV6_RECVHOPOPTS
                    | IPV6_RECVDSTOPTS | IPV6_RECVTCLASS | IPV6_2292PKTINFO
                    | IPV6_2292HOPLIMIT | IPV6_2292RTHDR | IPV6_2292HOPOPTS
                    | IPV6_2292DSTOPTS => {
                        if backend != SocketBackendKind::RawIpv6 {
                            return Err(ERRNO::ENOPROTOOPT);
                        }
                        let value = with_raw_ipv6_socket(fd, |raw| raw.get_bool_option(optname))?;
                        write_getsockopt_i32(token, optval, optlen, value)?;
                        Ok(0)
                    }
                    _ => Err(ERRNO::ENOPROTOOPT),
                }
            }
            _ => {
                warn!(
                    "getsockopt(fd={}, level={}, optname={}) not implemented, ignored",
                    fd,
                    level,
                    optname
                );
                // Provide a deterministic fallback instead of leaving user memory
                // uninitialized or attempting complex async operations here.
                write_getsockopt_i32(token, optval, optlen, 0)?;
                Ok(0)
            }
        }
    })
}

pub fn sys_sendmsg(fd: i32, msg: *const MsgHdr, flags: u32) -> isize {
    syscall_body!({
        if msg.is_null() {
            return Err(ERRNO::EFAULT);
        }
        if flags != 0 {
            return Err(ERRNO::EOPNOTSUPP);
        }

        let token = current_user_token();
        let msghdr = *translated_ref(token, msg).or_errno(ERRNO::EFAULT)?;
        if msghdr.msg_iovlen > MAX_MSG_IOV {
            return Err(ERRNO::EINVAL);
        }
        if msghdr.msg_controllen > MAX_MSG_CONTROL {
            return Err(ERRNO::EINVAL);
        }

        let iovecs = copy_user_iovecs(token, msghdr.msg_iov as *const IoVec, msghdr.msg_iovlen)?;
        let total_len = iovecs_total_len(&iovecs)?;
        let ubuf = iovecs_to_user_buffer(token, &iovecs, PageFaultAccess::Read)?;

        let fd = fd as usize;
        let n = match socket_backend(fd)? {
            SocketBackendKind::UnixStream => {
                let ancillary = if msghdr.msg_controllen == 0 {
                    UnixSocketAncillaryData::default()
                } else {
                    if msghdr.msg_control == 0 {
                        return Err(ERRNO::EFAULT);
                    }
                    let control_bytes = copy_user_bytes(
                        token,
                        msghdr.msg_control as *const u8,
                        msghdr.msg_controllen,
                    )?;
                    parse_send_ancillary(control_bytes.as_slice())?
                };

                if total_len == 0 && !ancillary.is_empty() {
                    return Err(ERRNO::EINVAL);
                }

                with_unix_socket(fd, |unix| unix.sendmsg(ubuf, ancillary))?
            }
            SocketBackendKind::AlgRequest => {
                let params = if msghdr.msg_controllen == 0 {
                    AlgSendMsgParams::default()
                } else {
                    if msghdr.msg_control == 0 {
                        return Err(ERRNO::EFAULT);
                    }
                    let control_bytes = copy_user_bytes(
                        token,
                        msghdr.msg_control as *const u8,
                        msghdr.msg_controllen,
                    )?;
                    parse_alg_sendmsg(control_bytes.as_slice())?
                };
                with_alg_request(fd, |alg| alg.sendmsg(total_len, params))?
            }
            SocketBackendKind::RawIpv6 => {
                let dst = if msghdr.msg_name == 0 {
                    return Err(ERRNO::EDESTADDRREQ);
                } else {
                    read_sockaddr_in6(msghdr.msg_name as *const u8, msghdr.msg_namelen)?
                };
                let meta = if msghdr.msg_controllen == 0 {
                    RawIpv6SendMeta::default()
                } else {
                    if msghdr.msg_control == 0 {
                        return Err(ERRNO::EFAULT);
                    }
                    let control_bytes = copy_user_bytes(
                        token,
                        msghdr.msg_control as *const u8,
                        msghdr.msg_controllen,
                    )?;
                    parse_raw_ipv6_send_meta(control_bytes.as_slice())?
                };
                with_raw_ipv6_socket(fd, |raw| raw.send_user_buffer_to(&ubuf, &dst, &meta))?
            }
            SocketBackendKind::NetlinkRoute => {
                if msghdr.msg_controllen != 0 {
                    return Err(ERRNO::EOPNOTSUPP);
                }
                with_netlink_route_socket(fd, |netlink| netlink.send_user_buffer(&ubuf))?
            }
            SocketBackendKind::Udp | SocketBackendKind::Tcp | SocketBackendKind::UnixDatagram | SocketBackendKind::CompatIfreq | SocketBackendKind::Packet | SocketBackendKind::AlgSocket => return Err(ERRNO::EOPNOTSUPP),
        };
        Ok(n as isize)
    })
}

pub fn sys_recvmsg(fd: i32, msg: *mut MsgHdr, flags: u32) -> isize {
    syscall_body!({
        if msg.is_null() {
            return Err(ERRNO::EFAULT);
        }
        if flags & !MSG_CMSG_CLOEXEC != 0 {
            return Err(ERRNO::EOPNOTSUPP);
        }

        let token = current_user_token();
        let mut msghdr = *translated_ref(token, msg as *const MsgHdr).or_errno(ERRNO::EFAULT)?;
        if msghdr.msg_iovlen > MAX_MSG_IOV {
            return Err(ERRNO::EINVAL);
        }
        if msghdr.msg_controllen > MAX_MSG_CONTROL {
            return Err(ERRNO::EINVAL);
        }
        if msghdr.msg_controllen > 0 && msghdr.msg_control == 0 {
            return Err(ERRNO::EFAULT);
        }

        let iovecs = copy_user_iovecs(token, msghdr.msg_iov as *const IoVec, msghdr.msg_iovlen)?;
        let _total_len = iovecs_total_len(&iovecs)?;
        let ubuf = iovecs_to_user_buffer(token, &iovecs, PageFaultAccess::Write)?;

        let fd = fd as usize;
        let backend = socket_backend(fd)?;
        let (n, ancillary, raw_control): (usize, UnixSocketAncillaryData, Vec<RawIpv6ControlMessage>) = match backend {
            SocketBackendKind::UnixStream => with_unix_socket(fd, |unix| {
                let (n, mut ancillary) = unix.recvmsg(ubuf)?;
                if !unix.passcred_enabled() {
                    ancillary.credentials = None;
                }
                Ok((n, ancillary, Vec::new()))
            })?,
            SocketBackendKind::NetlinkRoute => (
                with_netlink_route_socket(fd, |netlink| {
                    let mut ubuf = ubuf;
                    netlink.recv_into_user_buffer(&mut ubuf, false)
                })?,
                UnixSocketAncillaryData::default(),
                Vec::new(),
            ),
            SocketBackendKind::RawIpv6 => {
                let mut ubuf = ubuf;
                let packet = with_raw_ipv6_socket(fd, |raw| raw.recv_into_user_buffer(&mut ubuf))?;
                (packet.data.len(), UnixSocketAncillaryData::default(), packet.control)
            }
            SocketBackendKind::Udp | SocketBackendKind::Tcp | SocketBackendKind::UnixDatagram | SocketBackendKind::CompatIfreq | SocketBackendKind::Packet | SocketBackendKind::AlgSocket | SocketBackendKind::AlgRequest => {
                return Err(ERRNO::EOPNOTSUPP)
            }
        };

        let cloexec = (flags & MSG_CMSG_CLOEXEC) != 0;
        let control_cap = msghdr.msg_controllen;
        let mut control_out = Vec::new();
        let mut used = 0usize;
        msghdr.msg_flags = 0;

        if let Some(cred) = ancillary.credentials {
            let cred_payload = unsafe {
                core::slice::from_raw_parts(
                    (&cred as *const UnixUcred) as *const u8,
                    size_of::<UnixUcred>(),
                )
            };
            let need = cmsg_align(size_of::<CmsgHdr>() + cred_payload.len());
            if used + need <= control_cap {
                append_cmsg(&mut control_out, SocketLevel::SolSocket as i32, SCM_CREDENTIALS, cred_payload);
                used += need;
            } else {
                msghdr.msg_flags |= MSG_CTRUNC;
            }
        }

        if !ancillary.rights.is_empty() {
            let rights_payload_len = ancillary.rights.len() * size_of::<i32>();
            let need = cmsg_align(size_of::<CmsgHdr>() + rights_payload_len);
            if used + need <= control_cap {
                let received_fds = install_received_rights(ancillary.rights, cloexec)?;
                let mut payload = Vec::with_capacity(received_fds.len() * size_of::<i32>());
                for fd in received_fds {
                    payload.extend_from_slice(&fd.to_ne_bytes());
                }
                append_cmsg(&mut control_out, SocketLevel::SolSocket as i32, SCM_RIGHTS, payload.as_slice());
            } else {
                msghdr.msg_flags |= MSG_CTRUNC;
            }
        }

        for cmsg in raw_control {
            let need = cmsg_align(size_of::<CmsgHdr>() + cmsg.data.len());
            if used + need <= control_cap {
                append_cmsg(&mut control_out, cmsg.level, cmsg.cmsg_type, cmsg.data.as_slice());
                used += need;
            } else {
                msghdr.msg_flags |= MSG_CTRUNC;
            }
        }

        if !control_out.is_empty() {
            write_bytes_to_user(msghdr.msg_control as *mut u8, control_out.as_slice())?;
        }

        msghdr.msg_controllen = control_out.len();
        if backend == SocketBackendKind::NetlinkRoute && msghdr.msg_name != 0 {
            let sockaddr = SockAddrNl {
                nl_family: AF_NETLINK as u16,
                nl_pad: 0,
                nl_pid: 0,
                nl_groups: 0,
            };
            let name_len = core::mem::size_of::<SockAddrNl>().min(msghdr.msg_namelen);
            write_bytes_to_user(
                msghdr.msg_name as *mut u8,
                &unsafe {
                    core::slice::from_raw_parts(
                        (&sockaddr as *const SockAddrNl) as *const u8,
                        name_len,
                    )
                },
            )?;
            msghdr.msg_namelen = core::mem::size_of::<SockAddrNl>();
        } else {
            msghdr.msg_namelen = 0;
        }
        write_pod_to_user(msg, &msghdr)?;

        Ok(n as isize)
    })
}
