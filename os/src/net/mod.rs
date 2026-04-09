//! Kernel networking stack based on smoltcp.
//!
//! Design highlights:
//! - Driver side uses VirtIO token-based completion.
//! - Socket side uses per-socket wait queues + poll source notifications.

mod tcp;
mod udp;

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use lazy_static::lazy_static;
use smoltcp::{
    iface::{Config, Interface, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken},
    socket::{tcp as tcp_socket, udp as udp_socket},
    time::Instant,
    wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, Ipv4Address},
};

use crate::{
    drivers,
    poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT},
    sync::SpinNoIrqLock,
    timer::get_time_ms,
};

pub(crate) use tcp::{create_tcp_socket_file, TcpSocketFile, TcpSocketState};
pub(crate) use udp::{create_udp_socket_file, UdpSocketFile, UdpSocketState};

const RX_BUF_LEN: usize = 2048;
const MAX_SOCKETS: usize = 64;
const UDP_RX_META: usize = 16;
const UDP_TX_META: usize = 16;
const UDP_BUF: usize = 4096;
const TCP_RX_BUF: usize = 8192;
const TCP_TX_BUF: usize = 8192;

const EPHEMERAL_PORT_START: u16 = 49152;
const EPHEMERAL_PORT_END: u16 = 65535;

lazy_static! {
    /// Global network stack instance.
    pub(crate) static ref NET_STACK: SpinNoIrqLock<Option<NetStack>> = unsafe {
        SpinNoIrqLock::new(None)
    };
}

/// Whether one immediate poll is needed due to IRQ or recent TX activity.
pub(crate) static NEED_POLL: AtomicBool = AtomicBool::new(false);

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
    info!("[kernel] net: smoltcp stack initialized");
}

/// Notify the net stack that one NIC IRQ has arrived.
pub fn notify_irq() {
    NEED_POLL.store(true, Ordering::Release);
}

/// Poll network stack once.
///
/// Call this from a safe context (e.g. timer interrupt path or scheduler tick).
pub fn poll() {
    let mut guard = NET_STACK.lock();
    let Some(stack) = guard.as_mut() else {
        return;
    };

    let _ = NEED_POLL.swap(false, Ordering::AcqRel);
    stack.poll();
}

pub(crate) struct NetStack {
    device: VirtioSmoltcpDevice,
    pub(crate) iface: Interface,
    pub(crate) sockets: SocketSet<'static>,
    pub(crate) udp_states: Vec<Arc<UdpSocketState>>,
    pub(crate) tcp_states: Vec<Arc<TcpSocketState>>,
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
        });
        let _ = iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2));

        let storage_vec: Vec<smoltcp::iface::SocketStorage<'static>> =
            (0..MAX_SOCKETS).map(|_| smoltcp::iface::SocketStorage::EMPTY).collect();
        let storage = Box::leak(storage_vec.into_boxed_slice());
        let sockets = SocketSet::new(storage);

        Self {
            device,
            iface,
            sockets,
            udp_states: Vec::new(),
            tcp_states: Vec::new(),
            next_ephemeral_port: EPHEMERAL_PORT_START,
        }
    }

    fn poll(&mut self) {
        let _ = self.iface.poll(now(), &mut self.device, &mut self.sockets);

        for st in self.udp_states.iter() {
            let mut ready = 0u16;
            {
                let socket = self.sockets.get_mut::<udp_socket::Socket>(st.handle);
                if socket.can_recv() {
                    st.read_wait.wake_one();
                    ready |= POLLIN;
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
            let mut may_recv = true;
            let mut open = true;
            {
                let socket = self.sockets.get_mut::<tcp_socket::Socket>(st.handle);
                if socket.can_recv() {
                    st.read_wait.wake_one();
                    ready |= POLLIN;
                }
                may_recv = socket.may_recv();
                if !may_recv {
                    st.read_wait.wake_all();
                    ready |= POLLIN | POLLHUP;
                }
                if socket.can_send() || !socket.may_send() {
                    st.write_wait.wake_one();
                    ready |= POLLOUT;
                }
                open = socket.is_open();
            }
            if ready != 0 {
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

struct VirtioSmoltcpDevice {
    dev: Arc<drivers::net::VirtIONetDevice>,
    caps: DeviceCapabilities,
}

impl VirtioSmoltcpDevice {
    fn new(dev: Arc<drivers::net::VirtIONetDevice>) -> Self {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = 1500;
        Self { dev, caps }
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
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(self.buf.as_mut_slice())
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
        let ret = f(buf.as_mut_slice());
        let _ = self.dev.send(buf.as_slice());
        NEED_POLL.store(true, Ordering::Release);
        ret
    }
}
