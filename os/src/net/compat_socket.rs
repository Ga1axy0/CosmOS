use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::cmp::min;
use core::mem::size_of;

use crate::fs::{File, Stat, StatMode};
use crate::mm::{PageFaultAccess, UserBuffer};
use crate::net::compat;
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::syscall::translated_byte_buffer_with_access;

const IFNAMSIZ: usize = 16;
const IFREQ_SIZE: usize = 40;
const ARPHRD_ETHER: u16 = 1;
const ETH_P_IP: u16 = 0x0800;
const ETH_P_ARP: u16 = 0x0806;
const ARPOP_REQUEST: u16 = 1;
const ARPOP_REPLY: u16 = 2;
const AF_PACKET_FAMILY: u16 = 17;
const PACKET_HOST: u8 = 0;
const SIOCGIFFLAGS: usize = 0x8913;
const SIOCSIFFLAGS: usize = 0x8914;
const SIOCGIFNAME: usize = 0x8910;
const SIOCGIFINDEX: usize = 0x8933;
const SIOCGIFMTU: usize = 0x8921;
const SIOCGIFHWADDR: usize = 0x8927;
const SIOCGIFTXQLEN: usize = 0x8942;

const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_NEWROUTE: u16 = 24;
const RTM_DELROUTE: u16 = 25;
const RTM_GETROUTE: u16 = 26;
const RTM_GETADDR: u16 = 22;
const RTM_NEWADDR: u16 = 20;
const RTM_DELADDR: u16 = 21;
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_ACK: u16 = 0x4;
const IFF_UP: u32 = 0x1;
const IFLA_IFNAME: u16 = 3;
const IFLA_ADDRESS: u16 = 1;
const IFLA_MTU: u16 = 4;
const IFLA_LINKINFO: u16 = 18;
const IFLA_NET_NS_PID: u16 = 19;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFLA_INFO_KIND: u16 = 1;
const IFLA_INFO_DATA: u16 = 2;
const VETH_INFO_PEER: u16 = 1;
const AF_INET: u8 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NlMsghdr {
    nlmsg_len: u32,
    nlmsg_type: u16,
    nlmsg_flags: u16,
    nlmsg_seq: u32,
    nlmsg_pid: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IfInfoMsg {
    ifi_family: u8,
    __ifi_pad: u8,
    ifi_type: u16,
    ifi_index: i32,
    ifi_flags: u32,
    ifi_change: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IfAddrMsg {
    ifa_family: u8,
    ifa_prefixlen: u8,
    ifa_flags: u8,
    ifa_scope: u8,
    ifa_index: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RtAttr {
    rta_len: u16,
    rta_type: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct SockAddrLl {
    pub(crate) sll_family: u16,
    pub(crate) sll_protocol: u16,
    pub(crate) sll_ifindex: i32,
    pub(crate) sll_hatype: u16,
    pub(crate) sll_pkttype: u8,
    pub(crate) sll_halen: u8,
    pub(crate) sll_addr: [u8; 8],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ArpHdr {
    ar_hrd: u16,
    ar_pro: u16,
    ar_hln: u8,
    ar_pln: u8,
    ar_op: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NlMsgErr {
    error: i32,
    msg: NlMsghdr,
}

fn socket_stat(id: usize) -> Stat {
    Stat {
        dev: 0,
        ino: id as u64,
        mode: StatMode::SOCK,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        pad0: 0,
        size: 0,
        blksize: 0,
        pad1: 0,
        blocks: 0,
        atime_sec: 0,
        atime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        ctime_sec: 0,
        ctime_nsec: 0,
        unused: [0; 2],
    }
}

fn copy_user_bytes(ptr: *const u8, len: usize) -> Result<Vec<u8>, ERRNO> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let chunks = translated_byte_buffer_with_access(ptr, len, PageFaultAccess::Read)?;
    let mut out = Vec::with_capacity(len);
    for chunk in chunks {
        out.extend_from_slice(chunk);
    }
    if out.len() != len {
        return Err(ERRNO::EFAULT);
    }
    Ok(out)
}

fn write_user_buffer(dst: &mut UserBuffer, src: &[u8]) -> usize {
    let mut copied = 0usize;
    for chunk in dst.buffers.iter_mut() {
        if copied >= src.len() {
            break;
        }
        let take = min(chunk.len(), src.len() - copied);
        chunk[..take].copy_from_slice(&src[copied..copied + take]);
        copied += take;
    }
    copied
}

fn c_name(bytes: &[u8]) -> &str {
    let len = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..len]).unwrap_or("")
}

fn nlmsg_align(len: usize) -> usize {
    (len + 3) & !3
}

fn rta_align(len: usize) -> usize {
    nlmsg_align(len)
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((value as *const T) as *const u8, size_of::<T>()) }
}

fn build_done(seq: u32) -> Vec<u8> {
    let hdr = NlMsghdr {
        nlmsg_len: size_of::<NlMsghdr>() as u32,
        nlmsg_type: NLMSG_DONE,
        nlmsg_flags: 0,
        nlmsg_seq: seq,
        nlmsg_pid: 0,
    };
    as_bytes(&hdr).to_vec()
}

fn build_ack(req: &NlMsghdr, error: i32) -> Vec<u8> {
    let payload = NlMsgErr { error, msg: *req };
    let hdr = NlMsghdr {
        nlmsg_len: (size_of::<NlMsghdr>() + size_of::<NlMsgErr>()) as u32,
        nlmsg_type: NLMSG_ERROR,
        nlmsg_flags: 0,
        nlmsg_seq: req.nlmsg_seq,
        nlmsg_pid: 0,
    };
    let mut out = Vec::with_capacity(size_of::<NlMsghdr>() + size_of::<NlMsgErr>());
    out.extend_from_slice(as_bytes(&hdr));
    out.extend_from_slice(as_bytes(&payload));
    out
}

fn push_ack(req: &NlMsghdr, error: i32, out: &mut Vec<u8>) {
    out.extend_from_slice(build_ack(req, error).as_slice());
}

fn maybe_push_ack(req: &NlMsghdr, error: i32, out: &mut Vec<u8>) {
    if req.nlmsg_flags & NLM_F_ACK != 0 {
        push_ack(req, error, out);
    }
}

fn for_each_attr(mut buf: &[u8], mut f: impl FnMut(u16, &[u8])) {
    while buf.len() >= size_of::<RtAttr>() {
        let attr = unsafe { &*(buf.as_ptr() as *const RtAttr) };
        let len = attr.rta_len as usize;
        if len < size_of::<RtAttr>() || len > buf.len() {
            break;
        }
        let payload = &buf[size_of::<RtAttr>()..len];
        f(attr.rta_type, payload);
        let step = rta_align(len);
        if step > buf.len() {
            break;
        }
        buf = &buf[step..];
    }
}

fn scan_link_attrs(
    buf: &[u8],
    ifnames: &mut Vec<String>,
    link_kind: &mut Option<String>,
    has_netns_pid: &mut bool,
) {
    for_each_attr(buf, |kind, payload| match kind {
        IFLA_IFNAME => {
            let name = c_name(payload);
            if !name.is_empty() {
                ifnames.push(name.to_string());
            }
        }
        IFLA_INFO_KIND => {
            let kind_name = c_name(payload);
            if !kind_name.is_empty() {
                *link_kind = Some(kind_name.to_string());
            }
        }
        IFLA_NET_NS_PID => *has_netns_pid = true,
        IFLA_LINKINFO => scan_link_attrs(payload, ifnames, link_kind, has_netns_pid),
        IFLA_INFO_DATA => {
            for_each_attr(payload, |data_kind, data_payload| {
                if data_kind == VETH_INFO_PEER {
                    if data_payload.len() >= size_of::<IfInfoMsg>() {
                        scan_link_attrs(
                            &data_payload[size_of::<IfInfoMsg>()..],
                            ifnames,
                            link_kind,
                            has_netns_pid,
                        );
                    } else {
                        scan_link_attrs(data_payload, ifnames, link_kind, has_netns_pid);
                    }
                } else {
                    scan_link_attrs(data_payload, ifnames, link_kind, has_netns_pid);
                }
            });
        }
        _ => {}
    });
}

fn scan_nested_ifnames(buf: &[u8], ifnames: &mut Vec<String>) {
    for_each_attr(buf, |kind, payload| {
        if kind == IFLA_IFNAME {
            let name = c_name(payload);
            if !name.is_empty() {
                ifnames.push(name.to_string());
            }
        }
        if payload.len() >= size_of::<RtAttr>() {
            scan_nested_ifnames(payload, ifnames);
        }
        if payload.len() > size_of::<IfInfoMsg>() {
            scan_nested_ifnames(&payload[size_of::<IfInfoMsg>()..], ifnames);
        }
    });
}

fn build_addr_dump(seq: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for iface in compat::list_ifaces() {
        if iface.has_ipv4 == 0 {
            continue;
        }
        let addr = IfAddrMsg {
            ifa_family: AF_INET,
            ifa_prefixlen: iface.prefix,
            ifa_flags: 0,
            ifa_scope: 0,
            ifa_index: iface.ifindex as u32,
        };
        let attr_len = size_of::<RtAttr>() + iface.ipv4.len();
        let msg_len = size_of::<NlMsghdr>() + size_of::<IfAddrMsg>() + rta_align(attr_len);
        let hdr = NlMsghdr {
            nlmsg_len: msg_len as u32,
            nlmsg_type: RTM_NEWADDR,
            nlmsg_flags: 0,
            nlmsg_seq: seq,
            nlmsg_pid: 0,
        };
        let attr = RtAttr {
            rta_len: attr_len as u16,
            rta_type: IFA_ADDRESS,
        };
        out.extend_from_slice(as_bytes(&hdr));
        out.extend_from_slice(as_bytes(&addr));
        out.extend_from_slice(as_bytes(&attr));
        out.extend_from_slice(&iface.ipv4);
        let aligned = rta_align(attr_len);
        if aligned > attr_len {
            out.resize(out.len() + (aligned - attr_len), 0);
        }
    }
    out.extend_from_slice(build_done(seq).as_slice());
    out
}

fn append_attr(out: &mut Vec<u8>, rta_type: u16, payload: &[u8]) {
    let len = size_of::<RtAttr>() + payload.len();
    let attr = RtAttr {
        rta_len: len as u16,
        rta_type,
    };
    out.extend_from_slice(as_bytes(&attr));
    out.extend_from_slice(payload);
    let aligned = rta_align(len);
    if aligned > len {
        out.resize(out.len() + (aligned - len), 0);
    }
}

fn build_link_dump(seq: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for iface in compat::list_ifaces() {
        let info = IfInfoMsg {
            ifi_family: 0,
            __ifi_pad: 0,
            ifi_type: ARPHRD_ETHER,
            ifi_index: iface.ifindex as i32,
            ifi_flags: if iface.up != 0 { IFF_UP } else { 0 },
            ifi_change: 0,
        };
        let mut attrs = Vec::new();
        let name_len = iface
            .name
            .iter()
            .position(|byte| *byte == 0)
            .map(|idx| idx + 1)
            .unwrap_or(IFNAMSIZ);
        append_attr(&mut attrs, IFLA_IFNAME, &iface.name[..name_len]);
        append_attr(&mut attrs, IFLA_MTU, &iface.mtu.to_ne_bytes());
        append_attr(&mut attrs, IFLA_ADDRESS, &iface.mac);

        let hdr = NlMsghdr {
            nlmsg_len: (size_of::<NlMsghdr>() + size_of::<IfInfoMsg>() + attrs.len()) as u32,
            nlmsg_type: RTM_NEWLINK,
            nlmsg_flags: 0,
            nlmsg_seq: seq,
            nlmsg_pid: 0,
        };
        out.extend_from_slice(as_bytes(&hdr));
        out.extend_from_slice(as_bytes(&info));
        out.extend_from_slice(attrs.as_slice());
    }
    out.extend_from_slice(build_done(seq).as_slice());
    out
}

fn derive_veth_peer_name(name: &str) -> String {
    let digit_start = name
        .rfind(|ch: char| !ch.is_ascii_digit())
        .map(|idx| idx + 1)
        .unwrap_or(0);
    if digit_start < name.len() {
        if let Ok(value) = name[digit_start..].parse::<usize>() {
            let mut peer = String::from(&name[..digit_start]);
            peer.push_str(&(value + 1).to_string());
            return peer;
        }
    }
    let mut peer = String::from(name);
    peer.push_str("_peer");
    peer
}

fn read_arp_ipv4_request(packet: &[u8]) -> Option<([u8; 6], [u8; 4], [u8; 4])> {
    if packet.len() < size_of::<ArpHdr>() + 20 {
        return None;
    }
    let hdr = unsafe { &*(packet.as_ptr() as *const ArpHdr) };
    if u16::from_be(hdr.ar_hrd) != ARPHRD_ETHER
        || u16::from_be(hdr.ar_pro) != ETH_P_IP
        || hdr.ar_hln != 6
        || hdr.ar_pln != 4
        || u16::from_be(hdr.ar_op) != ARPOP_REQUEST
    {
        return None;
    }

    let payload = &packet[size_of::<ArpHdr>()..];
    let mut src_mac = [0u8; 6];
    let mut src_ip = [0u8; 4];
    let mut dst_ip = [0u8; 4];
    src_mac.copy_from_slice(&payload[..6]);
    src_ip.copy_from_slice(&payload[6..10]);
    dst_ip.copy_from_slice(&payload[16..20]);
    Some((src_mac, src_ip, dst_ip))
}

fn build_arp_ipv4_reply(
    sender_mac: [u8; 6],
    sender_ip: [u8; 4],
    target_mac: [u8; 6],
    target_ip: [u8; 4],
) -> Vec<u8> {
    let hdr = ArpHdr {
        ar_hrd: ARPHRD_ETHER.to_be(),
        ar_pro: ETH_P_IP.to_be(),
        ar_hln: 6,
        ar_pln: 4,
        ar_op: ARPOP_REPLY.to_be(),
    };
    let mut out = Vec::with_capacity(size_of::<ArpHdr>() + 20);
    out.extend_from_slice(as_bytes(&hdr));
    out.extend_from_slice(&sender_mac);
    out.extend_from_slice(&sender_ip);
    out.extend_from_slice(&target_mac);
    out.extend_from_slice(&target_ip);
    out
}

pub(crate) fn compat_ifreq_ioctl(req: usize, arg: usize) -> Result<isize, ERRNO> {
    let ptr = arg as *mut u8;
    if ptr.is_null() {
        return Err(ERRNO::EFAULT);
    }
    let mut ifreq = copy_user_bytes(ptr as *const u8, IFREQ_SIZE)?;
    match req {
        SIOCGIFINDEX => {
            let name = c_name(&ifreq[..IFNAMSIZ]);
            let iface = compat::get_iface_info(name).ok_or(ERRNO::ENODEV)?;
            ifreq[IFNAMSIZ..IFNAMSIZ + 4].copy_from_slice(&(iface.ifindex as i32).to_ne_bytes());
        }
        SIOCGIFNAME => {
            let mut raw = [0u8; 4];
            raw.copy_from_slice(&ifreq[IFNAMSIZ..IFNAMSIZ + 4]);
            let ifindex = i32::from_ne_bytes(raw) as usize;
            let iface = compat::get_iface_by_ifindex(ifindex).ok_or(ERRNO::ENODEV)?;
            ifreq[..IFNAMSIZ].fill(0);
            let name_bytes = c_name(&iface.name).as_bytes();
            let take = min(name_bytes.len(), IFNAMSIZ.saturating_sub(1));
            ifreq[..take].copy_from_slice(&name_bytes[..take]);
        }
        SIOCGIFFLAGS => {
            let name = c_name(&ifreq[..IFNAMSIZ]);
            let iface = compat::get_iface_info(name).ok_or(ERRNO::ENODEV)?;
            let flags = if iface.up != 0 { IFF_UP as u16 } else { 0 };
            ifreq[IFNAMSIZ..IFNAMSIZ + 2].copy_from_slice(&flags.to_ne_bytes());
        }
        SIOCSIFFLAGS => {
            let name = c_name(&ifreq[..IFNAMSIZ]).to_string();
            let mut raw = [0u8; 2];
            raw.copy_from_slice(&ifreq[IFNAMSIZ..IFNAMSIZ + 2]);
            let flags = u16::from_ne_bytes(raw);
            compat::set_link_up(name.as_str(), (flags as u32 & IFF_UP) != 0)?;
        }
        SIOCGIFMTU => {
            let name = c_name(&ifreq[..IFNAMSIZ]);
            let iface = compat::get_iface_info(name).ok_or(ERRNO::ENODEV)?;
            ifreq[IFNAMSIZ..IFNAMSIZ + 4].copy_from_slice(&iface.mtu.to_ne_bytes());
        }
        SIOCGIFHWADDR => {
            let name = c_name(&ifreq[..IFNAMSIZ]);
            let iface = compat::get_iface_info(name).ok_or(ERRNO::ENODEV)?;
            ifreq[IFNAMSIZ..].fill(0);
            ifreq[IFNAMSIZ..IFNAMSIZ + 2].copy_from_slice(&ARPHRD_ETHER.to_ne_bytes());
            ifreq[IFNAMSIZ + 2..IFNAMSIZ + 8].copy_from_slice(&iface.mac);
        }
        SIOCGIFTXQLEN => {
            ifreq[IFNAMSIZ..IFNAMSIZ + 4].copy_from_slice(&0i32.to_ne_bytes());
        }
        _ => return Err(ERRNO::ENOTTY),
    }
    crate::syscall::write_bytes_to_user(ptr, &ifreq)?;
    Ok(0)
}

struct PacketBinding {
    ifindex: Option<usize>,
    protocol: u16,
}

struct PendingPacket {
    data: Vec<u8>,
    from: SockAddrLl,
}

pub(crate) struct PacketSocketFile {
    socket_type: i32,
    binding: SpinNoIrqLock<Option<PacketBinding>>,
    pending: SpinNoIrqLock<Option<PendingPacket>>,
}

pub(crate) struct CompatIfreqSocketFile;

impl File for PacketSocketFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn ioctl(&self, req: usize, arg: usize) -> Result<isize, ERRNO> {
        compat_ifreq_ioctl(req, arg)
    }

    fn stat(&self) -> Stat {
        socket_stat(self.poll_source_id())
    }
}

impl PacketSocketFile {
    pub(crate) fn socket_type(&self) -> i32 {
        self.socket_type
    }

    pub(crate) fn bind_raw(&self, addr: &[u8]) -> Result<(), ERRNO> {
        if addr.len() < size_of::<SockAddrLl>() {
            return Err(ERRNO::EINVAL);
        }
        let raw = unsafe { core::ptr::read_unaligned(addr.as_ptr() as *const SockAddrLl) };
        if raw.sll_family != AF_PACKET_FAMILY {
            return Err(ERRNO::EAFNOSUPPORT);
        }
        let ifindex = if raw.sll_ifindex == 0 {
            None
        } else if raw.sll_ifindex > 0 {
            let ifindex = raw.sll_ifindex as usize;
            compat::get_iface_by_ifindex(ifindex).ok_or(ERRNO::ENODEV)?;
            Some(ifindex)
        } else {
            return Err(ERRNO::ENODEV);
        };
        *self.binding.lock() = Some(PacketBinding {
            ifindex,
            protocol: u16::from_be(raw.sll_protocol),
        });
        Ok(())
    }

    pub(crate) fn getsockname_raw(&self) -> Result<SockAddrLl, ERRNO> {
        let binding = self.binding.lock();
        let binding = binding.as_ref().ok_or(ERRNO::EINVAL)?;
        let iface = binding.ifindex.and_then(compat::get_iface_by_ifindex);
        let mut out = SockAddrLl {
            sll_family: AF_PACKET_FAMILY,
            sll_protocol: binding.protocol.to_be(),
            sll_ifindex: binding.ifindex.unwrap_or(0) as i32,
            sll_hatype: ARPHRD_ETHER,
            sll_pkttype: PACKET_HOST,
            sll_halen: if iface.is_some() { 6 } else { 0 },
            sll_addr: [0; 8],
        };
        if let Some(iface) = iface {
            out.sll_addr[..6].copy_from_slice(&iface.mac);
        }
        Ok(out)
    }

    pub(crate) fn send_user_buffer_to(
        &self,
        buf: &UserBuffer,
        addr: Option<&[u8]>,
    ) -> Result<usize, ERRNO> {
        let mut data = Vec::with_capacity(buf.len());
        for chunk in buf.buffers.iter() {
            data.extend_from_slice(chunk);
        }

        let bound_ifindex = {
            let binding = self.binding.lock();
            binding.as_ref().ok_or(ERRNO::EINVAL)?.ifindex
        };

        let send_ifindex = if let Some(addr) = addr {
            if addr.len() < size_of::<SockAddrLl>() {
                return Err(ERRNO::EINVAL);
            }
            let raw = unsafe { core::ptr::read_unaligned(addr.as_ptr() as *const SockAddrLl) };
            if raw.sll_family != AF_PACKET_FAMILY {
                return Err(ERRNO::EAFNOSUPPORT);
            }
            raw.sll_ifindex as usize
        } else {
            bound_ifindex.unwrap_or(2)
        };
        let local = compat::get_iface_by_ifindex(send_ifindex).ok_or(ERRNO::ENODEV)?;
        if let Some(bound_ifindex) = bound_ifindex {
            if send_ifindex != bound_ifindex {
                return Err(ERRNO::ENODEV);
            }
        }

        if let Some((src_mac, src_ip, dst_ip)) = read_arp_ipv4_request(data.as_slice()) {
            if compat::lookup_peer_target(c_name(&local.name), dst_ip) {
                if let Some(peer) = compat::find_iface_by_ipv4(dst_ip) {
                    let reply = build_arp_ipv4_reply(peer.mac, dst_ip, src_mac, src_ip);
                    let mut from = SockAddrLl {
                        sll_family: AF_PACKET_FAMILY,
                        sll_protocol: ETH_P_ARP.to_be(),
                        sll_ifindex: local.ifindex as i32,
                        sll_hatype: ARPHRD_ETHER,
                        sll_pkttype: PACKET_HOST,
                        sll_halen: 6,
                        sll_addr: [0; 8],
                    };
                    from.sll_addr[..6].copy_from_slice(&peer.mac);
                    *self.pending.lock() = Some(PendingPacket { data: reply, from });
                }
            }
        }

        Ok(data.len())
    }

    pub(crate) fn recv_into_user_buffer(
        &self,
        buf: &mut UserBuffer,
    ) -> Result<(usize, SockAddrLl), ERRNO> {
        let pending = self.pending.lock().take().ok_or(ERRNO::EAGAIN)?;
        let copied = write_user_buffer(buf, pending.data.as_slice());
        Ok((copied, pending.from))
    }
}

impl File for CompatIfreqSocketFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn ioctl(&self, req: usize, arg: usize) -> Result<isize, ERRNO> {
        compat_ifreq_ioctl(req, arg)
    }

    fn stat(&self) -> Stat {
        socket_stat(self.poll_source_id())
    }
}

