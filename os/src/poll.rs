//! Poll wait registry and keyed wakeup helpers.
//!
//! This module provides a fixed-size bitmap-based registry for `ppoll` waits:
//! - kernel-fd rows (max 128)
//! - poll-key columns (max 128)
//! - per-row interest bitmaps for POLLIN/POLLOUT
//!
//! It is intentionally crate-private and shared by syscall/timer/device paths.

use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{TaskControlBlock, WaitQueueKeyed, WaitReason};
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::lazy_static;

/// Readable event bit.
pub(crate) const POLLIN: u16 = 0x001;
/// Writable event bit.
pub(crate) const POLLOUT: u16 = 0x004;
/// Error event bit.
pub(crate) const POLLERR: u16 = 0x008;
/// Hangup event bit.
pub(crate) const POLLHUP: u16 = 0x010;

const MAX_KERNEL_FD: usize = 128;
const MAX_POLL_KEYS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PollKeyState {
    Free,
    Active,
    Ready,
    TimedOut,
}

#[derive(Clone, Copy, Debug)]
struct KernelFdSlot {
    active: bool,
    generation: u16,
    owner_pid: usize,
    owner_fd: usize,
    source_id: usize,
    key_bits: u128,
    key_bits_in: u128,
    key_bits_out: u128,
}

impl Default for KernelFdSlot {
    fn default() -> Self {
        Self {
            active: false,
            generation: 0,
            owner_pid: 0,
            owner_fd: 0,
            source_id: 0,
            key_bits: 0,
            key_bits_in: 0,
            key_bits_out: 0,
        }
    }
}

impl KernelFdSlot {
    const EMPTY: Self = Self {
        active: false,
        generation: 0,
        owner_pid: 0,
        owner_fd: 0,
        source_id: 0,
        key_bits: 0,
        key_bits_in: 0,
        key_bits_out: 0,
    };
}

#[derive(Clone, Copy, Debug)]
struct PollKeySlot {
    generation: u8,
    state: PollKeyState,
    task_ptr: usize,
    owner_pid: usize,
    rows_mask: u128,
}

impl Default for PollKeySlot {
    fn default() -> Self {
        Self {
            generation: 0,
            state: PollKeyState::Free,
            task_ptr: 0,
            owner_pid: 0,
            rows_mask: 0,
        }
    }
}

impl PollKeySlot {
    const EMPTY: Self = Self {
        generation: 0,
        state: PollKeyState::Free,
        task_ptr: 0,
        owner_pid: 0,
        rows_mask: 0,
    };
}

#[derive(Debug)]
struct PollRegistry {
    kernel_slots: [KernelFdSlot; MAX_KERNEL_FD],
    key_slots: [PollKeySlot; MAX_POLL_KEYS],
    next_kernel_fd: usize,
    next_key: usize,
}

impl PollRegistry {
    const fn new() -> Self {
        Self {
            kernel_slots: [KernelFdSlot::EMPTY; MAX_KERNEL_FD],
            key_slots: [PollKeySlot::EMPTY; MAX_POLL_KEYS],
            next_kernel_fd: 0,
            next_key: 0,
        }
    }

    fn alloc_key(&mut self, task_ptr: usize, owner_pid: usize) -> Result<PollWaitHandle, ERRNO> {
        for off in 0..MAX_POLL_KEYS {
            let idx = (self.next_key + off) % MAX_POLL_KEYS;
            if !matches!(self.key_slots[idx].state, PollKeyState::Free) {
                continue;
            }
            let slot = &mut self.key_slots[idx];
            slot.generation = slot.generation.wrapping_add(1);
            slot.state = PollKeyState::Active;
            slot.task_ptr = task_ptr;
            slot.owner_pid = owner_pid;
            slot.rows_mask = 0;
            self.next_key = (idx + 1) % MAX_POLL_KEYS;
            return Ok(PollWaitHandle {
                key_idx: idx as u8,
                key_generation: slot.generation,
            });
        }
        Err(ERRNO::ENOSPC)
    }

    fn find_or_alloc_kernel_fd(
        &mut self,
        pid: usize,
        fd: usize,
        source_id: usize,
    ) -> Result<usize, ERRNO> {
        for (idx, slot) in self.kernel_slots.iter().enumerate() {
            if slot.active
                && slot.owner_pid == pid
                && slot.owner_fd == fd
                && slot.source_id == source_id
            {
                return Ok(idx);
            }
        }

        for off in 0..MAX_KERNEL_FD {
            let idx = (self.next_kernel_fd + off) % MAX_KERNEL_FD;
            if self.kernel_slots[idx].active {
                continue;
            }
            let slot = &mut self.kernel_slots[idx];
            slot.active = true;
            slot.generation = slot.generation.wrapping_add(1);
            slot.owner_pid = pid;
            slot.owner_fd = fd;
            slot.source_id = source_id;
            slot.key_bits = 0;
            slot.key_bits_in = 0;
            slot.key_bits_out = 0;
            self.next_kernel_fd = (idx + 1) % MAX_KERNEL_FD;
            return Ok(idx);
        }

        Err(ERRNO::ENOSPC)
    }

