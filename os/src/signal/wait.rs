//! Registry for tasks blocked in `sigtimedwait`.

use crate::sync::SpinNoIrqLock;
use crate::sched::pid2process;
use crate::task::{current_process, wakeup_task, SignalBit, TaskControlBlock};
use alloc::sync::Arc;
use alloc::vec::Vec;
use lazy_static::lazy_static;

const MAX_SIGNAL_WAITERS: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignalWaitState {
    Free,
    Active,
    Ready,
    TimedOut,
}

#[derive(Clone, Copy, Debug)]
struct SignalWaitSlot {
    generation: u8,
    state: SignalWaitState,
    pid: usize,
    signal_bits: u64,
    task_ptr: usize,
}

impl SignalWaitSlot {
    const EMPTY: Self = Self {
        generation: 0,
        state: SignalWaitState::Free,
        pid: 0,
        signal_bits: 0,
        task_ptr: 0,
    };
}

impl Default for SignalWaitSlot {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[derive(Debug)]
struct SignalWaitRegistry {
    slots: [SignalWaitSlot; MAX_SIGNAL_WAITERS],
    next_slot: usize,
}

impl SignalWaitRegistry {
    const fn new() -> Self {
        Self {
            slots: [SignalWaitSlot::EMPTY; MAX_SIGNAL_WAITERS],
            next_slot: 0,
        }
    }

    fn alloc(
        &mut self,
        pid: usize,
        signal_bits: u64,
        task: &Arc<TaskControlBlock>,
    ) -> Option<SignalWaitHandle> {
        for off in 0..MAX_SIGNAL_WAITERS {
            let idx = (self.next_slot + off) % MAX_SIGNAL_WAITERS;
            if !matches!(self.slots[idx].state, SignalWaitState::Free) {
                continue;
            }
            let slot = &mut self.slots[idx];
            slot.generation = slot.generation.wrapping_add(1);
            slot.state = SignalWaitState::Active;
            slot.pid = pid;
            slot.signal_bits = signal_bits;
            slot.task_ptr = Arc::as_ptr(task) as usize;
            self.next_slot = (idx + 1) % MAX_SIGNAL_WAITERS;
            return Some(SignalWaitHandle {
                slot_idx: idx as u8,
                generation: slot.generation,
            });
        }
        None
    }

    fn key_valid(&self, handle: SignalWaitHandle) -> bool {
        let idx = handle.slot_idx as usize;
        idx < MAX_SIGNAL_WAITERS
            && !matches!(self.slots[idx].state, SignalWaitState::Free)
            && self.slots[idx].generation == handle.generation
    }

    fn wake_state(&self, handle: SignalWaitHandle) -> SignalWakeState {
        if !self.key_valid(handle) {
            return SignalWakeState::Canceled;
        }
        match self.slots[handle.slot_idx as usize].state {
            SignalWaitState::Ready => SignalWakeState::Ready,
            SignalWaitState::TimedOut => SignalWakeState::TimedOut,
            SignalWaitState::Active | SignalWaitState::Free => SignalWakeState::Canceled,
        }
    }

    fn cleanup(&mut self, handle: SignalWaitHandle) {
        if !self.key_valid(handle) {
            return;
        }
        let slot = &mut self.slots[handle.slot_idx as usize];
        slot.state = SignalWaitState::Free;
        slot.pid = 0;
        slot.signal_bits = 0;
        slot.task_ptr = 0;
    }
}

lazy_static! {
    static ref SIGNAL_WAIT_REGISTRY: SpinNoIrqLock<SignalWaitRegistry> =
        SpinNoIrqLock::new(SignalWaitRegistry::new());
}

/// Opaque handle for one in-flight `sigtimedwait`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SignalWaitHandle {
    slot_idx: u8,
    generation: u8,
}

impl SignalWaitHandle {
    pub(crate) fn timer_tag(self) -> SignalTimerTag {
        SignalTimerTag {
            slot_idx: self.slot_idx,
            generation: self.generation,
        }
    }
}

/// Timeout identity attached to timer heap entries created by `sigtimedwait`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct SignalTimerTag {
    slot_idx: u8,
    generation: u8,
}

/// Observable wake state after a sigtimedwait sleep returns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SignalWakeState {
    Ready,
    TimedOut,
    Canceled,
}

pub(crate) fn register_signal_wait(
    pid: usize,
    signal_set: SignalBit,
    task: &Arc<TaskControlBlock>,
) -> Option<SignalWaitHandle> {
    SIGNAL_WAIT_REGISTRY
        .lock()
        .alloc(pid, signal_set.bits(), task)
}

pub(crate) fn cleanup_signal_wait(handle: SignalWaitHandle) {
    SIGNAL_WAIT_REGISTRY.lock().cleanup(handle);
}

pub(crate) fn signal_wait_state(handle: SignalWaitHandle) -> SignalWakeState {
    SIGNAL_WAIT_REGISTRY.lock().wake_state(handle)
}

pub(crate) fn signal_wait_should_skip(handle: SignalWaitHandle) -> bool {
    let registry = SIGNAL_WAIT_REGISTRY.lock();
    if !registry.key_valid(handle) {
        return true;
    }
    !matches!(
        registry.slots[handle.slot_idx as usize].state,
        SignalWaitState::Active
    )
}

