//! Kernel networking stack based on smoltcp.
//!
//! Design highlights:
//! - Driver side uses VirtIO token-based completion.
//! - Socket side uses per-socket wait queues + poll source notifications.

mod af_alg;
mod loopback;
mod socket_timeout;
mod tcp;
mod udp;
mod unix_socket;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lazy_static::lazy_static;
use smoltcp::{
    iface::{Config, Interface, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::{tcp as tcp_socket, udp as udp_socket},
    time::Instant,
    wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address},
};

use crate::{
    drivers,
    poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT},
    sync::SpinNoIrqLock,
    timer::get_time_us,
};

pub(crate) use af_alg::{
    create_alg_socket_file, AlgRequestFile, AlgSendMsgParams, AlgSocketFile, AF_ALG,
    ALG_OP_DECRYPT, ALG_OP_ENCRYPT, ALG_SET_AEAD_ASSOCLEN, ALG_SET_IV, ALG_SET_KEY,
    ALG_SET_OP, SOCK_SEQPACKET, SOL_ALG,
};
pub(crate) use tcp::{create_tcp_socket_file, TcpSocketFile, TcpSocketState};
pub(crate) use udp::{create_udp_socket_file, UdpSocketFile, UdpSocketState};
pub(crate) use socket_timeout::{
    cleanup_socket_wait, handle_socket_wait_timeout, register_socket_wait, socket_wait_mark_ready,
    socket_wait_should_skip, socket_wait_state, timeout_ns_to_deadline_ns, SocketTimerTag,
    SocketWakeState,
};
pub(crate) use unix_socket::create_unix_stream_socket_file;
pub use unix_socket::{
    UnixSocketAncillaryData, UnixSocketPairEnd, UnixUcred, SCM_CREDENTIALS, SCM_RIGHTS,
    SocketLevel,
};

const RX_BUF_LEN: usize = 32 * 1024;
const MAX_SOCKETS: usize = 256;
const UDP_RX_META: usize = 512;
const UDP_TX_META: usize = 512;
const UDP_BUF: usize = 64 * 1024;
const TCP_RX_BUF: usize = 128 * 1024;
const TCP_TX_BUF: usize = 128 * 1024;

const EPHEMERAL_PORT_START: u16 = 49152;
const EPHEMERAL_PORT_END: u16 = 65535;
const MAX_IMMEDIATE_POLLS: usize = 4;

const NO_POLL_DEADLINE_US: u64 = u64::MAX;

// Kernel UDP echo feature (for quick network stack testing).
const ENABLE_KERNEL_UDP_ECHO: bool = true;
const KERNEL_UDP_ECHO_PORT: u16 = 5555;

lazy_static! {
    /// Global network stack instance.
    pub(crate) static ref NET_STACK: SpinNoIrqLock<Option<NetStack>> = SpinNoIrqLock::new(None);
}

/// Whether one immediate poll is needed due to IRQ or recent TX activity.
pub(crate) static NEED_POLL: AtomicBool = AtomicBool::new(false);
/// Next soft deadline (us since boot) for calling into smoltcp.
/// `u64::MAX` means no timer-driven deadline currently exists.
pub(crate) static NEXT_POLL_DEADLINE_US: AtomicU64 = AtomicU64::new(NO_POLL_DEADLINE_US);

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
    NEXT_POLL_DEADLINE_US.store(0, Ordering::Release);
    info!("[kernel] net: smoltcp stack initialized");
}

/// Notify the net stack that one NIC IRQ has arrived.
pub fn notify_irq() {
    NEED_POLL.store(true, Ordering::Release);
    NEXT_POLL_DEADLINE_US.store(0, Ordering::Release);
}