    fn key_valid(&self, handle: PollWaitHandle) -> bool {
        let idx = handle.key_idx as usize;
        let slot = &self.key_slots[idx];
        !matches!(slot.state, PollKeyState::Free) && slot.generation == handle.key_generation
    }

    fn clear_key_rows(&mut self, handle: PollWaitHandle) {
        let key_idx = handle.key_idx as usize;
        let key_bit = key_bit(key_idx);
        let rows_mask = self.key_slots[key_idx].rows_mask;
        for row in 0..MAX_KERNEL_FD {
            let row_mask = row_bit(row);
            if (rows_mask & row_mask) == 0 {
                continue;
            }
            let slot = &mut self.kernel_slots[row];
            slot.key_bits &= !key_bit;
            slot.key_bits_in &= !key_bit;
            slot.key_bits_out &= !key_bit;
            if slot.key_bits == 0 {
                slot.active = false;
            }
        }
        self.key_slots[key_idx].rows_mask = 0;
    }

    fn cleanup_key(&mut self, handle: PollWaitHandle) {
        if !self.key_valid(handle) {
            return;
        }
        self.clear_key_rows(handle);
        let key_idx = handle.key_idx as usize;
        let slot = &mut self.key_slots[key_idx];
        slot.state = PollKeyState::Free;
        slot.task_ptr = 0;
        slot.owner_pid = 0;
    }
}

lazy_static! {
    static ref POLL_WAIT_QUEUE: WaitQueueKeyed<u16> = WaitQueueKeyed::new();
}

// Use const static initialization for registry to avoid a large lazy-init stack
// frame in `spin::once` (can overflow small kernel stacks on early IRQ paths).
static POLL_REGISTRY: SpinNoIrqLock<PollRegistry> = SpinNoIrqLock::new(PollRegistry::new());

#[inline]
fn row_bit(row: usize) -> u128 {
    1u128 << row
}

#[inline]
fn key_bit(key_idx: usize) -> u128 {
    1u128 << key_idx
}

#[inline]
fn encode_wait_key(key_idx: u8, key_generation: u8) -> u16 {
    ((key_generation as u16) << 8) | (key_idx as u16)
}

/// Opaque handle for one in-flight `ppoll` wait.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PollWaitHandle {
    key_idx: u8,
    key_generation: u8,
}

impl PollWaitHandle {
    /// Encoded wait-queue key (`generation << 8 | index`).
    pub(crate) fn wait_key(self) -> u16 {
        encode_wait_key(self.key_idx, self.key_generation)
    }

    /// Timeout tag consumed by timer path.
    pub(crate) fn timer_tag(self) -> PollTimerTag {
        PollTimerTag {
            key_idx: self.key_idx,
            key_generation: self.key_generation,
        }
    }
}

/// Timeout identity attached to timer heap entries created by `ppoll`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PollTimerTag {
    key_idx: u8,
    key_generation: u8,
}

/// Observable state of a poll wait key after wakeup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PollWakeState {
    /// Triggered by fd readiness notification.
    Ready,
    /// Triggered by timeout.
    TimedOut,
    /// Key no longer valid (already cleaned or generation mismatch).
    Canceled,
}

/// Register one poll wait key and attach interested `(fd, source_id, events)` rows.
///
/// `pid` + `fd` disambiguates per-process fd namespace;
/// `source_id` identifies the underlying readiness source.
pub(crate) fn register_poll_wait(
    pid: usize,
    task: &Arc<TaskControlBlock>,
    interests: &[(usize, usize, u16)],
) -> Result<PollWaitHandle, ERRNO> {
    let task_ptr = Arc::as_ptr(task) as usize;
    let mut registry = POLL_REGISTRY.lock();
    let handle = registry.alloc_key(task_ptr, pid)?;
    let key_idx = handle.key_idx as usize;
    let key_bit = key_bit(key_idx);

    // debug!("register_poll_wait: pid={}, handle={:?}, interests={:?}", pid, handle, interests);

    for &(fd, source_id, events) in interests {
        let row = match registry.find_or_alloc_kernel_fd(pid, fd, source_id) {
            Ok(row) => row,
            Err(e) => {
                registry.cleanup_key(handle);
                return Err(e);
            }
        };
        let row_slot = &mut registry.kernel_slots[row];
        row_slot.key_bits |= key_bit;
        if (events & POLLIN) != 0 {
            row_slot.key_bits_in |= key_bit;
        }
        if (events & POLLOUT) != 0 {
            row_slot.key_bits_out |= key_bit;
        }
        registry.key_slots[key_idx].rows_mask |= row_bit(row);
    }

    Ok(handle)
}

/// Remove all bitmap registrations bound to this wait key and free the key slot.
pub(crate) fn cleanup_poll_wait(handle: PollWaitHandle) {
    POLL_REGISTRY.lock().cleanup_key(handle);
}

