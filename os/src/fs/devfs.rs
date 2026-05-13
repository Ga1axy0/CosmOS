//! Block-device VFS nodes for `/dev`.
//!
//! [`BlockDevNode`] wraps an `Arc<dyn BlockDevice>` and exposes it as a VFS
//! node so that `sys_mount` can resolve `/dev/vda` (or `/dev/vdb`, etc.) into
//! the underlying block-device driver without a separate devfs daemon.
//!
//! The nodes are purely in-memory and are registered under the virtual `/dev`
//! directory by [`super::inode::init_dev`] at boot time.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use fs::vfs::{VfsFileType, VfsNode};
use fs::BlockDevice;

use crate::drivers::rtc;
use crate::fs::{Stat, StatMode};
use crate::mm::translated_ref;
use crate::syscall::errno::ERRNO;
use crate::syscall::{write_pod_to_user, Pod};
use crate::task::current_user_token;

use crate::random as kernel_random;

const RTC_RD_TIME: usize = 0xFFFF_FFFF_8024_7009;
const RTC_SET_TIME: usize = 0x4024_700A;

/// Linux `struct rtc_time` ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LinuxRtcTime {
    /// seconds (0-59)
    pub tm_sec: i32,
    /// minutes (0-59)
    pub tm_min: i32,
    /// hours (0-23)
    pub tm_hour: i32,
    /// day of month (1-31)
    pub tm_mday: i32,
    /// month since January (0-11)
    pub tm_mon: i32,
    /// years since 1900
    pub tm_year: i32,
    /// days since Sunday (0-6)
    pub tm_wday: i32,
    /// days since January 1 (0-365)
    pub tm_yday: i32,
    /// daylight saving time flag
    pub tm_isdst: i32,
}

// 允许 RTC ioctl 将该 C ABI 结构整体写回用户空间。
impl Pod for LinuxRtcTime {}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

fn days_in_month(year: i32, month0: i32) -> Option<i32> {
    let d = match month0 {
        0 => 31,
        1 => if is_leap_year(year) { 29 } else { 28 },
        2 => 31,
        3 => 30,
        4 => 31,
        5 => 30,
        6 => 31,
        7 => 31,
        8 => 30,
        9 => 31,
        10 => 30,
        11 => 31,
        _ => return None,
    };
    Some(d)
}

