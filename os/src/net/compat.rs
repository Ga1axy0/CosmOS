use alloc::string::{String, ToString};
use alloc::vec::Vec;

use lazy_static::lazy_static;

use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;

const IFACE_NAME_MAX: usize = 32;

#[derive(Clone, Debug)]
struct CompatNetIf {
    name: String,
    ifindex: usize,
    peer: Option<String>,
    up: bool,
    ipv4: Option<[u8; 4]>,
    prefix: u8,
    mac: [u8; 6],
    mtu: u32,
}

#[derive(Clone, Debug)]
struct CompatNetState {
    next_ifindex: usize,
    ifaces: Vec<CompatNetIf>,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CompatNetIfInfo {
    pub(crate) ifindex: usize,
    pub(crate) up: u8,
    pub(crate) prefix: u8,
    pub(crate) has_peer: u8,
    pub(crate) has_ipv4: u8,
    pub(crate) mtu: u32,
    pub(crate) name: [u8; IFACE_NAME_MAX],
    pub(crate) peer: [u8; IFACE_NAME_MAX],
    pub(crate) ipv4: [u8; 4],
    pub(crate) mac: [u8; 6],
}

lazy_static! {
    static ref COMPAT_NET_STATE: SpinNoIrqLock<CompatNetState> =
        SpinNoIrqLock::new(default_state());
}

fn default_state() -> CompatNetState {
    CompatNetState {
        next_ifindex: 3,
        ifaces: alloc::vec![
            CompatNetIf {
                name: String::from("lo"),
                ifindex: 1,
                peer: None,
                up: true,
                ipv4: Some([127, 0, 0, 1]),
                prefix: 8,
                mac: [0, 0, 0, 0, 0, 0],
                mtu: 65536,
            },
            CompatNetIf {
                name: String::from("eth0"),
                ifindex: 2,
                peer: None,
                up: true,
                ipv4: Some([10, 0, 2, 15]),
                prefix: 24,
                mac: [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
                mtu: 1500,
            },
        ],
    }
}

fn copy_name(dst: &mut [u8; IFACE_NAME_MAX], src: &str) {
    let bytes = src.as_bytes();
    let len = bytes.len().min(IFACE_NAME_MAX.saturating_sub(1));
    dst[..len].copy_from_slice(&bytes[..len]);
    if len < IFACE_NAME_MAX {
        dst[len] = 0;
    }
}

fn iface_to_info(iface: &CompatNetIf) -> CompatNetIfInfo {
    let mut info = CompatNetIfInfo {
        ifindex: iface.ifindex,
        up: iface.up as u8,
        prefix: iface.prefix,
        has_peer: iface.peer.is_some() as u8,
        has_ipv4: iface.ipv4.is_some() as u8,
        mtu: iface.mtu,
        ipv4: iface.ipv4.unwrap_or([0; 4]),
        mac: iface.mac,
        ..Default::default()
    };
    copy_name(&mut info.name, iface.name.as_str());
    if let Some(peer) = iface.peer.as_deref() {
        copy_name(&mut info.peer, peer);
    }
    info
}

fn synthetic_veth_mac(ifindex: usize) -> [u8; 6] {
    [
        0x02,
        0x00,
        ((ifindex >> 16) & 0xff) as u8,
        ((ifindex >> 8) & 0xff) as u8,
        (ifindex & 0xff) as u8,
        0x01,
    ]
}

pub(crate) fn set_eth0_mac(mac: [u8; 6]) {
    let mut state = COMPAT_NET_STATE.lock();
    if let Some(iface) = state.ifaces.iter_mut().find(|iface| iface.name == "eth0") {
        iface.mac = mac;
    }
}

pub(crate) fn list_ifaces() -> Vec<CompatNetIfInfo> {
    COMPAT_NET_STATE
        .lock()
        .ifaces
        .iter()
        .map(iface_to_info)
        .collect()
}

pub(crate) fn get_iface_info(name: &str) -> Option<CompatNetIfInfo> {
    COMPAT_NET_STATE
        .lock()
        .ifaces
        .iter()
        .find(|iface| iface.name == name)
        .map(iface_to_info)
}

pub(crate) fn get_iface_by_ifindex(ifindex: usize) -> Option<CompatNetIfInfo> {
    COMPAT_NET_STATE
        .lock()
        .ifaces
        .iter()
        .find(|iface| iface.ifindex == ifindex)
        .map(iface_to_info)
}

pub(crate) fn find_iface_by_ipv4(ip: [u8; 4]) -> Option<CompatNetIfInfo> {
    COMPAT_NET_STATE
        .lock()
        .ifaces
        .iter()
        .find(|iface| iface.ipv4 == Some(ip))
        .map(iface_to_info)
}

pub(crate) fn create_veth_pair(left: &str, right: &str) -> Result<(), ERRNO> {
    let mut state = COMPAT_NET_STATE.lock();
    if state
        .ifaces
        .iter()
        .any(|iface| iface.name == left || iface.name == right)
    {
        return Err(ERRNO::EEXIST);
    }
    let left_idx = state.next_ifindex;
    let right_idx = state.next_ifindex + 1;
    state.next_ifindex += 2;
    state.ifaces.push(CompatNetIf {
        name: left.to_string(),
        ifindex: left_idx,
        peer: Some(right.to_string()),
        up: false,
        ipv4: None,
        prefix: 0,
        mac: synthetic_veth_mac(left_idx),
        mtu: 1500,
    });
    state.ifaces.push(CompatNetIf {
        name: right.to_string(),
        ifindex: right_idx,
        peer: Some(left.to_string()),
        up: false,
        ipv4: None,
        prefix: 0,
        mac: synthetic_veth_mac(right_idx),
        mtu: 1500,
    });
    Ok(())
}

pub(crate) fn set_link_up(name: &str, up: bool) -> Result<(), ERRNO> {
    let mut state = COMPAT_NET_STATE.lock();
    let iface = state
        .ifaces
        .iter_mut()
        .find(|iface| iface.name == name)
        .ok_or(ERRNO::ENODEV)?;
    iface.up = up;
    Ok(())
}

pub(crate) fn flush_addr(name: &str) -> Result<(), ERRNO> {
    let mut state = COMPAT_NET_STATE.lock();
    let iface = state
        .ifaces
        .iter_mut()
        .find(|iface| iface.name == name)
        .ok_or(ERRNO::ENODEV)?;
    iface.ipv4 = None;
    iface.prefix = 0;
    Ok(())
}

pub(crate) fn set_addr(name: &str, ip: [u8; 4], prefix: u8) -> Result<(), ERRNO> {
    let mut state = COMPAT_NET_STATE.lock();
    let iface = state
        .ifaces
        .iter_mut()
        .find(|iface| iface.name == name)
        .ok_or(ERRNO::ENODEV)?;
    iface.ipv4 = Some(ip);
    iface.prefix = prefix;
    Ok(())
}

pub(crate) fn lookup_peer_target(source_name: &str, target_ip: [u8; 4]) -> bool {
    let state = COMPAT_NET_STATE.lock();
    let Some(source) = state.ifaces.iter().find(|iface| iface.name == source_name) else {
        return false;
    };
    if !source.up {
        return false;
    }
    let Some(peer_name) = source.peer.as_deref() else {
        return false;
    };
    let Some(peer) = state.ifaces.iter().find(|iface| iface.name == peer_name) else {
        return false;
    };
    peer.up && peer.ipv4 == Some(target_ip)
}
