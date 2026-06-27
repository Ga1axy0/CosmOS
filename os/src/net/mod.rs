//! Kernel networking stack based on smoltcp.
//!
//! Design highlights:
//! - Driver side uses VirtIO token-based completion.
//! - Socket side uses per-socket wait queues + poll source notifications.

mod af_alg;
pub(crate) mod compat;
mod compat_socket;
mod loopback;
mod raw_ipv6;
mod socket_timeout;
mod tcp;
mod udp;
mod unix_socket;

#[cfg(feature = "net_perf_counters")]
use alloc::string::String;
use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
#[cfg(feature = "net_perf_counters")]
use core::fmt::Write;
#[cfg(feature = "net_perf_counters")]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use lazy_static::lazy_static;
use smoltcp::{
    iface::{Config, Interface, SocketSet},
    phy::{Device, DeviceCapabilities, Medium, PacketMeta, RxToken, TxToken},
    socket::{tcp as tcp_socket, udp as udp_socket},
    time::Instant,
    wire::{
        EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, Ipv4Address, Ipv6Address,
    },
};

use crate::{
    drivers,
    poll::{notify_poll_source, POLLHUP, POLLIN, POLLOUT},
    sync::SpinNoIrqLock,
    timer::get_time_us,
};

pub(crate) use af_alg::{
    create_alg_socket_file, AlgRequestFile, AlgSendMsgParams, AlgSocketFile, AF_ALG,
    ALG_OP_DECRYPT, ALG_OP_ENCRYPT, ALG_SET_AEAD_ASSOCLEN, ALG_SET_IV, ALG_SET_KEY, ALG_SET_OP,
    SOCK_SEQPACKET, SOL_ALG,
};
pub(crate) use compat_socket::{
    compat_ifreq_ioctl, create_compat_ifreq_socket_file, create_netlink_route_socket_file,
    create_packet_socket_file, CompatIfreqSocketFile, NetlinkRouteSocketFile, PacketSocketFile,
    SockAddrLl,
};
pub(crate) use raw_ipv6::{
    create_raw_ipv6_socket_file, In6PktInfo, RawIpv6ControlMessage, RawIpv6SendMeta,
    RawIpv6SocketFile, SockAddrIn6, AF_INET6, ICMP6_FILTER, IPPROTO_ICMPV6, IPV6_2292DSTOPTS,
    IPV6_2292HOPLIMIT, IPV6_2292HOPOPTS, IPV6_2292PKTINFO, IPV6_2292RTHDR, IPV6_CHECKSUM,
    IPV6_HOPLIMIT, IPV6_PKTINFO, IPV6_RECVDSTOPTS, IPV6_RECVHOPLIMIT, IPV6_RECVHOPOPTS,
    IPV6_RECVPKTINFO, IPV6_RECVRTHDR, IPV6_RECVTCLASS, IPV6_TCLASS, SOL_IPV6,
};
pub(crate) use socket_timeout::{
    cleanup_socket_wait, handle_socket_wait_timeout, register_socket_wait, socket_wait_mark_ready,
    socket_wait_should_skip, socket_wait_state, timeout_ns_to_deadline_ns, SocketTimerTag,
    SocketWakeState,
};
pub(crate) use tcp::{create_tcp_socket_file, TcpSocketFile, TcpSocketState};
pub(crate) use udp::{create_udp_socket_file, UdpSocketFile, UdpSocketState};
pub(crate) use unix_socket::{
    create_unix_datagram_socket_file, create_unix_stream_socket_file, unix_stream_listener,
    UnixDatagramSocketFile,
};
pub use unix_socket::{
    SocketLevel, UnixSocketAncillaryData, UnixSocketPairEnd, UnixUcred, SCM_CREDENTIALS, SCM_RIGHTS,
};

const RX_BUF_LEN: usize = 32 * 1024;
const MAX_SOCKETS: usize = 256;
const UDP_RX_META: usize = 2048;
const UDP_TX_META: usize = 512;
const UDP_RX_BUF: usize = 512 * 1024;
const UDP_TX_BUF: usize = 64 * 1024;
const TCP_RX_BUF: usize = 512 * 1024;
const TCP_TX_BUF: usize = 512 * 1024;