pub(crate) fn cleanup_signal_wait_for_task(task: &Arc<TaskControlBlock>) {
    let task_ptr = Arc::as_ptr(task) as usize;
    let mut registry = SIGNAL_WAIT_REGISTRY.lock();
    for slot in registry.slots.iter_mut() {
        if slot.task_ptr == task_ptr && !matches!(slot.state, SignalWaitState::Free) {
            slot.state = SignalWaitState::Free;
            slot.pid = 0;
            slot.signal_bits = 0;
            slot.task_ptr = 0;
        }
    }
}

pub(crate) fn notify_signal_wait_pid(pid: usize, pending_bits: u64) {
    if pending_bits == 0 {
        return;
    }
    let mut tasks = Vec::new();
    {
        let mut registry = SIGNAL_WAIT_REGISTRY.lock();
        for slot in registry.slots.iter_mut() {
            if slot.pid != pid || !matches!(slot.state, SignalWaitState::Active) {
                continue;
            }
            if (slot.signal_bits & pending_bits) == 0 {
                continue;
            }
            slot.state = SignalWaitState::Ready;
            if let Some(task) = task_from_pid_ptr(pid, slot.task_ptr) {
                tasks.push(task);
            }
        }
    }
    for task in tasks {
        debug!("notify_signal_wait_pid: waking up task of pid {}", task.process.upgrade().unwrap().getpid());
        wakeup_task(task);
    }
}

pub(crate) fn notify_signal_wait_task(task: &Arc<TaskControlBlock>, pending_bits: u64) {
    if pending_bits == 0 {
        return;
    }
    let task_ptr = Arc::as_ptr(task) as usize;
    let mut should_wake = false;
    {
        let mut registry = SIGNAL_WAIT_REGISTRY.lock();
        for slot in registry.slots.iter_mut() {
            if slot.task_ptr != task_ptr || !matches!(slot.state, SignalWaitState::Active) {
                continue;
            }
            if (slot.signal_bits & pending_bits) == 0 {
                continue;
            }
            slot.state = SignalWaitState::Ready;
            should_wake = true;
        }
    }
    if should_wake {
        wakeup_task(Arc::clone(task));
    }
}

pub(crate) fn handle_signal_wait_timeout(
    tag: SignalTimerTag,
    task: &Arc<TaskControlBlock>,
) -> bool {
    let mut registry = SIGNAL_WAIT_REGISTRY.lock();
    let handle = SignalWaitHandle {
        slot_idx: tag.slot_idx,
        generation: tag.generation,
    };
    if !registry.key_valid(handle) {
        return true;
    }
    let slot = &mut registry.slots[handle.slot_idx as usize];
    if slot.task_ptr != Arc::as_ptr(task) as usize {
        return true;
    }
    if matches!(slot.state, SignalWaitState::Ready) {
        return true;
    }
    if !matches!(slot.state, SignalWaitState::Active) {
        return true;
    }
    slot.state = SignalWaitState::TimedOut;
    drop(registry);
    wakeup_task(Arc::clone(task));
    true
}

pub(crate) fn has_pending_signal_in_set(signal_set: SignalBit) -> bool {
    let task = crate::task::current_task().unwrap();
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let task_inner = task.inner_exclusive_access();
    !((task_inner.pending_signals | process_inner.pending_signals) & signal_set).is_empty()
}

pub(crate) fn has_unmasked_pending_signal() -> bool {
    let task = crate::task::current_task().unwrap();
    let process = current_process();
    let process_inner = process.inner_exclusive_access();
    let task_inner = task.inner_exclusive_access();
    !((task_inner.pending_signals | process_inner.pending_signals) & !task_inner.signal_mask).is_empty()
}

pub(crate) fn take_pending_signal_in_set(signal_set: SignalBit) -> Option<i32> {
    let task = crate::task::current_task().unwrap();
    let process = current_process();
    let mut process_inner = process.inner_exclusive_access();
    let mut task_inner = task.inner_exclusive_access();
    let thread_pending = task_inner.pending_signals & signal_set;
    let process_pending = process_inner.pending_signals & signal_set;
    for signum in 1..=crate::signal::MAX_SIG {
        let Some(flag) = SignalBit::from_signum(signum as u32) else {
            continue;
        };
        if thread_pending.contains(flag) {
            task_inner.pending_signals &= !flag;
            return Some(signum as i32);
        }
        if process_pending.contains(flag) {
            process_inner.pending_signals &= !flag;
            return Some(signum as i32);
        }
    }
    None
}

fn task_from_pid_ptr(pid: usize, task_ptr: usize) -> Option<Arc<TaskControlBlock>> {
    let process = pid2process(pid)?;
    let inner = process.inner_exclusive_access();
    inner
        .tasks
        .iter()
        .filter_map(|slot: &Option<Arc<TaskControlBlock>>| slot.as_ref())
        .find(|task| Arc::as_ptr(task) as usize == task_ptr)
        .map(Arc::clone)
}
