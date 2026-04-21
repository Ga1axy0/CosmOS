//! Kernel networking stack based on smoltcp.
//!
//! Design highlights:
//! - Driver side uses VirtIO token-based completion.
//! - Socket side uses per-socket wait queues + poll source notifications.

mod tcp;
mod udp;
mod unix_socket;

use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lazy_static::lazy_static;
use smoltcp::{
    iface::{Config, Interface, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::{tcp as tcp_socket, udp as udp_socket},
    time::Instant,
    wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpListenEndpoint, Ipv4Address},
};

use crate::{
    drivers,
    poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT},
    sync::SpinNoIrqLock,
    timer::get_time_ms,
};

pub(crate) use tcp::{create_tcp_socket_file, TcpSocketFile, TcpSocketState};
pub(crate) use udp::{create_udp_socket_file, UdpSocketFile, UdpSocketState};
pub use unix_socket::{
    UnixSocketAncillaryData, UnixSocketPairEnd, UnixUcred, SCM_CREDENTIALS, SCM_RIGHTS, SocketLevel
};

const RX_BUF_LEN: usize = 2048;
const MAX_SOCKETS: usize = 256;
const UDP_RX_META: usize = 16;
const UDP_TX_META: usize = 16;
const UDP_BUF: usize = 4096;
const TCP_RX_BUF: usize = 8192;
const TCP_TX_BUF: usize = 8192;

const EPHEMERAL_PORT_START: u16 = 49152;
const EPHEMERAL_PORT_END: u16 = 65535;

const NO_POLL_DEADLINE_MS: u64 = u64::MAX;

const LOOPBACK_OCTET: u8 = 127;

const ETH_HEADER_LEN: usize = 14;
const ETH_TYPE_IPV4: u16 = 0x0800;
const ETH_TYPE_ARP: u16 = 0x0806;

const ARP_FRAME_LEN: usize = ETH_HEADER_LEN + 28;
const ARP_OP_REQUEST: u16 = 1;
const ARP_OP_REPLY: u16 = 2;

// Kernel UDP echo feature (for quick network stack testing).
const ENABLE_KERNEL_UDP_ECHO: bool = true;
const KERNEL_UDP_ECHO_PORT: u16 = 5555;

lazy_static! {
    /// Global network stack instance.
    pub(crate) static ref NET_STACK: SpinNoIrqLock<Option<NetStack>> = SpinNoIrqLock::new(None);
}

/// Whether one immediate poll is needed due to IRQ or recent TX activity.
pub(crate) static NEED_POLL: AtomicBool = AtomicBool::new(false);
/// Next soft deadline (ms since boot) for calling into smoltcp.
/// `u64::MAX` means no timer-driven deadline currently exists.
pub(crate) static NEXT_POLL_DEADLINE_MS: AtomicU64 = AtomicU64::new(NO_POLL_DEADLINE_MS);

/// Userspace-visible IPv4 socket address layout.
///
/// `sin_port` and `sin_addr` are in network byte order.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct SockAddrIn {
    /// Address family (`AF_INET`).
    pub sin_family: u16,
    /// Port in network byte order.
    pub sin_port: u16,
    /// IPv4 address in network byte order.
    pub sin_addr: u32,
    /// Padding.
    pub sin_zero: [u8; 8],
}

/// Initialize the network stack if a VirtIO network device is present.
pub fn init() {
    let dev = drivers::net::with_device(Arc::clone);
    let Some(dev) = dev else {
        info!("[kernel] net: no virtio-net device, skip stack init");
        return;
    };

    let stack = NetStack::new(dev);
    *NET_STACK.lock() = Some(stack);
    NEED_POLL.store(true, Ordering::Release);
    NEXT_POLL_DEADLINE_MS.store(0, Ordering::Release);
    info!("[kernel] net: smoltcp stack initialized");
}

/// Notify the net stack that one NIC IRQ has arrived.
pub fn notify_irq() {
    NEED_POLL.store(true, Ordering::Release);
    NEXT_POLL_DEADLINE_MS.store(0, Ordering::Release);
}