pub(crate) struct NetlinkRouteSocketFile {
    pending: SpinNoIrqLock<Vec<u8>>,
}

impl NetlinkRouteSocketFile {
    fn handle_newlink(
        &self,
        hdr: &NlMsghdr,
        buf: &[u8],
        replies: &mut Vec<u8>,
    ) -> Result<(), ERRNO> {
        if buf.len() < size_of::<NlMsghdr>() + size_of::<IfInfoMsg>() {
            return Err(ERRNO::EINVAL);
        }
        let info = unsafe { &*(buf[size_of::<NlMsghdr>()..].as_ptr() as *const IfInfoMsg) };
        let attrs = &buf[size_of::<NlMsghdr>() + size_of::<IfInfoMsg>()..];
        let mut ifnames = Vec::new();
        let mut link_kind: Option<String> = None;
        let mut has_netns_pid = false;
        scan_link_attrs(attrs, &mut ifnames, &mut link_kind, &mut has_netns_pid);
        if link_kind.as_deref() == Some("veth") && ifnames.len() < 2 {
            let mut nested_ifnames = Vec::new();
            scan_nested_ifnames(attrs, &mut nested_ifnames);
            for name in nested_ifnames {
                if !ifnames.iter().any(|existing| existing == &name) {
                    ifnames.push(name);
                }
            }
        }

        if info.ifi_index > 0 {
            let iface =
                compat::get_iface_by_ifindex(info.ifi_index as usize).ok_or(ERRNO::ENODEV)?;
            if info.ifi_change & IFF_UP != 0 {
                compat::set_link_up(c_name(&iface.name), (info.ifi_flags & IFF_UP) != 0)?;
            }
            let _ = has_netns_pid;
        } else if link_kind.as_deref() == Some("veth") {
            let left = ifnames[0].clone();
            let right = if ifnames.len() >= 2 {
                ifnames[1].clone()
            } else {
                derive_veth_peer_name(left.as_str())
            };
            compat::create_veth_pair(left.as_str(), right.as_str())?;
        } else {
            debug!(
                "netlink newlink unsupported create: ifindex={} kind={:?} ifnames={:?} flags={:#x} change={:#x}",
                info.ifi_index,
                link_kind,
                ifnames,
                info.ifi_flags,
                info.ifi_change
            );
            return Err(ERRNO::EOPNOTSUPP);
        }

        maybe_push_ack(hdr, 0, replies);
        Ok(())
    }