pub(crate) const AF_INET: u16 = 2;
const EPHEMERAL_PORT_START: u16 = 49152;
const EPHEMERAL_PORT_END: u16 = 65535;
const MAX_IMMEDIATE_POLLS: usize = 64;
const MAX_SOCKET_IMMEDIATE_POLLS: usize = 8;
const MAX_SOCKET_CATCHUP_POLLS: usize = 32;
const MAX_PASSIVE_LISTEN_SOCKETS: usize = 16;

const NO_POLL_DEADLINE_US: u64 = u64::MAX;
const IPV6_LOOPBACK_SOLICITED_NODE: [u8; 16] = [
    0xff, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0xff, 0x00, 0x00, 0x01,
];

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

#[cfg(feature = "net_perf_counters")]
static PERF_POLL_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_POLL_SOCKET_WORK_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_POLL_SOCKET_WORK_DEEP: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_POLL_SOCKET_WORK_LIGHT: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_POLL_SOCKET_WORK_CATCHUP: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_POLL_SOCKET_RECV_WORK: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_POLL_SOCKET_WORK_ACTIVE_SUM: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_POLL_SOCKET_WORK_ACTIVE_MAX: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_POLL_ONCE_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_POLL_BUDGET_EXHAUSTED: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_LOOPBACK_TX_FRAMES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_LOOPBACK_TX_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_LOOPBACK_RX_FRAMES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_LOOPBACK_RX_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_LOOPBACK_MAX_QUEUE: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_VIRTIO_TX_FRAMES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_VIRTIO_TX_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_VIRTIO_RX_FRAMES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_VIRTIO_RX_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_UDP_DIRECT_PKTS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_UDP_DIRECT_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_UDP_DIRECT_DROPS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_UDP_USER_SEND_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_UDP_USER_SEND_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_UDP_USER_RECV_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_UDP_USER_RECV_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_TCP_USER_SEND_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_TCP_USER_SEND_BYTES: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_TCP_USER_RECV_CALLS: AtomicUsize = AtomicUsize::new(0);
#[cfg(feature = "net_perf_counters")]
static PERF_TCP_USER_RECV_BYTES: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "net_perf_counters")]
#[inline]
fn perf_load(counter: &AtomicUsize) -> usize {
    counter.load(Ordering::Relaxed)
}

#[cfg(feature = "net_perf_counters")]
#[inline]
fn perf_inc(counter: &AtomicUsize) {
    counter.fetch_add(1, Ordering::Relaxed);
}

#[cfg(feature = "net_perf_counters")]
#[inline]
fn perf_add(counter: &AtomicUsize, value: usize) {
    counter.fetch_add(value, Ordering::Relaxed);
}

#[cfg(feature = "net_perf_counters")]
#[inline]
fn perf_update_max(counter: &AtomicUsize, value: usize) {
    let mut current = counter.load(Ordering::Relaxed);
    while value > current {
        match counter.compare_exchange_weak(current, value, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return,
            Err(next) => current = next,
        }
    }
}

#[cfg(feature = "net_perf_counters")]
#[inline]
pub(crate) fn perf_tcp_user_send(bytes: usize) {
    perf_inc(&PERF_TCP_USER_SEND_CALLS);
    perf_add(&PERF_TCP_USER_SEND_BYTES, bytes);
}

#[cfg(not(feature = "net_perf_counters"))]
#[inline]
pub(crate) fn perf_tcp_user_send(_bytes: usize) {}

#[cfg(feature = "net_perf_counters")]
#[inline]
pub(crate) fn perf_tcp_user_recv(bytes: usize) {
    perf_inc(&PERF_TCP_USER_RECV_CALLS);
    perf_add(&PERF_TCP_USER_RECV_BYTES, bytes);
}

#[cfg(not(feature = "net_perf_counters"))]
#[inline]
pub(crate) fn perf_tcp_user_recv(_bytes: usize) {}