/// Poll network stack once.
///
/// Call this from a safe context (e.g. timer interrupt path or scheduler tick).
pub fn poll() {
    let now_ms = get_time_ms() as u64;
    let need_immediate = NEED_POLL.swap(false, Ordering::AcqRel);
    let deadline_ms = NEXT_POLL_DEADLINE_MS.load(Ordering::Acquire);
    let deadline_due = deadline_ms != NO_POLL_DEADLINE_MS && now_ms >= deadline_ms;

    if !need_immediate && !deadline_due {
        return;
    }

    let mut guard = NET_STACK.lock();
    let Some(stack) = guard.as_mut() else {
        return;
    };

    stack.poll();
}

pub(crate) struct NetStack {
    device: VirtioSmoltcpDevice,
    pub(crate) iface: Interface,
    pub(crate) sockets: SocketSet<'static>,
    pub(crate) udp_states: Vec<Arc<UdpSocketState>>,
    pub(crate) tcp_states: Vec<Arc<TcpSocketState>>,
    pub(crate) echo_udp: Option<smoltcp::iface::SocketHandle>,
    next_ephemeral_port: u16,
}

impl NetStack {
    fn new(dev: Arc<drivers::net::VirtIONetDevice>) -> Self {
        let mac = dev.mac_address();
        let eth = EthernetAddress(mac);

        let mut device = VirtioSmoltcpDevice::new(dev);
        let now = now();
        let mut cfg = Config::new(HardwareAddress::Ethernet(eth));
        cfg.random_seed = 0x5A5A_1234;
        let mut iface = Interface::new(cfg, &mut device, now);

        // QEMU user networking defaults.
        iface.update_ip_addrs(|addrs| {
            let cidr = IpCidr::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 15)), 24);
            if addrs.iter().all(|a| *a != cidr) {
                let _ = addrs.push(cidr);
            }