/// Convert civil date to days since Unix epoch (1970-01-01).
fn days_from_civil(year: i32, month1: i32, day: i32) -> i64 {
    let mut y = year as i64;
    let m = month1 as i64;
    let d = day as i64;
    y -= if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Convert days since Unix epoch to (year, month1, day).
fn civil_from_days(days_since_epoch: i64) -> (i32, i32, i32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    y += if m <= 2 { 1 } else { 0 };
    (y as i32, m as i32, d as i32)
}

fn yday_from_ymd(year: i32, month0: i32, mday: i32) -> Option<i32> {
    const CUM_DAYS: [i32; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    if !(0..=11).contains(&month0) {
        return None;
    }
    let mut yday = CUM_DAYS[month0 as usize] + (mday - 1);
    if is_leap_year(year) && month0 >= 2 {
        yday += 1;
    }
    Some(yday)
}

fn rtc_time_from_unix_secs(unix_secs: i64) -> LinuxRtcTime {
    let days = unix_secs.div_euclid(86_400);
    let sec_of_day = unix_secs.rem_euclid(86_400);
    let hour = (sec_of_day / 3_600) as i32;
    let min = ((sec_of_day % 3_600) / 60) as i32;
    let sec = (sec_of_day % 60) as i32;

    let (year, month1, mday) = civil_from_days(days);
    let month0 = month1 - 1;
    let wday = ((days + 4).rem_euclid(7)) as i32;
    let yday = yday_from_ymd(year, month0, mday).unwrap_or(0);

    LinuxRtcTime {
        tm_sec: sec,
        tm_min: min,
        tm_hour: hour,
        tm_mday: mday,
        tm_mon: month0,
        tm_year: year - 1900,
        tm_wday: wday,
        tm_yday: yday,
        tm_isdst: 0,
    }
}

fn unix_secs_from_rtc_time(tm: LinuxRtcTime) -> Option<i64> {
    if !(0..=59).contains(&tm.tm_sec)
        || !(0..=59).contains(&tm.tm_min)
        || !(0..=23).contains(&tm.tm_hour)
        || !(0..=11).contains(&tm.tm_mon)
    {
        return None;
    }
    let year = tm.tm_year.checked_add(1900)?;
    let max_day = days_in_month(year, tm.tm_mon)?;
    if tm.tm_mday < 1 || tm.tm_mday > max_day {
        return None;
    }

    let month1 = tm.tm_mon + 1;
    let days = days_from_civil(year, month1, tm.tm_mday);
    let sec_of_day = (tm.tm_hour as i64)
        .checked_mul(3_600)?
        .checked_add((tm.tm_min as i64).checked_mul(60)?)?
        .checked_add(tm.tm_sec as i64)?;
    days.checked_mul(86_400)?.checked_add(sec_of_day)
}

/// VFS node representing a raw block device (e.g. `/dev/vda`).
///
/// Supports `read_at` / `write_at` for direct sector-aligned block I/O.
/// All directory operations (`ls`, `find`, `mkdir`, …) return empty / `None`.
pub struct BlockDevNode {
    /// The underlying block device driver.
    pub device: Arc<dyn BlockDevice>,
}

impl BlockDevNode {
    /// Wrap `device` in a new node.
    pub fn new(device: Arc<dyn BlockDevice>) -> Self {
        Self { device }
    }
}

/// VFS node representing the special `/dev/null` device.
///
/// Reads always return EOF (0 bytes). Writes discard data and report the
/// full write length as written.
pub struct NullDevNode;

impl NullDevNode {
    /// Create a new `/dev/null` node.
    pub fn new() -> Self {
        Self {}
    }
}

// SAFETY: stateless and immutable.
unsafe impl Send for NullDevNode {}
unsafe impl Sync for NullDevNode {}

impl VfsNode for NullDevNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Char
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        Vec::new()
    }

    fn find(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn create(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn mkdir(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn clear(&self) {}

    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        // /dev/null reads EOF
        0
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> usize {
        // Discard and report full length written
        buf.len()
    }

    fn truncate(&self, _new_size: usize) -> Result<(), fs::errno::FS_ERRNO> {
        Ok(())   
    }

}

// SAFETY: single-processor kernel; `BlockDevice` is already `Send + Sync`.
unsafe impl Send for BlockDevNode {}
unsafe impl Sync for BlockDevNode {}

impl VfsNode for BlockDevNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Block
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        Vec::new()
    }

    fn find(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn create(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn mkdir(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn clear(&self) {}

    /// Read `buf.len()` bytes from the device starting at byte `offset`.
    ///
    /// Uses a 512-byte stack buffer for any partial-block reads.
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        const BLOCK_SIZE: usize = 512;
        let mut total = 0usize;
        let mut pos = offset;
        let mut tmp = [0u8; BLOCK_SIZE];
        while total < buf.len() {
            let blk = pos / BLOCK_SIZE;
            let blk_off = pos % BLOCK_SIZE;
            self.device.read_block(blk, &mut tmp);
            let copy = (BLOCK_SIZE - blk_off).min(buf.len() - total);
            buf[total..total + copy].copy_from_slice(&tmp[blk_off..blk_off + copy]);
            total += copy;
            pos += copy;
        }
        total
    }

    /// Write `buf` to the device starting at byte `offset`.
    ///
    /// Performs a read-modify-write for any partial leading/trailing blocks.
    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        const BLOCK_SIZE: usize = 512;
        let mut total = 0usize;
        let mut pos = offset;
        while total < buf.len() {
            let blk = pos / BLOCK_SIZE;
            let blk_off = pos % BLOCK_SIZE;
            let mut tmp = [0u8; BLOCK_SIZE];
            // Read the existing block content for partial writes.
            self.device.read_block(blk, &mut tmp);
            let copy = (BLOCK_SIZE - blk_off).min(buf.len() - total);
            tmp[blk_off..blk_off + copy].copy_from_slice(&buf[total..total + copy]);
            self.device.write_block(blk, &tmp);
            total += copy;
            pos += copy;
        }
        total
    }
}

/// VFS node representing RTC char device (`/dev/rtc*`, `/dev/misc/rtc`).
pub struct RtcDevNode;

impl Default for RtcDevNode {
    fn default() -> Self {
        Self::new()
    }
}

impl RtcDevNode {
    /// Create a new RTC device node.
    pub fn new() -> Self {
        Self
    }

    /// Handle Linux RTC ioctls.
    pub fn ioctl(&self, req: usize, arg: usize) -> Result<isize, ERRNO> {
        match req {
            RTC_RD_TIME => {
                if !rtc::rtc_ready() {
                    return Err(ERRNO::ENODEV);
                }
                let now_ns = rtc::read_time_ns();
                let now_secs = (now_ns / 1_000_000_000) as i64;
                write_pod_to_user(arg as *mut LinuxRtcTime, &rtc_time_from_unix_secs(now_secs))?;
                Ok(0)
            }
            RTC_SET_TIME => {
                let token = current_user_token();
                let tm = *translated_ref(token, arg as *const LinuxRtcTime).ok_or(ERRNO::EFAULT)?;
                let unix_secs = unix_secs_from_rtc_time(tm).ok_or(ERRNO::EINVAL)?;
                if unix_secs < 0 {
                    return Err(ERRNO::EINVAL);
                }
                let unix_ns = (unix_secs as u64)
                    .checked_mul(1_000_000_000)
                    .ok_or(ERRNO::EINVAL)?;
                rtc::write_time_ns(unix_ns);
                Ok(0)
            }
            _ => {
                debug!("RTC ioctl: unknown req {:#x}", req);
                Err(ERRNO::ENOTTY)},
        }
    }

    /// `stat(2)` metadata for rtc char device.
    pub fn stat(&self) -> Stat {
        Stat {
            dev: 0,
            ino: 0,
            mode: StatMode::CHAR,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            pad0: 0,
            size: 0,
            blksize: 0,
            pad1: 0,
            blocks: 0,
            atime_sec: 0,
            atime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            ctime_sec: 0,
            ctime_nsec: 0,
            unused: [0; 2],
        }
    }
}

impl VfsNode for RtcDevNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Char
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        Vec::new()
    }

    fn find(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn create(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn mkdir(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn clear(&self) {}

    fn read_at(&self, _offset: usize, _buf: &mut [u8]) -> usize {
        0
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }
}

/// VFS node representing `/dev/urandom` (CSPRNG character device).
///
/// Reads block until the kernel RNG is seeded (initial seed), then fills the
/// provided buffer with cryptographic random bytes via `random::fill_bytes`.
pub struct UrandomDevNode;

impl UrandomDevNode {
    /// Create a new `/dev/urandom` node.
    pub fn new() -> Self {
        Self
    }
}

impl Default for UrandomDevNode {
    fn default() -> Self {
        Self::new()
    }
}

impl VfsNode for UrandomDevNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Char
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        Vec::new()
    }

    fn find(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn create(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn mkdir(&self, _name: &str) -> Option<Arc<dyn VfsNode>> {
        None
    }

    fn clear(&self) {}

    fn read_at(&self, _offset: usize, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        // Block until RNG seeded (initial seed). Ignore error path — callers
        // that require non-blocking semantics should use getrandom syscall.
        debug!("UrandomDevNode::read_at");
        let _ = kernel_random::wait_for_seed(true);
        kernel_random::fill_bytes(buf);
        debug!("UrandomDevNode::read_at: filled {} bytes", buf.len());
        buf.len()
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        // Writing to /dev/urandom is a no-op in this minimal implementation.
        0
    }

    fn mode(&self) -> Option<u32> {
        Some(StatMode::CHAR.bits())
    }
}