    fn handle_newaddr(
        &self,
        hdr: &NlMsghdr,
        buf: &[u8],
        replies: &mut Vec<u8>,
    ) -> Result<(), ERRNO> {
        if buf.len() < size_of::<NlMsghdr>() + size_of::<IfAddrMsg>() {
            return Err(ERRNO::EINVAL);
        }
        let info = unsafe { &*(buf[size_of::<NlMsghdr>()..].as_ptr() as *const IfAddrMsg) };
        let iface = compat::get_iface_by_ifindex(info.ifa_index as usize).ok_or(ERRNO::ENODEV)?;
        let attrs = &buf[size_of::<NlMsghdr>() + size_of::<IfAddrMsg>()..];
        let mut ipv4: Option<[u8; 4]> = None;
        for_each_attr(attrs, |kind, payload| {
            if (kind == IFA_LOCAL || kind == IFA_ADDRESS) && payload.len() >= 4 && ipv4.is_none() {
                ipv4 = Some([payload[0], payload[1], payload[2], payload[3]]);
            }
        });
        let ip = ipv4.ok_or(ERRNO::EINVAL)?;
        compat::set_addr(c_name(&iface.name), ip, info.ifa_prefixlen)?;
        maybe_push_ack(hdr, 0, replies);
        Ok(())
    }

