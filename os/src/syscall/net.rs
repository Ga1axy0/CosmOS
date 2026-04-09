use alloc::{sync::Arc, vec::Vec};
use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address};

use crate::fs::{AccessMode, File, FileDescription, FileStatusFlags};
use crate::mm::{translated_byte_buffer, translated_ref, translated_refmut, UserBuffer};
use crate::net::{
    create_tcp_socket_file,
    create_udp_socket_file,
    SockAddrIn,
    TcpSocketFile,
    UdpSocketFile,
};
use crate::syscall::errno::{ERRNO, OrErrno};
use crate::syscall_body;
use crate::task::{current_process, current_user_token, FdEntry};

const AF_INET: u16 = 2;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;

fn get_file_description(fd: usize) -> Result<Arc<FileDescription>, ERRNO> {
    let process = current_process();
    let inner = process.inner_exclusive_access();
    if fd >= inner.fd_table.len() {
        return Err(ERRNO::EBADF);
    }
    let desc = inner.fd_table[fd].as_ref().ok_or(ERRNO::EBADF)?.desc.clone();
    Ok(desc)
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

fn sockaddr_to_endpoint(addr: &SockAddrIn) -> Result<IpEndpoint, ERRNO> {
    if addr.sin_family != AF_INET {
        return Err(ERRNO::EAFNOSUPPORT);
    }
    let port = u16::from_be(addr.sin_port);
    let ip_b = addr.sin_addr.to_be_bytes();
    let ip = Ipv4Address::new(ip_b[0], ip_b[1], ip_b[2], ip_b[3]);
    Ok(IpEndpoint::new(IpAddress::Ipv4(ip), port))
}

fn endpoint_to_sockaddr(ep: IpEndpoint) -> SockAddrIn {
    let (sin_addr, sin_port) = match ep.addr {
        IpAddress::Ipv4(v4) => {
            let b = v4.as_bytes();
            (u32::from_be_bytes([b[0], b[1], b[2], b[3]]), ep.port.to_be())
        }
        _ => (0, ep.port.to_be()),
    };
    SockAddrIn {
        sin_family: AF_INET,
        sin_port,
        sin_addr,
        sin_zero: [0; 8],
    }
}

pub fn sys_socket(domain: usize, socket_type: usize, _protocol: usize) -> isize {
    syscall_body!({
        if domain != AF_INET as usize {
            return Err(ERRNO::EAFNOSUPPORT);
        }

        let base_type = socket_type & 0xf;
        let file: Arc<dyn File + Send + Sync> = match base_type {
            SOCK_DGRAM => create_udp_socket_file()
                .map(|f| f as Arc<dyn File + Send + Sync>)
                .ok_or(ERRNO::ENETDOWN)?,
            SOCK_STREAM => create_tcp_socket_file()
                .map(|f| f as Arc<dyn File + Send + Sync>)
                .ok_or(ERRNO::ENETDOWN)?,
            _ => return Err(ERRNO::ESOCKTNOSUPPORT),
        };

        let desc = Arc::new(FileDescription::new(
            file,
            AccessMode::ReadWrite,
            FileStatusFlags::empty(),
            0,
        ));

        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        let fd = inner.alloc_fd();
        inner.fd_table[fd] = Some(FdEntry::new(desc));
        Ok(fd as isize)
    })
}

pub fn sys_bind(fd: u32, addr: *const SockAddrIn, addrlen: usize) -> isize {
    syscall_body!({
        if addr.is_null() || addrlen < core::mem::size_of::<SockAddrIn>() {
            return Err(ERRNO::EINVAL);
        }
        let token = current_user_token();
        let uaddr = translated_ref(token, addr).or_errno(ERRNO::EFAULT)?;
        let ep = sockaddr_to_endpoint(uaddr)?;

        let fd = fd as usize;
        if with_udp_socket(fd, |udp| udp.bind(ep.port)).is_ok() {
            return Ok(0);
        }
        with_tcp_socket(fd, |tcp| tcp.bind(ep.port))?;
        Ok(0)
    })
}

pub fn sys_connect(fd: u32, addr: *const SockAddrIn, addrlen: usize) -> isize {
    syscall_body!({
        if addr.is_null() || addrlen < core::mem::size_of::<SockAddrIn>() {
            return Err(ERRNO::EINVAL);
        }
        let token = current_user_token();
        let uaddr = translated_ref(token, addr).or_errno(ERRNO::EFAULT)?;
        let ep = sockaddr_to_endpoint(uaddr)?;

        let fd = fd as usize;
        if with_udp_socket(fd, |udp| udp.connect(ep)).is_ok() {
            return Ok(0);
        }
        with_tcp_socket(fd, |tcp| tcp.connect(ep))?;
        Ok(0)
    })
}

pub fn sys_listen(fd: u32, backlog: usize) -> isize {
    syscall_body!({
        with_tcp_socket(fd as usize, |tcp| tcp.listen(backlog))?;
        Ok(0)
    })
}

pub fn sys_accept(fd: u32, addr: *mut SockAddrIn, addrlen: usize) -> isize {
    syscall_body!({
        let (accepted, peer) = with_tcp_socket(fd as usize, |tcp| tcp.accept())?;

        let accepted_file: Arc<dyn File + Send + Sync> = accepted;
        let accepted_desc = Arc::new(FileDescription::new(
            accepted_file,
            AccessMode::ReadWrite,
            FileStatusFlags::empty(),
            0,
        ));

        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        let new_fd = inner.alloc_fd();
        inner.fd_table[new_fd] = Some(FdEntry::new(accepted_desc));
        drop(inner);

        if !addr.is_null() && addrlen >= core::mem::size_of::<SockAddrIn>() {
            if let Some(ep) = peer {
                let token = current_user_token();
                let out = translated_refmut(token, addr).or_errno(ERRNO::EFAULT)?;
                *out = endpoint_to_sockaddr(ep);
            }
        }

        Ok(new_fd as isize)
    })
}

pub fn sys_sendto(
    fd: u32,
    buf: *const u8,
    len: usize,
    flags: usize,
    addr: *const SockAddrIn,
    addrlen: usize,
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
        let mut data = Vec::new();
        data.resize(len, 0);
        let ubuf = UserBuffer::new(
            translated_byte_buffer(token, buf, len).or_errno(ERRNO::EFAULT)?,
        );
        let mut off = 0usize;
        for slice in ubuf.buffers.iter() {
            let end = off + slice.len();
            data[off..end].copy_from_slice(slice);
            off = end;
        }

        let fd = fd as usize;
        let n = if addr.is_null() {
            with_udp_socket(fd, |udp| udp.send(data.as_slice()))?
        } else {
            if addrlen < core::mem::size_of::<SockAddrIn>() {
                return Err(ERRNO::EINVAL);
            }
            let uaddr = translated_ref(token, addr).or_errno(ERRNO::EFAULT)?;
            let ep = sockaddr_to_endpoint(uaddr)?;
            with_udp_socket(fd, |udp| udp.send_to(data.as_slice(), ep))?
        };

        Ok(n as isize)
    })
}

pub fn sys_recvfrom(
    fd: u32,
    buf: *mut u8,
    len: usize,
    flags: usize,
    addr: *mut SockAddrIn,
    addrlen: usize,
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
        if !addr.is_null() && addrlen < core::mem::size_of::<SockAddrIn>() {
            return Err(ERRNO::EINVAL);
        }

        let fd = fd as usize;
        let mut tmp = Vec::new();
        tmp.resize(len, 0);
        let (n, ep) = with_udp_socket(fd, |udp| udp.recv_from(tmp.as_mut_slice()))?;

        let token = current_user_token();
        let mut ubuf = UserBuffer::new(
            translated_byte_buffer(token, buf as *const u8, len).or_errno(ERRNO::EFAULT)?,
        );
        let mut off = 0usize;
        for slice in ubuf.buffers.iter_mut() {
            if off >= n {
                break;
            }
            let end = core::cmp::min(off + slice.len(), n);
            slice[..(end - off)].copy_from_slice(&tmp[off..end]);
            off = end;
        }

        if !addr.is_null() {
            let out = translated_refmut(token, addr).or_errno(ERRNO::EFAULT)?;
            *out = endpoint_to_sockaddr(ep);
        }

        Ok(n as isize)
    })
}