/// Check whether wait should be skipped because key has already been triggered.
pub(crate) fn poll_wait_should_skip(handle: PollWaitHandle) -> bool {
    let registry = POLL_REGISTRY.lock();
    if !registry.key_valid(handle) {
        return true;
    }
    let state = registry.key_slots[handle.key_idx as usize].state;
    !matches!(state, PollKeyState::Active)
}

/// Query current wake state for this wait key.
pub(crate) fn poll_wait_state(handle: PollWaitHandle) -> PollWakeState {
    let registry = POLL_REGISTRY.lock();
    if !registry.key_valid(handle) {
        return PollWakeState::Canceled;
    }
    match registry.key_slots[handle.key_idx as usize].state {
        PollKeyState::TimedOut => PollWakeState::TimedOut,
        PollKeyState::Ready => PollWakeState::Ready,
        PollKeyState::Active => PollWakeState::Canceled,
        PollKeyState::Free => PollWakeState::Canceled,
    }
}

/// Block on global poll wait queue with race-safe skip recheck.
pub(crate) fn wait_poll_key(handle: PollWaitHandle) {
    let wait_key = handle.wait_key();
    POLL_WAIT_QUEUE.wait_selected_with_reason_or_skip(wait_key, WaitReason::Poll, || {
        poll_wait_should_skip(handle)
    });
}

/// Notify readiness for a source id and wake interested wait keys.
pub(crate) fn notify_poll_source(source_id: usize, ready_mask: u16) {
    // debug!("notify_poll_source: source_id={}, ready_mask={:#x}", source_id, ready_mask);
    let mut wait_keys = Vec::new();
    {
        let mut registry = POLL_REGISTRY.lock();
        let mut wake_bits = 0u128;

        for row in 0..MAX_KERNEL_FD {
            let row_slot = &registry.kernel_slots[row];
            if !row_slot.active || row_slot.source_id != source_id {
                continue;
            }
            let mut row_wake = 0u128;
            if (ready_mask & POLLIN) != 0 {
                row_wake |= row_slot.key_bits_in;
            }
            if (ready_mask & POLLOUT) != 0 {
                row_wake |= row_slot.key_bits_out;
            }
            if (ready_mask & (POLLERR | POLLHUP)) != 0 {
                row_wake |= row_slot.key_bits;
            }
            wake_bits |= row_wake;
        }

        for key_idx in 0..MAX_POLL_KEYS {
            if (wake_bits & key_bit(key_idx)) == 0 {
                continue;
            }
            let slot = &mut registry.key_slots[key_idx];
            if !matches!(slot.state, PollKeyState::Active) {
                continue;
            }
            slot.state = PollKeyState::Ready;
            wait_keys.push(encode_wait_key(key_idx as u8, slot.generation));
        }
    }

    for key in wait_keys {
        POLL_WAIT_QUEUE.wake_selected(key);
    }
}

/// Notify pending signal delivery for a process and wake all active poll waiters of that pid.
pub(crate) fn notify_poll_signal_pid(pid: usize) {
    debug!("notify_poll_signal_pid: pid={}", pid);
    let mut wait_keys = Vec::new();
    {
        let mut registry = POLL_REGISTRY.lock();
        for key_idx in 0..MAX_POLL_KEYS {
            let slot = &mut registry.key_slots[key_idx];
            if slot.owner_pid != pid || !matches!(slot.state, PollKeyState::Active) {
                continue;
            }
            slot.state = PollKeyState::Ready;
            wait_keys.push(encode_wait_key(key_idx as u8, slot.generation));
        }
    }

    for key in wait_keys {
        POLL_WAIT_QUEUE.wake_selected(key);
    }
}

/// Check whether a task currently has an in-flight keyed poll wait entry.
pub(crate) fn task_has_inflight_keyed_poll_wait(task: &Arc<TaskControlBlock>) -> bool {
    let task_ptr = Arc::as_ptr(task) as usize;
    let registry = POLL_REGISTRY.lock();
    registry
        .key_slots
        .iter()
        .any(|slot| slot.task_ptr == task_ptr && !matches!(slot.state, PollKeyState::Free))
}

/// Timer callback for poll timeout entries.
///
/// Returns `true` when the timer entry should be popped from heap.
pub(crate) fn handle_poll_timeout(tag: PollTimerTag, task: &Arc<TaskControlBlock>) -> bool {
    let handle = PollWaitHandle {
        key_idx: tag.key_idx,
        key_generation: tag.key_generation,
    };
    let wait_key = {
        let mut registry = POLL_REGISTRY.lock();
        if !registry.key_valid(handle) {
            return true;
        }
        let key_idx = handle.key_idx as usize;
        let generation = {
            let slot = &mut registry.key_slots[key_idx];
            if slot.task_ptr != (Arc::as_ptr(task) as usize) {
                return true;
            }
            if matches!(slot.state, PollKeyState::Ready) {
                return true;
            }
            if !matches!(slot.state, PollKeyState::Active) {
                return true;
            }
            slot.state = PollKeyState::TimedOut;
            slot.generation
        };
        registry.clear_key_rows(handle);
        encode_wait_key(handle.key_idx, generation)
    };

    POLL_WAIT_QUEUE.wake_selected(wait_key);
    true
}