    fn handle_deladdr(
        &self,
        hdr: &NlMsghdr,
        buf: &[u8],
        replies: &mut Vec<u8>,
    ) -> Result<(), ERRNO> {
        if buf.len() < size_of::<NlMsghdr>() + size_of::<IfAddrMsg>() {
            return Err(ERRNO::EINVAL);
        }
        let info = unsafe { &*(buf[size_of::<NlMsghdr>()..].as_ptr() as *const IfAddrMsg) };
        let iface = compat::get_iface_by_ifindex(info.ifa_index as usize).ok_or(ERRNO::ENODEV)?;
        compat::flush_addr(c_name(&iface.name))?;
        maybe_push_ack(hdr, 0, replies);
        Ok(())
    }

    fn handle_send(&self, buf: &[u8]) -> Result<usize, ERRNO> {
        if buf.len() < size_of::<NlMsghdr>() {
            return Err(ERRNO::EINVAL);
        }
        let mut replies = Vec::new();
        let mut off = 0usize;
        while off + size_of::<NlMsghdr>() <= buf.len() {
            let hdr = unsafe { &*(buf[off..].as_ptr() as *const NlMsghdr) };
            let msg_len = hdr.nlmsg_len as usize;
            if msg_len < size_of::<NlMsghdr>() || off + msg_len > buf.len() {
                return Err(ERRNO::EINVAL);
            }
            let msg = &buf[off..off + msg_len];
            match hdr.nlmsg_type {
                RTM_NEWLINK => self.handle_newlink(hdr, msg, &mut replies)?,
                RTM_GETLINK => replies.extend_from_slice(build_link_dump(hdr.nlmsg_seq).as_slice()),
                RTM_NEWROUTE | RTM_DELROUTE => maybe_push_ack(hdr, 0, &mut replies),
                RTM_GETROUTE => replies.extend_from_slice(build_done(hdr.nlmsg_seq).as_slice()),
                RTM_NEWADDR => self.handle_newaddr(hdr, msg, &mut replies)?,
                RTM_DELADDR => self.handle_deladdr(hdr, msg, &mut replies)?,
                RTM_GETADDR => replies.extend_from_slice(build_addr_dump(hdr.nlmsg_seq).as_slice()),
                _ => {
                    debug!("netlink route unsupported nlmsg_type={}", hdr.nlmsg_type);
                    return Err(ERRNO::EOPNOTSUPP);
                }
            }
            off = off.saturating_add(nlmsg_align(msg_len));
        }
        *self.pending.lock() = replies;
        Ok(buf.len())
    }