            // Add loopback IPv4 on the same interface so smoltcp can pick
            // a loopback source address for 127/8 destinations.
            let loopback = IpCidr::new(IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)), 8);
            if addrs.iter().all(|a| *a != loopback) {
                let _ = addrs.push(loopback);
            }
        });
        let _ = iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2));

        let storage_vec: Vec<smoltcp::iface::SocketStorage<'static>> =
            (0..MAX_SOCKETS).map(|_| smoltcp::iface::SocketStorage::EMPTY).collect();
        let storage = Box::leak(storage_vec.into_boxed_slice());
        let sockets = SocketSet::new(storage);

        let mut stack = Self {
            device,
            iface,
            sockets,
            udp_states: Vec::new(),
            tcp_states: Vec::new(),
            echo_udp: None,
            next_ephemeral_port: EPHEMERAL_PORT_START,
        };

        // Optionally create a kernel UDP echo socket bound to the configured port.
        if ENABLE_KERNEL_UDP_ECHO {
            let (h, _st) = stack.create_udp_socket();
            let bound = {
                let socket = stack.sockets.get_mut::<udp_socket::Socket>(h);
                socket
                    .bind(KERNEL_UDP_ECHO_PORT)
                    .is_ok()
            };
            if bound {
                stack.echo_udp = Some(h);
                info!("[kernel] net: UDP echo enabled on port {}", KERNEL_UDP_ECHO_PORT);
            } else {
                // binding failed (port in use) — remove the socket we created.
                stack.remove_udp_socket(h);
                info!("[kernel] net: UDP echo disabled, port {} unavailable", KERNEL_UDP_ECHO_PORT);
            }
        }

        stack
    }

    fn poll(&mut self) {
        // print!("p");
        let ts = now();
        let _ = self.iface.poll(ts, &mut self.device, &mut self.sockets);

        for st in self.udp_states.iter() {
            let mut ready = 0u16;
            {
                let socket = self.sockets.get_mut::<udp_socket::Socket>(st.handle);
                if socket.can_recv() {
                    st.read_wait.wake_one();
                    ready |= POLLIN;

                    // If this is the kernel echo socket, consume incoming packets and
                    // send back the reversed payload to the sender.
                    if self.echo_udp == Some(st.handle) {
                        while socket.can_recv() {
                            if let Ok((data, meta)) = socket.recv() {
                                // Reverse the payload bytes and attempt to send back.
                                let mut rev = Vec::from(data);
                                rev.reverse();
                                if socket.can_send() {
                                    let _ = socket.send_slice(&rev, meta.endpoint);
                                    NEED_POLL.store(true, Ordering::Release);
                                }
                            } else {
                                break;
                            }
                        }
                    }
                }
                if socket.can_send() {
                    st.write_wait.wake_one();
                    ready |= POLLOUT;
                }
            }
            if ready != 0 {
                notify_poll_source(st.source_id(), ready);
            }
        }

        for st in self.tcp_states.iter() {
            let mut ready = 0u16;
            let mut listener_source_id = None;
            let mut listener_owned = false;
            let open;
            {
                let socket = self.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                let state = socket.state();
                if st.is_listener_owned() {
                    listener_owned = true;
                    listener_source_id = tcp::queue_listener_connection_if_ready(st, state);
                } else {
                    if socket.can_recv() {
                        st.read_wait.wake_one();
                        ready |= POLLIN;
                    }
                    let may_recv = socket.may_recv();
                    if !may_recv {
                        st.read_wait.wake_all();
                        ready |= POLLIN | POLLHUP;
                    }
                    if socket.can_send() || !socket.may_send() {
                        st.write_wait.wake_one();
                        ready |= POLLOUT;
                    }
                }
                open = socket.is_open();
            }
            if let Some(source_id) = listener_source_id {
                notify_poll_source(source_id, POLLIN);
            }
            if !listener_owned && ready != 0 {
                notify_poll_source(st.source_id(), ready);
            }
            if st.orphaned.load(Ordering::Acquire) && !open {
                notify_poll_source(st.source_id(), POLLIN | POLLOUT | POLLHUP);
            }
        }

        // Garbage collect orphaned TCP sockets after fully closed.
        let mut remove_tcp: Vec<smoltcp::iface::SocketHandle> = Vec::new();
        for st in self.tcp_states.iter() {
            if !st.orphaned.load(Ordering::Acquire) {
                continue;
            }
            let open = {
                let socket = self.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                socket.is_open()
            };
            if !open {
                remove_tcp.push(st.handle);
            }
        }
        if !remove_tcp.is_empty() {
            for h in remove_tcp.iter().copied() {
                let _ = self.sockets.remove(h);
            }
            self.tcp_states
                .retain(|st| !remove_tcp.iter().any(|h| *h == st.handle));
        }

        self.refresh_poll_deadline(ts);
    }

    fn refresh_poll_deadline(&mut self, ts: Instant) {
        let next = self
            .iface
            .poll_at(ts, &self.sockets)
            .map(|t| t.total_millis().max(0) as u64)
            .unwrap_or(NO_POLL_DEADLINE_MS);
        NEXT_POLL_DEADLINE_MS.store(next, Ordering::Release);

        // Keep liveness for immediate work units (e.g. handshake progress)
        // even when there's no external IRQ.
        if next != NO_POLL_DEADLINE_MS && (get_time_ms() as u64) >= next {
            NEED_POLL.store(true, Ordering::Release);
        }
    }

    pub(crate) fn create_udp_socket(
        &mut self,
    ) -> (smoltcp::iface::SocketHandle, Arc<UdpSocketState>) {
        let rx_meta = vec![udp_socket::PacketMetadata::EMPTY; UDP_RX_META];
        let tx_meta = vec![udp_socket::PacketMetadata::EMPTY; UDP_TX_META];
        let rx_buf = vec![0u8; UDP_BUF];
        let tx_buf = vec![0u8; UDP_BUF];
        let udp = udp_socket::Socket::new(
            udp_socket::PacketBuffer::new(rx_meta, rx_buf),
            udp_socket::PacketBuffer::new(tx_meta, tx_buf),
        );
        let handle = self.sockets.add(udp);
        let st = Arc::new(UdpSocketState::new(handle));
        self.udp_states.push(Arc::clone(&st));
        (handle, st)
    }

    pub(crate) fn remove_udp_socket(&mut self, handle: smoltcp::iface::SocketHandle) {
        let _ = self.sockets.remove(handle);
        self.udp_states.retain(|s| s.handle != handle);
    }

    pub(crate) fn create_tcp_socket(
        &mut self,
    ) -> (smoltcp::iface::SocketHandle, Arc<TcpSocketState>) {
        let rx = tcp_socket::SocketBuffer::new(vec![0u8; TCP_RX_BUF]);
        let tx = tcp_socket::SocketBuffer::new(vec![0u8; TCP_TX_BUF]);
        let tcp = tcp_socket::Socket::new(rx, tx);
        let handle = self.sockets.add(tcp);
        let st = Arc::new(TcpSocketState::new(handle));
        self.tcp_states.push(Arc::clone(&st));
        (handle, st)
    }

    pub(crate) fn alloc_ephemeral_port(&mut self) -> u16 {
        let port = self.next_ephemeral_port;
        self.next_ephemeral_port = if port == EPHEMERAL_PORT_END {
            EPHEMERAL_PORT_START
        } else {
            port + 1
        };
        port
    }
}

