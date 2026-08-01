#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use user_lib::{
    bind, close, connect, getpeername, getsockname, recvfrom, sendto, setsockopt_timeval,
    socket, write, TimeVal,
    net::{SockAddrIn, AF_INET, SOCK_DGRAM, SO_RCVTIMEO, SOL_SOCKET},
};

const DNS_PORT: u16 = 53;
const DEFAULT_SERVER: [u8; 4] = [10, 0, 2, 3];
const DEFAULT_NAME: &str = "www.baidu.com";
const RECV_TIMEOUT_MS: usize = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Auto,
    Bind,
    Connect,
    Query,
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "bind" => Some(Self::Bind),
            "connect" => Some(Self::Connect),
            "query" => Some(Self::Query),
            _ => None,
        }
    }
}

fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut idx = 0usize;
    let mut cur = 0u16;
    let mut has_digit = false;

    for b in s.as_bytes().iter().copied().chain(core::iter::once(b'.')) {
        match b {
            b'0'..=b'9' => {
                has_digit = true;
                cur = cur.checked_mul(10)?.checked_add((b - b'0') as u16)?;
                if cur > 255 {
                    return None;
                }
            }
            b'.' => {
                if !has_digit || idx >= 4 {
                    return None;
                }
                out[idx] = cur as u8;
                idx += 1;
                cur = 0;
                has_digit = false;
            }
            _ => return None,
        }
    }

    if idx == 4 { Some(out) } else { None }
}

fn encode_qname(name: &str, out: &mut [u8], mut off: usize) -> Option<usize> {
    let bytes = name.as_bytes();
    let mut start = 0usize;
    let mut i = 0usize;

    while i <= bytes.len() {
        if i == bytes.len() || bytes[i] == b'.' {
            let len = i.checked_sub(start)?;
            if len == 0 || len > 63 || off + 1 + len > out.len() {
                return None;
            }
            out[off] = len as u8;
            off += 1;
            out[off..off + len].copy_from_slice(&bytes[start..i]);
            off += len;
            start = i + 1;
        }
        i += 1;
    }

    if off >= out.len() {
        return None;
    }
    out[off] = 0;
    Some(off + 1)
}

fn build_dns_query(name: &str, out: &mut [u8; 512]) -> Option<usize> {
    out.fill(0);
    out[0] = 0x12;
    out[1] = 0x34;
    out[2] = 0x01;
    out[3] = 0x00;
    out[4] = 0x00;
    out[5] = 0x01;

    let mut off = encode_qname(name, out, 12)?;
    if off + 4 > out.len() {
        return None;
    }
    out[off] = 0x00;
    out[off + 1] = 0x01; // QTYPE=A
    out[off + 2] = 0x00;
    out[off + 3] = 0x01; // QCLASS=IN
    off += 4;
    Some(off)
}

fn print_sockaddr(label: &str, addr: &SockAddrIn) {
    let ip = addr.ipv4();
    println!(
        "{} {}.{}.{}.{}:{}",
        label,
        ip[0],
        ip[1],
        ip[2],
        ip[3],
        addr.port()
    );
}

fn dump_local(fd: usize) {
    let mut local = SockAddrIn::default();
    let ret = getsockname(fd, Some(&mut local));
    if ret < 0 {
        println!("getsockname failed: {}", ret);
    } else {
        print_sockaddr("local", &local);
    }
}

fn dump_peer(fd: usize) {
    let mut peer = SockAddrIn::default();
    let ret = getpeername(fd, Some(&mut peer));
    if ret < 0 {
        println!("getpeername failed: {}", ret);
    } else {
        print_sockaddr("peer", &peer);
    }
}

fn usage() {
    println!("usage: dns_probe [auto|bind|connect|query] [server_ip] [domain]");
    println!("default: dns_probe auto 10.0.2.3 www.baidu.com");
}

fn set_recv_timeout(fd: usize, timeout_ms: usize) {
    let tv = TimeVal {
        sec: timeout_ms / 1000,
        usec: (timeout_ms % 1000) * 1000,
    };
    let ret = setsockopt_timeval(fd, SOL_SOCKET, SO_RCVTIMEO, &tv);
    println!("setsockopt(SO_RCVTIMEO={}ms) -> {}", timeout_ms, ret);
}

#[no_mangle]
pub fn main(argc: usize, argv: &[&str]) -> i32 {
    let mode = if argc >= 2 {
        match Mode::parse(argv[1]) {
            Some(mode) => mode,
            None => {
                usage();
                return 1;
            }
        }
    } else {
        Mode::Auto
    };

    let server_ip = if argc >= 3 {
        match parse_ipv4(argv[2]) {
            Some(ip) => ip,
            None => {
                println!("invalid server ip: {}", argv[2]);
                return 1;
            }
        }
    } else {
        DEFAULT_SERVER
    };

    let domain = if argc >= 4 { argv[3] } else { DEFAULT_NAME };
    let server = SockAddrIn::from_ipv4_port(server_ip, DNS_PORT);
    let wildcard = SockAddrIn::from_ipv4_port([0, 0, 0, 0], 0);

    let fd = socket(AF_INET, SOCK_DGRAM, 0);
    if fd < 0 {
        println!("socket failed: {}", fd);
        return 1;
    }
    let fd = fd as usize;

    println!("mode: {:?}, domain: {}", mode, domain);
    print_sockaddr("server", &server);
    dump_local(fd);

    if matches!(mode, Mode::Bind | Mode::Query) {
        let ret = bind(fd, &wildcard);
        println!("bind(0.0.0.0:0) -> {}", ret);
        if ret < 0 {
            let _ = close(fd);
            return 2;
        }
        dump_local(fd);
    }

    let mut packet = [0u8; 512];
    let query_len = match build_dns_query(domain, &mut packet) {
        Some(len) => len,
        None => {
            println!("build_dns_query failed");
            let _ = close(fd);
            return 3;
        }
    };
    println!("query bytes: {}", query_len);

    let send_ret = match mode {
        Mode::Auto | Mode::Bind | Mode::Query => sendto(fd, &packet[..query_len], 0, Some(&server)),
        Mode::Connect => {
            let cret = connect(fd, &server);
            println!("connect -> {}", cret);
            if cret < 0 {
                let _ = close(fd);
                return 4;
            }
            dump_local(fd);
            dump_peer(fd);
            write(fd, &packet[..query_len])
        }
    };

    println!("send -> {}", send_ret);
    dump_local(fd);
    if send_ret < 0 {
        let _ = close(fd);
        return 5;
    }

    if mode == Mode::Query {
        set_recv_timeout(fd, RECV_TIMEOUT_MS);
        let mut from = SockAddrIn::default();
        let mut buf = [0u8; 512];
        let rn = recvfrom(fd, &mut buf, 0, Some(&mut from));
        println!("recvfrom -> {}", rn);
        if rn > 0 {
            print_sockaddr("from", &from);
            let rn = rn as usize;
            let shown = rn.min(32);
            print!("resp:");
            for b in &buf[..shown] {
                print!(" {:02x}", *b);
            }
            println!("");
        }
    } else {
        println!("send-only mode; use packet capture on host to confirm UDP egress");
    }

    let _ = close(fd);
    0
}