    pub(crate) fn send_user_buffer(&self, buf: &UserBuffer) -> Result<usize, ERRNO> {
        let mut data = Vec::with_capacity(buf.len());
        for chunk in buf.buffers.iter() {
            data.extend_from_slice(chunk);
        }
        self.handle_send(&data)
    }

    pub(crate) fn recv_into_user_buffer(
        &self,
        buf: &mut UserBuffer,
        peek: bool,
    ) -> Result<usize, ERRNO> {
        let pending = self.pending.lock().clone();
        if pending.is_empty() {
            return Err(ERRNO::EAGAIN);
        }
        let copied = write_user_buffer(buf, &pending);
        if !peek {
            self.pending.lock().drain(..copied);
        }
        Ok(copied)
    }

    fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }
}

impl File for NetlinkRouteSocketFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read_at_result(&self, _offset: usize, mut buf: UserBuffer) -> Result<usize, ERRNO> {
        self.recv_into_user_buffer(&mut buf, false)
    }

    fn write_at_result(&self, _offset: usize, buf: UserBuffer) -> Result<usize, ERRNO> {
        self.send_user_buffer(&buf)
    }

    fn poll(&self, events: u16) -> u16 {
        let mut ready = 0u16;
        if events & 0x001 != 0 && self.has_pending() {
            ready |= 0x001;
        }
        if events & 0x004 != 0 {
            ready |= 0x004;
        }
        ready
    }

    fn stat(&self) -> Stat {
        socket_stat(self.poll_source_id())
    }
}

pub(crate) fn create_packet_socket_file(socket_type: i32) -> Arc<PacketSocketFile> {
    Arc::new(PacketSocketFile {
        socket_type,
        binding: SpinNoIrqLock::new(None),
        pending: SpinNoIrqLock::new(None),
    })
}

pub(crate) fn create_compat_ifreq_socket_file() -> Arc<CompatIfreqSocketFile> {
    Arc::new(CompatIfreqSocketFile)
}

pub(crate) fn create_netlink_route_socket_file() -> Arc<NetlinkRouteSocketFile> {
    Arc::new(NetlinkRouteSocketFile {
        pending: SpinNoIrqLock::new(Vec::new()),
    })
}