#[inline]
fn now() -> Instant {
    Instant::from_millis(get_time_ms() as i64)
}

#[inline]
fn is_loopback_ipv4_bytes(ip: &[u8]) -> bool {
    ip.len() == 4 && ip[0] == LOOPBACK_OCTET
}

struct LoopbackDataplane {
    rxq: SpinNoIrqLock<VecDeque<Vec<u8>>>,
}

impl LoopbackDataplane {
    fn new() -> Self {
        Self {
            rxq: SpinNoIrqLock::new(VecDeque::new()),
        }
    }

    fn push_rx(&self, frame: Vec<u8>) {
        self.rxq.lock().push_back(frame);
    }

    fn pop_rx(&self) -> Option<Vec<u8>> {
        self.rxq.lock().pop_front()
    }
}

struct VirtioSmoltcpDevice {
    dev: Arc<drivers::net::VirtIONetDevice>,
    caps: DeviceCapabilities,
    loopback: Arc<LoopbackDataplane>,
    mac: [u8; 6],
}

impl VirtioSmoltcpDevice {
    fn new(dev: Arc<drivers::net::VirtIONetDevice>) -> Self {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1500;
        Self {
            mac: dev.mac_address(),
            dev,
            caps,
            loopback: Arc::new(LoopbackDataplane::new()),
        }
    }
}

impl Device for VirtioSmoltcpDevice {
    type RxToken<'a>
        = VirtioRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = VirtioTxToken
    where
        Self: 'a;

    fn capabilities(&self) -> DeviceCapabilities {
        self.caps.clone()
    }

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if let Some(buf) = self.loopback.pop_rx() {
            return Some((
                VirtioRxToken { buf },
                VirtioTxToken {
                    dev: Arc::clone(&self.dev),
                    loopback: Arc::clone(&self.loopback),
                    mac: self.mac,
                },
            ));
        }

        let mut buf = vec![0u8; RX_BUF_LEN];
        let len = self.dev.try_recv(buf.as_mut_slice())?;
        buf.truncate(len);
        Some((
            VirtioRxToken { buf },
            VirtioTxToken {
                dev: Arc::clone(&self.dev),
                loopback: Arc::clone(&self.loopback),
                mac: self.mac,
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.dev.can_send() {
            Some(VirtioTxToken {
                dev: Arc::clone(&self.dev),
                loopback: Arc::clone(&self.loopback),
                mac: self.mac,
            })
        } else {
            None
        }
    }
}

struct VirtioRxToken {
    buf: Vec<u8>,
}

impl RxToken for VirtioRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(self.buf.as_mut_slice())
    }
}