#[cfg(feature = "net_perf_counters")]
#[inline]
pub(crate) fn perf_udp_user_send(bytes: usize) {
    perf_inc(&PERF_UDP_USER_SEND_CALLS);
    perf_add(&PERF_UDP_USER_SEND_BYTES, bytes);
}

#[cfg(not(feature = "net_perf_counters"))]
#[inline]
pub(crate) fn perf_udp_user_send(_bytes: usize) {}

#[cfg(feature = "net_perf_counters")]
#[inline]
pub(crate) fn perf_udp_user_recv(bytes: usize) {
    perf_inc(&PERF_UDP_USER_RECV_CALLS);
    perf_add(&PERF_UDP_USER_RECV_BYTES, bytes);
}

#[cfg(not(feature = "net_perf_counters"))]
#[inline]
pub(crate) fn perf_udp_user_recv(_bytes: usize) {}

#[cfg(feature = "net_perf_counters")]
pub(crate) fn reset_perf_counters() {
    for counter in [
        &PERF_POLL_CALLS,
        &PERF_POLL_SOCKET_WORK_CALLS,
        &PERF_POLL_SOCKET_WORK_DEEP,
        &PERF_POLL_SOCKET_WORK_LIGHT,
        &PERF_POLL_SOCKET_WORK_CATCHUP,
        &PERF_POLL_SOCKET_RECV_WORK,
        &PERF_POLL_SOCKET_WORK_ACTIVE_SUM,
        &PERF_POLL_SOCKET_WORK_ACTIVE_MAX,
        &PERF_POLL_ONCE_CALLS,
        &PERF_POLL_BUDGET_EXHAUSTED,
        &PERF_LOOPBACK_TX_FRAMES,
        &PERF_LOOPBACK_TX_BYTES,
        &PERF_LOOPBACK_RX_FRAMES,
        &PERF_LOOPBACK_RX_BYTES,
        &PERF_LOOPBACK_MAX_QUEUE,
        &PERF_VIRTIO_TX_FRAMES,
        &PERF_VIRTIO_TX_BYTES,
        &PERF_VIRTIO_RX_FRAMES,
        &PERF_VIRTIO_RX_BYTES,
        &PERF_UDP_DIRECT_PKTS,
        &PERF_UDP_DIRECT_BYTES,
        &PERF_UDP_DIRECT_DROPS,
        &PERF_UDP_USER_SEND_CALLS,
        &PERF_UDP_USER_SEND_BYTES,
        &PERF_UDP_USER_RECV_CALLS,
        &PERF_UDP_USER_RECV_BYTES,
        &PERF_TCP_USER_SEND_CALLS,
        &PERF_TCP_USER_SEND_BYTES,
        &PERF_TCP_USER_RECV_CALLS,
        &PERF_TCP_USER_RECV_BYTES,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

#[cfg(feature = "net_perf_counters")]
pub(crate) fn render_perf_counters() -> String {
    let mut out = String::new();
    let _ = writeln!(&mut out, "net:");
    let _ = writeln!(&mut out, "  poll_calls {}", perf_load(&PERF_POLL_CALLS));
    let _ = writeln!(
        &mut out,
        "  poll_socket_work_calls {}",
        perf_load(&PERF_POLL_SOCKET_WORK_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  poll_socket_work_deep {}",
        perf_load(&PERF_POLL_SOCKET_WORK_DEEP)
    );
    let _ = writeln!(
        &mut out,
        "  poll_socket_work_light {}",
        perf_load(&PERF_POLL_SOCKET_WORK_LIGHT)
    );
    let _ = writeln!(
        &mut out,
        "  poll_socket_work_catchup {}",
        perf_load(&PERF_POLL_SOCKET_WORK_CATCHUP)
    );
    let _ = writeln!(
        &mut out,
        "  poll_socket_recv_work {}",
        perf_load(&PERF_POLL_SOCKET_RECV_WORK)
    );
    let _ = writeln!(
        &mut out,
        "  poll_socket_work_active_sum {}",
        perf_load(&PERF_POLL_SOCKET_WORK_ACTIVE_SUM)
    );
    let _ = writeln!(
        &mut out,
        "  poll_socket_work_active_max {}",
        perf_load(&PERF_POLL_SOCKET_WORK_ACTIVE_MAX)
    );
    let _ = writeln!(
        &mut out,
        "  poll_once_calls {}",
        perf_load(&PERF_POLL_ONCE_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  poll_budget_exhausted {}",
        perf_load(&PERF_POLL_BUDGET_EXHAUSTED)
    );
    let _ = writeln!(&mut out, "loopback:");
    let _ = writeln!(
        &mut out,
        "  tx_frames {}",
        perf_load(&PERF_LOOPBACK_TX_FRAMES)
    );
    let _ = writeln!(
        &mut out,
        "  tx_bytes {}",
        perf_load(&PERF_LOOPBACK_TX_BYTES)
    );
    let _ = writeln!(
        &mut out,
        "  rx_frames {}",
        perf_load(&PERF_LOOPBACK_RX_FRAMES)
    );
    let _ = writeln!(
        &mut out,
        "  rx_bytes {}",
        perf_load(&PERF_LOOPBACK_RX_BYTES)
    );
    let _ = writeln!(
        &mut out,
        "  max_queue_len {}",
        perf_load(&PERF_LOOPBACK_MAX_QUEUE)
    );
    let _ = writeln!(&mut out, "virtio:");
    let _ = writeln!(
        &mut out,
        "  tx_frames {}",
        perf_load(&PERF_VIRTIO_TX_FRAMES)
    );
    let _ = writeln!(&mut out, "  tx_bytes {}", perf_load(&PERF_VIRTIO_TX_BYTES));
    let _ = writeln!(
        &mut out,
        "  rx_frames {}",
        perf_load(&PERF_VIRTIO_RX_FRAMES)
    );
    let _ = writeln!(&mut out, "  rx_bytes {}", perf_load(&PERF_VIRTIO_RX_BYTES));
    let _ = writeln!(&mut out, "udp:");
    let _ = writeln!(
        &mut out,
        "  direct_packets {}",
        perf_load(&PERF_UDP_DIRECT_PKTS)
    );
    let _ = writeln!(
        &mut out,
        "  direct_bytes {}",
        perf_load(&PERF_UDP_DIRECT_BYTES)
    );
    let _ = writeln!(
        &mut out,
        "  direct_drops {}",
        perf_load(&PERF_UDP_DIRECT_DROPS)
    );
    let _ = writeln!(
        &mut out,
        "  user_send_calls {}",
        perf_load(&PERF_UDP_USER_SEND_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  user_send_bytes {}",
        perf_load(&PERF_UDP_USER_SEND_BYTES)
    );
    let _ = writeln!(
        &mut out,
        "  user_recv_calls {}",
        perf_load(&PERF_UDP_USER_RECV_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  user_recv_bytes {}",
        perf_load(&PERF_UDP_USER_RECV_BYTES)
    );
    let _ = writeln!(&mut out, "tcp:");
    let _ = writeln!(
        &mut out,
        "  user_send_calls {}",
        perf_load(&PERF_TCP_USER_SEND_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  user_send_bytes {}",
        perf_load(&PERF_TCP_USER_SEND_BYTES)
    );
    let _ = writeln!(
        &mut out,
        "  user_recv_calls {}",
        perf_load(&PERF_TCP_USER_RECV_CALLS)
    );
    let _ = writeln!(
        &mut out,
        "  user_recv_bytes {}",
        perf_load(&PERF_TCP_USER_RECV_BYTES)
    );
    render_tcp_state_snapshot(&mut out);
    out
}

#[cfg(feature = "net_perf_counters")]
fn render_tcp_state_snapshot(out: &mut String) {
    let mut guard = NET_STACK.lock();
    let Some(stack) = guard.as_mut() else {
        let _ = writeln!(out, "tcp_state_current:");
        let _ = writeln!(out, "  unavailable 1");
        return;
    };

    let mut total = 0usize;
    let mut closed = 0usize;
    let mut listen = 0usize;
    let mut syn_sent = 0usize;
    let mut syn_received = 0usize;
    let mut established = 0usize;
    let mut fin_wait1 = 0usize;
    let mut fin_wait2 = 0usize;
    let mut close_wait = 0usize;
    let mut closing = 0usize;
    let mut last_ack = 0usize;
    let mut time_wait = 0usize;
    let mut listener_owned = 0usize;
    let mut orphaned = 0usize;

    for st in stack.tcp_states.iter() {
        total += 1;
        if st.is_listener_owned() {
            listener_owned += 1;
        }
        if st.orphaned.load(Ordering::Relaxed) {
            orphaned += 1;
        }
        let socket = stack.sockets.get_mut::<tcp_socket::Socket>(st.handle);
        match socket.state() {
            tcp_socket::State::Closed => closed += 1,
            tcp_socket::State::Listen => listen += 1,
            tcp_socket::State::SynSent => syn_sent += 1,
            tcp_socket::State::SynReceived => syn_received += 1,
            tcp_socket::State::Established => established += 1,
            tcp_socket::State::FinWait1 => fin_wait1 += 1,
            tcp_socket::State::FinWait2 => fin_wait2 += 1,
            tcp_socket::State::CloseWait => close_wait += 1,
            tcp_socket::State::Closing => closing += 1,
            tcp_socket::State::LastAck => last_ack += 1,
            tcp_socket::State::TimeWait => time_wait += 1,
        }
    }

    let _ = writeln!(out, "tcp_state_current:");
    let _ = writeln!(out, "  total {}", total);
    let _ = writeln!(out, "  closed {}", closed);
    let _ = writeln!(out, "  listen {}", listen);
    let _ = writeln!(out, "  syn_sent {}", syn_sent);
    let _ = writeln!(out, "  syn_received {}", syn_received);
    let _ = writeln!(out, "  established {}", established);
    let _ = writeln!(out, "  fin_wait1 {}", fin_wait1);
    let _ = writeln!(out, "  fin_wait2 {}", fin_wait2);
    let _ = writeln!(out, "  close_wait {}", close_wait);
    let _ = writeln!(out, "  closing {}", closing);
    let _ = writeln!(out, "  last_ack {}", last_ack);
    let _ = writeln!(out, "  time_wait {}", time_wait);
    let _ = writeln!(out, "  listener_owned {}", listener_owned);
    let _ = writeln!(out, "  orphaned {}", orphaned);
}

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
    #[cfg(feature = "net_perf_counters")]
    perf_inc(&PERF_POLL_CALLS);
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

/// Poll from the periodic timer path only when smoltcp has pending work.
pub fn poll_timer_tick() {
    if !NEED_POLL.load(Ordering::Acquire) {
        let deadline_us = NEXT_POLL_DEADLINE_US.load(Ordering::Acquire);
        if deadline_us == NO_POLL_DEADLINE_US {
            return;
        }
        if (get_time_us() as u64) < deadline_us {
            return;
        }
    }

    poll();
}

/// Return `(tcp, udp)` live socket-state counts for diagnostics
/// (e.g. `/proc/mm_perf`). Returns `(0, 0)` before the stack is initialized.
pub fn socket_state_counts() -> (usize, usize) {
    let guard = NET_STACK.lock();
    match guard.as_ref() {
        Some(stack) => (stack.tcp_states.len(), stack.udp_states.len()),
        None => (0, 0),
    }
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
        compat::set_eth0_mac(mac);
        let eth = EthernetAddress(mac);

        let mut device = MultiDevice::new(dev);
        let now = now();
        let mut cfg = Config::new(HardwareAddress::Ethernet(eth));
        cfg.random_seed = 0x5A5A_1234;
        let mut iface = Interface::new(cfg, &mut device, now);

        // Configure both external and loopback addresses on the same interface.
        // Rebuild the list explicitly so IPv6 localhost cannot be dropped silently.
        iface.update_ip_addrs(|addrs| {
            addrs.clear();

            // External network (QEMU user networking)
            addrs
                .push(IpCidr::new(
                    IpAddress::Ipv4(Ipv4Address::new(10, 0, 2, 15)),
                    24,
                ))
                .expect("failed to configure external IPv4 address");

            // IPv4 loopback
            addrs
                .push(IpCidr::new(
                    IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
                    8,
                ))
                .expect("failed to configure IPv4 loopback address");

            // IPv6 loopback
            addrs
                .push(IpCidr::new(IpAddress::Ipv6(Ipv6Address::LOCALHOST), 128))
                .expect("failed to configure IPv6 loopback address");
        });
        let _ = iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(10, 0, 2, 2));
        info!("[kernel] net: iface addresses = {:?}", iface.ip_addrs());

        let storage_vec: Vec<smoltcp::iface::SocketStorage<'static>> = (0..MAX_SOCKETS)
            .map(|_| smoltcp::iface::SocketStorage::EMPTY)
            .collect();
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
                socket.bind(KERNEL_UDP_ECHO_PORT).is_ok()
            };
            if bound {
                stack.echo_udp = Some(h);
                info!(
                    "[kernel] net: UDP echo enabled on port {}",
                    KERNEL_UDP_ECHO_PORT
                );
            } else {
                // binding failed (port in use) — remove the socket we created.
                stack.remove_udp_socket(h);
                info!(
                    "[kernel] net: UDP echo disabled, port {} unavailable",
                    KERNEL_UDP_ECHO_PORT
                );
            }
        }

        stack
    }

    fn poll(&mut self) {
        self.poll_with_budget(MAX_IMMEDIATE_POLLS);
    }

    pub(crate) fn poll_socket_work_for(&mut self, handle: smoltcp::iface::SocketHandle) {
        #[cfg(feature = "net_perf_counters")]
        perf_inc(&PERF_POLL_SOCKET_WORK_CALLS);
        let active = self.active_tcp_socket_count();
        #[cfg(feature = "net_perf_counters")]
        perf_add(&PERF_POLL_SOCKET_WORK_ACTIVE_SUM, active);
        #[cfg(feature = "net_perf_counters")]
        perf_update_max(&PERF_POLL_SOCKET_WORK_ACTIVE_MAX, active);
        if active <= 2 {
            #[cfg(feature = "net_perf_counters")]
            perf_inc(&PERF_POLL_SOCKET_WORK_DEEP);
            self.poll_with_budget(MAX_IMMEDIATE_POLLS);
            return;
        } else {
            #[cfg(feature = "net_perf_counters")]
            perf_inc(&PERF_POLL_SOCKET_WORK_LIGHT);
            let queued_before = self
                .sockets
                .get_mut::<tcp_socket::Socket>(handle)
                .send_queue();
            self.poll_with_budget(MAX_SOCKET_IMMEDIATE_POLLS);
            let socket = self.sockets.get_mut::<tcp_socket::Socket>(handle);
            let queued_after = socket.send_queue();
            if queued_after > 0
                && (queued_after >= queued_before.saturating_sub(queued_before / 4)
                    || queued_after >= TCP_TX_BUF / 8
                    || !socket.can_send())
            {
                #[cfg(feature = "net_perf_counters")]
                perf_inc(&PERF_POLL_SOCKET_WORK_CATCHUP);
                self.poll_with_budget(MAX_SOCKET_CATCHUP_POLLS);
            }
        }
    }

    pub(crate) fn poll_socket_recv_work(&mut self) {
        #[cfg(feature = "net_perf_counters")]
        perf_inc(&PERF_POLL_SOCKET_RECV_WORK);
        self.poll_with_budget(MAX_SOCKET_IMMEDIATE_POLLS);
    }

    fn active_tcp_socket_count(&mut self) -> usize {
        let mut count = 0usize;
        for st in self.tcp_states.iter() {
            if st.is_listener_owned() {
                continue;
            }
            let socket = self.sockets.get_mut::<tcp_socket::Socket>(st.handle);
            if matches!(socket.state(), tcp_socket::State::Established) {
                count += 1;
            }
        }
        count
    }

    fn poll_with_budget(&mut self, budget: usize) {
        for _ in 0..budget {
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
            #[cfg(feature = "net_perf_counters")]
            perf_inc(&PERF_POLL_BUDGET_EXHAUSTED);
            NEED_POLL.store(true, Ordering::Release);
        }
    }

    fn poll_once(&mut self) {
        #[cfg(feature = "net_perf_counters")]
        perf_inc(&PERF_POLL_ONCE_CALLS);
        let ts = now();
        if !self.device.loopback.queue.is_empty() {
            debug!(
                "NetStack::poll_once entering with loopback queue len={}",
                self.device.loopback.queue.len()
            );
        }
        let poll_result = self.iface.poll(ts, &mut self.device, &mut self.sockets);
        debug!(
            "NetStack::poll result={:?}, loopback queue len after={}",
            poll_result,
            self.device.loopback.queue.len()
        );

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
                if let Some(prev) = st.observe_state_change(state) {
                    debug!(
                        "tcp socket {:?} state {} -> {} listener_owned={} open={} local={:?} remote={:?}",
                        st.handle,
                        tcp::tcp_state_name_repr(prev),
                        tcp::tcp_state_name(state),
                        st.is_listener_owned(),
                        socket.is_open(),
                        socket.local_endpoint(),
                        socket.remote_endpoint()
                    );
                }
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
        let rx_buf = vec![0u8; UDP_RX_BUF];
        let tx_buf = vec![0u8; UDP_TX_BUF];
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

    pub(crate) fn deliver_udp_loopback(
        &mut self,
        source_handle: smoltcp::iface::SocketHandle,
        dst: IpEndpoint,
        payload: &[u8],
    ) -> bool {
        if !is_loopback_ip(dst.addr) {
            return false;
        }

        let source = self.sockets.get_mut::<udp_socket::Socket>(source_handle);
        let source_endpoint = source.endpoint();
        let source_addr = source_endpoint
            .addr
            .unwrap_or_else(|| loopback_addr_for(dst.addr));
        let remote = IpEndpoint::new(source_addr, source_endpoint.port);
        let metadata = udp_socket::UdpMetadata {
            endpoint: remote,
            local_address: Some(dst.addr),
            meta: PacketMeta::default(),
        };

        let mut best: Option<(u8, Arc<UdpSocketState>)> = None;
        for st in self.udp_states.iter() {
            let socket = self.sockets.get_mut::<udp_socket::Socket>(st.handle);
            let endpoint = socket.endpoint();
            if endpoint.port != dst.port {
                continue;
            }

            let addr_matches = match endpoint.addr {
                Some(addr) => addr == dst.addr,
                None => true,
            };
            if !addr_matches {
                continue;
            }

            let priority = match socket.remote_endpoint() {
                Some(peer) if peer == remote => 3,
                Some(_) => continue,
                None if endpoint.addr.is_some() => 2,
                None => 1,
            };

            if best
                .as_ref()
                .map(|(best_priority, _)| priority > *best_priority)
                .unwrap_or(true)
            {
                best = Some((priority, Arc::clone(st)));
            }
        }

        let Some((_, target)) = best else {
            return false;
        };

        let socket = self.sockets.get_mut::<udp_socket::Socket>(target.handle);
        if socket.inject_recv_slice(payload, metadata).is_ok() {
            #[cfg(feature = "net_perf_counters")]
            perf_inc(&PERF_UDP_DIRECT_PKTS);
            #[cfg(feature = "net_perf_counters")]
            perf_add(&PERF_UDP_DIRECT_BYTES, payload.len());
            target.read_wait.wake_one();
            notify_poll_source(target.source_id(), POLLIN);
        } else {
            #[cfg(feature = "net_perf_counters")]
            perf_inc(&PERF_UDP_DIRECT_DROPS);
        }
        true
    }

    pub(crate) fn create_tcp_socket(
        &mut self,
    ) -> (smoltcp::iface::SocketHandle, Arc<TcpSocketState>) {
        let rx = tcp_socket::SocketBuffer::new(vec![0u8; TCP_RX_BUF]);
        let tx = tcp_socket::SocketBuffer::new(vec![0u8; TCP_TX_BUF]);
        let mut tcp = tcp_socket::Socket::new(rx, tx);
        tcp.set_ack_delay(None);
        tcp.set_nagle_enabled(false);
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

#[inline]
fn is_loopback_ip(addr: IpAddress) -> bool {
    match addr {
        IpAddress::Ipv4(addr) => addr.is_loopback(),
        IpAddress::Ipv6(addr) => addr.is_loopback(),
    }
}

#[inline]
fn loopback_addr_for(addr: IpAddress) -> IpAddress {
    match addr {
        IpAddress::Ipv4(_) => IpAddress::Ipv4(Ipv4Address::new(127, 0, 0, 1)),
        IpAddress::Ipv6(_) => IpAddress::Ipv6(Ipv6Address::LOCALHOST),
    }
}

fn read_ipv6_addr(bytes: &[u8]) -> Option<Ipv6Address> {
    if bytes.len() < 16 {
        return None;
    }
    let mut octets = [0u8; 16];
    octets.copy_from_slice(&bytes[..16]);
    Some(Ipv6Address::from(octets))
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
    type RxToken<'a>
        = MultiRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = MultiTxToken<'a>
    where
        Self: 'a;

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
            MultiRxToken::Virtio(token) => token.consume(|frame| {
                #[cfg(feature = "net_perf_counters")]
                perf_inc(&PERF_VIRTIO_RX_FRAMES);
                #[cfg(feature = "net_perf_counters")]
                perf_add(&PERF_VIRTIO_RX_BYTES, frame.len());
                f(frame)
            }),
            MultiRxToken::Loopback(token) => token.consume(|frame| {
                #[cfg(feature = "net_perf_counters")]
                perf_inc(&PERF_LOOPBACK_RX_FRAMES);
                #[cfg(feature = "net_perf_counters")]
                perf_add(&PERF_LOOPBACK_RX_BYTES, frame.len());
                f(frame)
            }),
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
                0x86DD => {
                    if buf.len() >= 54 {
                        let dst = &buf[38..54];
                        dst == Ipv6Address::LOCALHOST.octets()
                            || dst == IPV6_LOOPBACK_SOLICITED_NODE
                    } else {
                        false
                    }
                }
                _ => false,
            }
        } else {
            false
        };

        if is_loopback {
            // Directly push to loopback queue (our custom Loopback has a public queue field)
            debug!(
                "net tx routed to loopback queue: before_len={} frame_len={}",
                self.loopback.queue.len(),
                len
            );
            self.loopback.queue.push_back(buf);
            #[cfg(feature = "net_perf_counters")]
            perf_inc(&PERF_LOOPBACK_TX_FRAMES);
            #[cfg(feature = "net_perf_counters")]
            perf_add(&PERF_LOOPBACK_TX_BYTES, len);
            #[cfg(feature = "net_perf_counters")]
            perf_update_max(&PERF_LOOPBACK_MAX_QUEUE, self.loopback.queue.len());
            debug!(
                "net tx routed to loopback queue: after_len={}",
                self.loopback.queue.len()
            );
        } else {
            // Send to VirtIO using the standard path
            match self.virtio.dev.try_send(&buf) {
                Ok(true) => {
                    #[cfg(feature = "net_perf_counters")]
                    perf_inc(&PERF_VIRTIO_TX_FRAMES);
                    #[cfg(feature = "net_perf_counters")]
                    perf_add(&PERF_VIRTIO_TX_BYTES, len);
                }
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
            Ok(true) => {
                #[cfg(feature = "net_perf_counters")]
                perf_inc(&PERF_VIRTIO_TX_FRAMES);
                #[cfg(feature = "net_perf_counters")]
                perf_add(&PERF_VIRTIO_TX_BYTES, len);
            }
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
