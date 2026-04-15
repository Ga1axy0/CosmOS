#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::mem::size_of;

use user_lib::{
    close, recvmsg, sendmsg, shutdown, socketpair, write,
    net::{
        AF_UNIX, CmsgHdr, IoVec, MsgHdr, Ucred, SCM_CREDENTIALS, SCM_RIGHTS, SHUT_WR,
        SOCK_STREAM, SOL_SOCKET,
    },
};

#[inline]
fn cmsg_align(len: usize) -> usize {
    let a = size_of::<usize>();
    (len + a - 1) & !(a - 1)
}

fn push_cmsg(buf: &mut [u8], off: usize, level: i32, ty: i32, payload: &[u8]) -> Option<usize> {
    let hdr_len = size_of::<CmsgHdr>();
    let cmsg_len = hdr_len + payload.len();
    let cmsg_space = cmsg_align(cmsg_len);
    if off.checked_add(cmsg_space)? > buf.len() {
        return None;
    }

    let hdr = CmsgHdr {
        cmsg_len,
        cmsg_level: level,
        cmsg_type: ty,
    };

    let hdr_bytes = unsafe {
        core::slice::from_raw_parts((&hdr as *const CmsgHdr) as *const u8, hdr_len)
    };
    buf[off..off + hdr_len].copy_from_slice(hdr_bytes);
    buf[off + hdr_len..off + hdr_len + payload.len()].copy_from_slice(payload);
    for b in &mut buf[off + hdr_len + payload.len()..off + cmsg_space] {
        *b = 0;
    }

    Some(off + cmsg_space)
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    println!("socketpair_msg_test: starting");

    let mut sv = [-1i32; 2];
    if socketpair(AF_UNIX, SOCK_STREAM, 0, &mut sv) < 0 {
        println!("socketpair_msg_test: socketpair failed");
        return -1;
    }
    let fd0 = sv[0] as usize;
    let fd1 = sv[1] as usize;

    let mut control = [0u8; 128];
    let mut off = 0usize;

    // 发送一个 stdout fd。
    let rights_payload = (1i32).to_ne_bytes();
    off = match push_cmsg(&mut control, off, SOL_SOCKET, SCM_RIGHTS, &rights_payload) {
        Some(v) => v,
        None => {
            println!("socketpair_msg_test: build SCM_RIGHTS failed");
            let _ = close(fd0);
            let _ = close(fd1);
            return -1;
        }
    };

    // 发送凭证（内核会以当前任务真实凭证回填/覆盖）。
    let dummy_cred = Ucred { pid: 0, uid: 0, gid: 0 };
    let cred_payload = unsafe {
        core::slice::from_raw_parts((&dummy_cred as *const Ucred) as *const u8, size_of::<Ucred>())
    };
    off = match push_cmsg(&mut control, off, SOL_SOCKET, SCM_CREDENTIALS, cred_payload) {
        Some(v) => v,
        None => {
            println!("socketpair_msg_test: build SCM_CREDENTIALS failed");
            let _ = close(fd0);
            let _ = close(fd1);
            return -1;
        }
    };

    let data = [b'X'];
    let iov = [IoVec::from_slice(&data)];
    let msg = MsgHdr {
        msg_name: 0,
        msg_namelen: 0,
        msg_iov: iov.as_ptr() as usize,
        msg_iovlen: iov.len(),
        msg_control: control.as_ptr() as usize,
        msg_controllen: off,
        msg_flags: 0,
    };

    let wn = sendmsg(fd0, &msg, 0);
    if wn != 1 {
        println!("socketpair_msg_test: sendmsg failed: {}", wn);
        let _ = close(fd0);
        let _ = close(fd1);
        return -1;
    }

    let mut rx = [0u8; 8];
    let mut riov = [IoVec::from_mut_slice(&mut rx)];
    let mut rctrl = [0u8; 256];
    let mut rmsg = MsgHdr {
        msg_name: 0,
        msg_namelen: 0,
        msg_iov: riov.as_mut_ptr() as usize,
        msg_iovlen: riov.len(),
        msg_control: rctrl.as_mut_ptr() as usize,
        msg_controllen: rctrl.len(),
        msg_flags: 0,
    };

    let rn = recvmsg(fd1, &mut rmsg, 0);
    if rn <= 0 {
        println!("socketpair_msg_test: recvmsg failed: {}", rn);
        let _ = close(fd0);
        let _ = close(fd1);
        return -1;
    }

    let mut got_rights_fd = -1i32;
    let mut got_cred = false;
    let mut cur = 0usize;
    while cur + size_of::<CmsgHdr>() <= rmsg.msg_controllen {
        let hdr = unsafe { core::ptr::read_unaligned(rctrl[cur..].as_ptr() as *const CmsgHdr) };
        if hdr.cmsg_len < size_of::<CmsgHdr>() || cur + hdr.cmsg_len > rmsg.msg_controllen {
            break;
        }
        let payload = &rctrl[cur + size_of::<CmsgHdr>()..cur + hdr.cmsg_len];
        if hdr.cmsg_level == SOL_SOCKET && hdr.cmsg_type == SCM_RIGHTS && payload.len() >= 4 {
            got_rights_fd = i32::from_ne_bytes([payload[0], payload[1], payload[2], payload[3]]);
        }
        if hdr.cmsg_level == SOL_SOCKET && hdr.cmsg_type == SCM_CREDENTIALS && payload.len() >= size_of::<Ucred>() {
            got_cred = true;
        }
        cur += cmsg_align(hdr.cmsg_len);
    }

    if got_rights_fd < 0 || !got_cred {
        println!(
            "socketpair_msg_test: ancillary missing rights_fd={} cred={}",
            got_rights_fd,
            got_cred
        );
        let _ = close(fd0);
        let _ = close(fd1);
        return -1;
    }

    let _ = write(got_rights_fd as usize, b"socketpair_msg_test: SCM_RIGHTS ok\n");
    let _ = close(got_rights_fd as usize);

    if shutdown(fd0, SHUT_WR) < 0 {
        println!("socketpair_msg_test: shutdown failed");
        let _ = close(fd0);
        let _ = close(fd1);
        return -1;
    }

    let wn_after = write(fd0, b"Y");
    if wn_after > 0 {
        println!("socketpair_msg_test: write after SHUT_WR unexpectedly wrote {}", wn_after);
        let _ = close(fd0);
        let _ = close(fd1);
        return -1;
    }

    let _ = close(fd0);
    let _ = close(fd1);
    println!("socketpair_msg_test: success");
    0
}