struct VirtioTxToken {
    dev: Arc<drivers::net::VirtIONetDevice>,
    loopback: Arc<LoopbackDataplane>,
    mac: [u8; 6],
}

impl TxToken for VirtioTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let ret = f(buf.as_mut_slice());

        if self.try_handle_loopback_tx(buf.as_slice()) {
            NEED_POLL.store(true, Ordering::Release);
            return ret;
        }

        match self.dev.try_send(buf.as_slice()) {
            Ok(true) => {}
            Ok(false) => {
                trace!("net: tx queue busy, drop one frame");
            }
            Err(e) => {
                warn!("net: try_send failed: {:?}", e);
            }
        }
        NEED_POLL.store(true, Ordering::Release);
        ret
    }
}

impl VirtioTxToken {
    fn try_handle_loopback_tx(&self, frame: &[u8]) -> bool {
        if frame.len() < ETH_HEADER_LEN {
            return false;
        }

        let ethertype = u16::from_be_bytes([frame[12], frame[13]]);
        match ethertype {
            ETH_TYPE_IPV4 => self.handle_ipv4_loopback(frame),
            ETH_TYPE_ARP => self.handle_arp_loopback(frame),
            _ => false,
        }
    }

    fn handle_ipv4_loopback(&self, frame: &[u8]) -> bool {
        const IPV4_MIN_HEADER_LEN: usize = 20;

        if frame.len() < ETH_HEADER_LEN + IPV4_MIN_HEADER_LEN {
            return false;
        }

        let ihl = ((frame[ETH_HEADER_LEN] & 0x0f) as usize) * 4;
        if ihl < IPV4_MIN_HEADER_LEN || frame.len() < ETH_HEADER_LEN + ihl {
            return false;
        }

        let dst_ip = &frame[ETH_HEADER_LEN + 16..ETH_HEADER_LEN + 20];
        if !is_loopback_ipv4_bytes(dst_ip) {
            return false;
        }

        let mut looped = frame.to_vec();
        looped[..6].copy_from_slice(&self.mac);
        looped[6..12].copy_from_slice(&self.mac);
        self.loopback.push_rx(looped);
        true
    }

    fn handle_arp_loopback(&self, frame: &[u8]) -> bool {
        if frame.len() < ARP_FRAME_LEN {
            return false;
        }

        // Ethernet(1)/IPv4(0x0800), hlen=6, plen=4
        if frame[14..20] != [0x00, 0x01, 0x08, 0x00, 0x06, 0x04] {
            return false;
        }

        let op = u16::from_be_bytes([frame[20], frame[21]]);
        let sender_mac = &frame[22..28];
        let sender_ip = &frame[28..32];
        let target_ip = &frame[38..42];

        let target_is_loopback = is_loopback_ipv4_bytes(target_ip);
        let sender_is_loopback = is_loopback_ipv4_bytes(sender_ip);

        // Resolve loopback ARP locally: synthesize ARP reply and enqueue it
        // into our software RX path.
        if op == ARP_OP_REQUEST && target_is_loopback {
            let mut reply = frame.to_vec();

            // Ethernet header
            reply[..6].copy_from_slice(sender_mac);
            reply[6..12].copy_from_slice(&self.mac);

            // ARP payload
            reply[20..22].copy_from_slice(&ARP_OP_REPLY.to_be_bytes());
            reply[22..28].copy_from_slice(&self.mac);
            reply[28..32].copy_from_slice(target_ip);
            reply[32..38].copy_from_slice(sender_mac);
            reply[38..42].copy_from_slice(sender_ip);

            self.loopback.push_rx(reply);
            return true;
        }

        // A request with loopback sender protocol address must never be sent
        // onto external Ethernet.
        if op == ARP_OP_REQUEST && sender_is_loopback {
            trace!("net: drop external ARP request with loopback sender IP");
            return true;
        }

        false
    }
}
