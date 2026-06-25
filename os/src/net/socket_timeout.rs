use alloc::sync::Arc;
use lazy_static::lazy_static;

use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{wakeup_task, TaskControlBlock};

const MAX_SOCKET_WAITERS: usize = 512;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SocketWaitState {
    Free,
    Active,
    Ready,
    TimedOut,
}

#[derive(Clone, Copy, Debug)]
struct SocketWaitSlot {
    generation: u8,
    state: SocketWaitState,
    task_ptr: usize,
}

impl SocketWaitSlot {
    const EMPTY: Self = Self {
        generation: 0,
        state: SocketWaitState::Free,
        task_ptr: 0,
    };
}

impl Default for SocketWaitSlot {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[derive(Debug)]
struct SocketWaitRegistry {
    slots: [SocketWaitSlot; MAX_SOCKET_WAITERS],
    next_slot: usize,
}

impl SocketWaitRegistry {
    const fn new() -> Self {
        Self {
            slots: [SocketWaitSlot::EMPTY; MAX_SOCKET_WAITERS],
            next_slot: 0,
        }
    }

    fn alloc(&mut self, task: &Arc<TaskControlBlock>) -> Option<SocketWaitHandle> {
        for off in 0..MAX_SOCKET_WAITERS {
            let idx = (self.next_slot + off) % MAX_SOCKET_WAITERS;
            if !matches!(self.slots[idx].state, SocketWaitState::Free) {
                continue;
            }
            let slot = &mut self.slots[idx];
            slot.generation = slot.generation.wrapping_add(1);
            slot.state = SocketWaitState::Active;
            slot.task_ptr = Arc::as_ptr(task) as usize;
            self.next_slot = (idx + 1) % MAX_SOCKET_WAITERS;
            return Some(SocketWaitHandle {
                slot_idx: idx as u8,
                generation: slot.generation,
            });
        }
        None
    }

    fn key_valid(&self, handle: SocketWaitHandle) -> bool {
        let idx = handle.slot_idx as usize;
        idx < MAX_SOCKET_WAITERS
            && !matches!(self.slots[idx].state, SocketWaitState::Free)
            && self.slots[idx].generation == handle.generation
    }

    fn cleanup(&mut self, handle: SocketWaitHandle) {
        if !self.key_valid(handle) {
            return;
        }
        let slot = &mut self.slots[handle.slot_idx as usize];
        slot.state = SocketWaitState::Free;
        slot.task_ptr = 0;
    }

    fn mark_ready(&mut self, handle: SocketWaitHandle) {
        if !self.key_valid(handle) {
            return;
        }
        let slot = &mut self.slots[handle.slot_idx as usize];
        if matches!(slot.state, SocketWaitState::Active) {
            slot.state = SocketWaitState::Ready;
        }
    }
}

lazy_static! {
    static ref SOCKET_WAIT_REGISTRY: SpinNoIrqLock<SocketWaitRegistry> =
        SpinNoIrqLock::new(SocketWaitRegistry::new());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SocketWaitHandle {
    slot_idx: u8,
    generation: u8,
}

impl SocketWaitHandle {
    pub(crate) fn timer_tag(self) -> SocketTimerTag {
        SocketTimerTag {
            slot_idx: self.slot_idx,
            generation: self.generation,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SocketTimerTag {
    slot_idx: u8,
    generation: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SocketWakeState {
    Ready,
    TimedOut,
    Canceled,
}

pub(crate) fn timeout_ns_to_deadline_ns(timeout_ns: u64) -> Result<Option<u64>, ERRNO> {
    if timeout_ns == 0 {
        return Ok(None);
    }
    Ok(Some(timeout_ns))
}

pub(crate) fn register_socket_wait(task: &Arc<TaskControlBlock>) -> Option<SocketWaitHandle> {
    SOCKET_WAIT_REGISTRY.lock().alloc(task)
}

pub(crate) fn cleanup_socket_wait(handle: SocketWaitHandle) {
    SOCKET_WAIT_REGISTRY.lock().cleanup(handle);
}

pub(crate) fn socket_wait_mark_ready(handle: SocketWaitHandle) {
    SOCKET_WAIT_REGISTRY.lock().mark_ready(handle);
}

pub(crate) fn socket_wait_state(handle: SocketWaitHandle) -> SocketWakeState {
    let registry = SOCKET_WAIT_REGISTRY.lock();
    if !registry.key_valid(handle) {
        return SocketWakeState::Canceled;
    }
    match registry.slots[handle.slot_idx as usize].state {
        SocketWaitState::Ready => SocketWakeState::Ready,
        SocketWaitState::TimedOut => SocketWakeState::TimedOut,
        SocketWaitState::Active | SocketWaitState::Free => SocketWakeState::Canceled,
    }
}

pub(crate) fn socket_wait_should_skip(handle: SocketWaitHandle) -> bool {
    let registry = SOCKET_WAIT_REGISTRY.lock();
    if !registry.key_valid(handle) {
        return true;
    }
    !matches!(
        registry.slots[handle.slot_idx as usize].state,
        SocketWaitState::Active
    )
}

fn wake_task_via_wait_handle(task: &Arc<TaskControlBlock>) {
    let handle = {
        let task_inner = task.inner_exclusive_access();
        task_inner.current_wq_handle.clone()
    };
    if let Some(handle) = handle {
        handle.wake_waiter(task);
        return;
    }
    wakeup_task(Arc::clone(task));
}

pub(crate) fn handle_socket_wait_timeout(
    tag: SocketTimerTag,
    task: &Arc<TaskControlBlock>,
) -> bool {
    let handle = SocketWaitHandle {
        slot_idx: tag.slot_idx,
        generation: tag.generation,
    };
    {
        let mut registry = SOCKET_WAIT_REGISTRY.lock();
        if !registry.key_valid(handle) {
            return true;
        }
        let slot = &mut registry.slots[handle.slot_idx as usize];
        if slot.task_ptr != Arc::as_ptr(task) as usize {
            return true;
        }
        if matches!(slot.state, SocketWaitState::Ready) {
            return true;
        }
        if !matches!(slot.state, SocketWaitState::Active) {
            return true;
        }
        slot.state = SocketWaitState::TimedOut;
    }
    wake_task_via_wait_handle(task);
    true
}
