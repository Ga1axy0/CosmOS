#![no_std]
#![no_main]

use user_lib::{exit, fork, shmctl, shmat, shmdt, shmget, waitpid, IPC_CREAT, IPC_RMID};

const KEY: i32 = 0x1234;
const SIZE: usize = 4096;

#[no_mangle]
pub fn main(_argc: usize, _argv: &[&str]) -> i32 {
    let shmid = shmget(KEY, SIZE, IPC_CREAT);
    assert!(shmid >= 0, "shmget failed: {}", shmid);
    let shmid = shmid as usize;

    let addr = shmat(shmid, 0, 0);
    assert!(addr >= 0, "shmat parent failed: {}", addr);
    let ptr = addr as usize as *mut u32;
    unsafe {
        ptr.write_volatile(7);
    }

    let pid = fork();
    assert!(pid >= 0, "fork failed: {}", pid);
    if pid == 0 {
        let child_addr = shmat(shmid, 0, 0);
        assert!(child_addr >= 0, "shmat child failed: {}", child_addr);
        let child_ptr = child_addr as usize as *mut u32;
        let seen = unsafe { child_ptr.read_volatile() };
        assert_eq!(seen, 7, "child saw {}", seen);
        unsafe {
            child_ptr.write_volatile(42);
        }
        assert_eq!(shmdt(child_addr as usize), 0, "child shmdt failed");
        exit(0);
    }

    let mut status = 0;
    assert!(waitpid(pid as usize, &mut status) >= 0, "waitpid failed");
    let parent_seen = unsafe { ptr.read_volatile() };
    assert_eq!(parent_seen, 42, "parent saw {}", parent_seen);
    assert_eq!(shmdt(addr as usize), 0, "parent shmdt failed");
    assert_eq!(shmctl(shmid, IPC_RMID, 0), 0, "IPC_RMID failed");
    0
}
