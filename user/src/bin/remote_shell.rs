#![no_std]
#![no_main]

#[macro_use]
extern crate user_lib;

use core::arch::asm;
use user_lib::{
    accept, bind, close, dup, exec, exit, fork, listen, socket, yield_,
    net::{SockAddrIn, AF_INET, SOCK_STREAM},
};

// We want to reap exited child processes (remote sessions) without blocking.
// `user_lib::wait()` blocks in a yield-loop, so we issue the raw syscall once.
const SYSCALL_WAITPID: usize = 260;

#[cfg(target_arch = "riscv64")]
fn syscall(id: usize, args: [usize; 3]) -> isize {
    let mut ret: isize;
    unsafe {
        asm!(
            "ecall",
            in("a7") id,
            in("a0") args[0],
            in("a1") args[1],
            in("a2") args[2],
            lateout("a0") ret
        );
    }
    ret
}

#[cfg(target_arch = "loongarch64")]
fn syscall(id: usize, args: [usize; 3]) -> isize {
    let mut ret: isize;
    unsafe {
        asm!(
            "syscall 0",
            inlateout("$a0") args[0] => ret,
            in("$a1") args[1],
            in("$a2") args[2],
            in("$a7") id,
        );
    }
    ret
}

fn sys_waitpid(pid: isize, exit_code: *mut i32) -> isize {
    syscall(SYSCALL_WAITPID, [pid as usize, exit_code as usize, 0])
}

fn reap_zombies() {
    loop {
        let mut code: i32 = 0;
        let r = sys_waitpid(-1, &mut code as *mut _);
        if r > 0 {
            println!("remote_shell: reaped pid={}, code={}", r, code);
            continue;
        }
        // r == -2: no zombies currently; r == -1: no children.
        break;
    }
}

fn redirect_stdio_to(fd: usize) {
    // Make stdin/stdout/stderr all point to `fd`.
    // dup() always returns the lowest unused fd.
    let _ = close(0);
    let _ = dup(fd);
    let _ = close(1);
    let _ = dup(fd);
    let _ = close(2);
    let _ = dup(fd);
    let _ = close(fd);
}

#[unsafe(no_mangle)]
fn main() -> i32 {
    // Telnet-like remote shell server (plaintext TCP).
    // Connect from host: `nc 127.0.0.1 7777` (requires QEMU hostfwd in Makefile).
    let listen_addr = SockAddrIn::from_ipv4_port([0, 0, 0, 0], 7777);

    let fd = socket(AF_INET, SOCK_STREAM, 0);
    if fd < 0 {
        println!("remote_shell: socket() failed");
        return -1;
    }
    let fd = fd as usize;

    if bind(fd, &listen_addr) < 0 {
        println!("remote_shell: bind() failed");
        let _ = close(fd);
        return -1;
    }

    if listen(fd, 32) < 0 {
        println!("remote_shell: listen() failed");
        let _ = close(fd);
        return -1;
    }

    println!("remote_shell: listening on 0.0.0.0:7777");

    loop {
        // reap_zombies();

        let mut peer = SockAddrIn::default();
        let cfd = accept(fd, Some(&mut peer));
        if cfd < 0 {
            yield_();
            continue;
        }
        let cfd = cfd as usize;

        let pid = fork();
        if pid < 0 {
            println!("remote_shell: fork() failed, dropping cfd={}", cfd);
            let _ = close(cfd);
            continue;
        }

        if pid == 0 {
            // Child: one remote session.
            let _ = close(fd);
            redirect_stdio_to(cfd);

            // Start the normal user shell, but over TCP.
            // NOTE: sys_exec expects a NUL-terminated string in user memory.
            exec("bash\0", &["bash\0".as_ptr(), "-i\0".as_ptr()]);

            // If exec fails, we are still on the network stdio.
            println!("remote_shell: exec('bash -i') failed");
            let _ = exit(-1);
            return -1;
        } else {
            // Parent: keep accepting more connections.
            let _ = close(cfd);
            println!("remote_shell: spawned session pid={}", pid);
        }
    }
}
