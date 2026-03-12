use crate::syscall::errno::{OrErrno, ERRNO};
use crate::syscall_body;
use crate::{
    mm::translated_byte_buffer,
    task::{current_process, current_task, current_user_token},
    timer::{get_time_ticks, get_time_us},
};

use core::mem::size_of;
use core::slice;

#[repr(C)]
#[derive(Debug)]
pub struct TimeVal {
    pub sec: usize,
    pub usec: usize,
}

#[repr(C)]
pub struct Tms {
    pub tms_utime: usize,
    pub tms_stime: usize,
    pub tms_cutime: usize,
    pub tms_cstime: usize,
}

/// get_time syscall
pub fn sys_get_time(_ts: *mut TimeVal, _tz: usize) -> isize {
    trace!(
        "kernel:pid[{}] sys_get_time",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let time_us = get_time_us();
        let timeval = TimeVal {
            sec: time_us / 1_000_000,
            usec: time_us % 1_000_000,
        };
        let timeval_bytes = unsafe {
            slice::from_raw_parts(
                &timeval as *const TimeVal as *const u8,
                size_of::<TimeVal>(),
            )
        };
        let mut buffers =
            translated_byte_buffer(current_user_token(), _ts as *const u8, size_of::<TimeVal>())
                .or_errno(ERRNO::EFAULT)?;
        let mut copied = 0usize;
        for buffer in buffers.iter_mut() {
            let len = buffer.len();
            buffer.copy_from_slice(&timeval_bytes[copied..copied + len]);
            copied += len;
        }
        Ok(0)
    })
}

pub fn sys_times(buf: *mut Tms) -> isize {
    trace!(
        "kernel:pid[{}] sys_times",
        current_task().unwrap().process.upgrade().unwrap().getpid()
    );
    syscall_body!({
        let process = current_process();
        let (tms_utime, tms_stime, tms_cutime, tms_cstime) = process.times_snapshot();
        let tms = Tms {
            tms_utime,
            tms_stime,
            tms_cutime,
            tms_cstime,
        };
        let tms_bytes = unsafe { slice::from_raw_parts(&tms as *const Tms as *const u8, size_of::<Tms>()) };
        let mut buffers =
            translated_byte_buffer(current_user_token(), buf as *const u8, size_of::<Tms>())
                .or_errno(ERRNO::EFAULT)?;
        let mut copied = 0usize;
        for buffer in buffers.iter_mut() {
            let len = buffer.len();
            buffer.copy_from_slice(&tms_bytes[copied..copied + len]);
            copied += len;
        }
        Ok(get_time_ticks() as isize)
    })
}