/// Poll network stack once.
///
/// Call this from a safe context (e.g. timer interrupt path or scheduler tick).
pub fn poll() {
    // print!("p");
    let now_us = get_time_us() as u64;
    let need_immediate = NEED_POLL.swap(false, Ordering::AcqRel);
    let deadline_us = NEXT_POLL_DEADLINE_US.load(Ordering::Acquire);
    let deadline_due = deadline_us != NO_POLL_DEADLINE_US && now_us >= deadline_us;

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
    device: MultiDevice,
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

        let mut device = MultiDevice::new(dev);
        let now = now();
        let mut cfg = Config::new(HardwareAddress::Ethernet(eth));
        cfg.random_seed = 0x5A5A_1234;
        let mut iface = Interface::new(cfg, &mut device, now);

        // Configure both external and loopback addresses on the same interface
        iface.update_ip_addrs(|addrs| {
            // External network (QEMU user networking)
            let external = IpCidr::new(IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 15)), 24);
            if addrs.iter().all(|a| *a != external) {
                let _ = addrs.push(external);
            }

            // Loopback address
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
        for _ in 0..MAX_IMMEDIATE_POLLS {
            self.poll_once();

            // Loopback packets and newly queued socket work often need another
            // immediate pass to become visible to recv()/poll() callers.
            let need_repoll =
                !self.device.loopback.queue.is_empty() || NEED_POLL.swap(false, Ordering::AcqRel);
            if !need_repoll {
                return;
            }
        }

        if !self.device.loopback.queue.is_empty() {
            NEED_POLL.store(true, Ordering::Release);
        }
    }

    fn poll_once(&mut self) {
        let ts = now();
        let poll_result = self.iface.poll(ts, &mut self.device, &mut self.sockets);
        trace!("NetStack::poll result={:?}, loopback queue len after={}", poll_result, self.device.loopback.queue.len());

        for st in self.udp_states.iter() {
            let mut ready = 0u16;
            {
                let socket = self.sockets.get_mut::<udp_socket::Socket>(st.handle);
                if socket.can_recv() {
                    // debug!("UDP socket {:?} can_recv, endpoint={:?}", st.handle, socket.endpoint());
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
            .map(|t| t.total_micros().max(0) as u64)
            .unwrap_or(NO_POLL_DEADLINE_US);
        NEXT_POLL_DEADLINE_US.store(next, Ordering::Release);

        // Keep liveness for immediate work units (e.g. handshake progress)
        // even when there's no external IRQ.
        if next != NO_POLL_DEADLINE_US && (get_time_us() as u64) >= next {
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
        trace!("Created UDP socket with handle {:?}", handle);
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
    Instant::from_micros(get_time_us() as i64)
}

/// Multi-device that routes packets between VirtIO (external) and Loopback (local).
struct MultiDevice {
    virtio: VirtioSmoltcpDevice,
    loopback: loopback::Loopback,
}

impl MultiDevice {
    fn new(virtio_dev: Arc<drivers::net::VirtIONetDevice>) -> Self {
        Self {
            virtio: VirtioSmoltcpDevice::new(virtio_dev),
            loopback: loopback::Loopback::new(Medium::Ethernet),
        }
    }
}

impl Device for MultiDevice {
    type RxToken<'a> = MultiRxToken where Self: 'a;
    type TxToken<'a> = MultiTxToken<'a> where Self: 'a;

    fn capabilities(&self) -> DeviceCapabilities {
        self.virtio.capabilities()
    }

    fn receive(&mut self, timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Try loopback first for lower latency on local traffic
        if let Some((rx, _tx)) = self.loopback.receive(timestamp) {
            return Some((
                MultiRxToken::Loopback(rx),
                MultiTxToken {
                    virtio: &mut self.virtio,
                    loopback: &mut self.loopback,
                },
            ));
        }

        // Then try VirtIO
        if let Some((rx, _tx)) = self.virtio.receive(timestamp) {
            return Some((
                MultiRxToken::Virtio(rx),
                MultiTxToken {
                    virtio: &mut self.virtio,
                    loopback: &mut self.loopback,
                },
            ));
        }

        None
    }

    fn transmit(&mut self, timestamp: Instant) -> Option<Self::TxToken<'_>> {
        // We can always transmit (will route based on destination)
        let virtio_can = self.virtio.transmit(timestamp).is_some();
        let loopback_can = self.loopback.transmit(timestamp).is_some();

        if virtio_can || loopback_can {
            Some(MultiTxToken {
                virtio: &mut self.virtio,
                loopback: &mut self.loopback,
            })
        } else {
            None
        }
    }
}

enum MultiRxToken {
    Virtio(VirtioRxToken),
    Loopback(loopback::RxToken),
}

impl RxToken for MultiRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        match self {
            MultiRxToken::Virtio(token) => token.consume(f),
            MultiRxToken::Loopback(token) => token.consume(f),
        }
    }
}

struct MultiTxToken<'a> {
    virtio: &'a mut VirtioSmoltcpDevice,
    loopback: &'a mut loopback::Loopback,
}

impl<'a> TxToken for MultiTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let ret = f(&mut buf);

        // Route based on destination: check if it's a loopback packet
        let is_loopback = if buf.len() >= 34 {
            let ethertype = u16::from_be_bytes([buf[12], buf[13]]);
            match ethertype {
                0x0800 => {
                    // IPv4: check destination IP (buf[30] is first byte of dest IP)
                    buf[30] == 127
                }
                0x0806 => {
                    // ARP: check target protocol address (TPA)
                    // ARP frame structure:
                    // [14-15] Hardware type
                    // [16-17] Protocol type
                    // [18] Hardware address length
                    // [19] Protocol address length
                    // [20-21] Operation
                    // [22-27] Sender hardware address (SHA)
                    // [28-31] Sender protocol address (SPA)
                    // [32-37] Target hardware address (THA)
                    // [38-41] Target protocol address (TPA)
                    if buf.len() >= 42 {
                        // Check if TPA (target IP) is 127.x.x.x
                        buf[38] == 127
                    } else {
                        false
                    }
                }
                _ => false,
            }
        } else {
            false
        };

        trace!("Consume token of len {}, loopback: {}, buf[12] = {:x}, buf[13] = {:x}, buf[30] = {:x}", len, is_loopback, buf[12], buf[13], buf[30]);

        if is_loopback {
            // Directly push to loopback queue (our custom Loopback has a public queue field)
            trace!("Pushing to loopback queue, current len={}", self.loopback.queue.len());
            self.loopback.queue.push_back(buf);
            trace!("Loopback queue len after push={}", self.loopback.queue.len());
        } else {
            // Send to VirtIO using the standard path
            match self.virtio.dev.try_send(&buf) {
                Ok(true) => {}
                Ok(false) => {
                    trace!("net: tx queue busy, drop one frame");
                }
                Err(e) => {
                    warn!("net: try_send failed: {:?}", e);
                }
            }
        }

        NEED_POLL.store(true, Ordering::Release);
        ret
    }
}

struct VirtioSmoltcpDevice {
    dev: Arc<drivers::net::VirtIONetDevice>,
    caps: DeviceCapabilities,
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
        let mut buf = vec![0u8; RX_BUF_LEN];
        let len = self.dev.try_recv(buf.as_mut_slice())?;
        buf.truncate(len);
        Some((
            VirtioRxToken { buf },
            VirtioTxToken {
                dev: Arc::clone(&self.dev),
            },
        ))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        if self.dev.can_send() {
            Some(VirtioTxToken {
                dev: Arc::clone(&self.dev),
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
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(self.buf.as_slice())
    }
}

struct VirtioTxToken {
    dev: Arc<drivers::net::VirtIONetDevice>,
}

impl TxToken for VirtioTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let ret = f(&mut buf);

        match self.dev.try_send(&buf) {
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
