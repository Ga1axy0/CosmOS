//! Persistent epoll interest sets and ready queues.
//!
//! The interest set is independent from the transient `ppoll` registry.  A
//! readiness source has a small subscriber list, so device/socket wakeups can
//! enqueue only the epoll items interested in that source.

use super::{File, FileDescription, Stat, StatMode};
use crate::mm::UserBuffer;
use crate::poll::{self, POLLERR, POLLHUP, POLLIN, POLLOUT};
use crate::sched::block_current_and_run_next;
use crate::signal::has_interrupting_signal;
use crate::sync::SpinNoIrqLock;
use crate::syscall::errno::ERRNO;
use crate::task::{current_process, current_task, TaskStatus, WaitReason};
use crate::timer::{add_current_timer_ns_preflagged, add_timer_with_poll_tag, get_time_ns};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use lazy_static::lazy_static;

pub(crate) const EPOLLIN: u32 = 0x001;
pub(crate) const EPOLLPRI: u32 = 0x002;
pub(crate) const EPOLLOUT: u32 = 0x004;
pub(crate) const EPOLLERR: u32 = 0x008;
pub(crate) const EPOLLHUP: u32 = 0x010;
pub(crate) const EPOLLRDHUP: u32 = 0x2000;
pub(crate) const EPOLLEXCLUSIVE: u32 = 1 << 28;
pub(crate) const EPOLLWAKEUP: u32 = 1 << 29;
pub(crate) const EPOLLONESHOT: u32 = 1 << 30;
pub(crate) const EPOLLET: u32 = 1 << 31;

pub(crate) const EPOLL_CTL_ADD: i32 = 1;
pub(crate) const EPOLL_CTL_DEL: i32 = 2;
pub(crate) const EPOLL_CTL_MOD: i32 = 3;

const SUPPORTED_EVENTS: u32 = EPOLLIN | EPOLLPRI | EPOLLOUT | EPOLLERR | EPOLLHUP | EPOLLRDHUP;
const UNSUPPORTED_MODIFIERS: u32 = EPOLLEXCLUSIVE | EPOLLWAKEUP | EPOLLONESHOT | EPOLLET;
const ITEM_ALIVE: usize = 1 << 0;
const ITEM_QUEUED: usize = 1 << 1;
const EPOLL_FALLBACK_POLL_NS: u64 = 10_000_000;

/// Kernel-side representation of one userspace `struct epoll_event`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EpollEvent {
    pub(crate) events: u32,
    pub(crate) data: u64,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct EpollKey {
    fd: i32,
    description_id: usize,
}

struct SubscriberBucket {
    items: SpinNoIrqLock<Vec<Weak<EpollItem>>>,
}

impl SubscriberBucket {
    fn new() -> Self {
        Self {
            items: SpinNoIrqLock::new(Vec::new()),
        }
    }
}

lazy_static! {
    /// Direct index used on readiness notification paths.
    static ref SOURCE_SUBSCRIBERS: SpinNoIrqLock<BTreeMap<usize, Arc<SubscriberBucket>>> =
        SpinNoIrqLock::new(BTreeMap::new());
    /// Index used only when the final userspace fd for an open file description closes.
    static ref DESCRIPTION_SUBSCRIBERS: SpinNoIrqLock<BTreeMap<usize, Arc<SubscriberBucket>>> =
        SpinNoIrqLock::new(BTreeMap::new());
}

struct EpollItem {
    key: EpollKey,
    source_id: usize,
    target: Arc<FileDescription>,
    owner: Weak<EpollInner>,
    events: AtomicU32,
    data: AtomicU64,
    state: AtomicUsize,
}

impl EpollItem {
    fn interested_in(&self, ready_mask: u16) -> bool {
        let events = self.events.load(Ordering::Acquire);
        (events & ready_mask as u32) != 0 || (ready_mask & (POLLERR | POLLHUP)) != 0
    }

    fn is_alive(&self) -> bool {
        self.state.load(Ordering::Acquire) & ITEM_ALIVE != 0
    }

    fn mark_dead(&self) -> bool {
        let previous = self.state.fetch_and(!ITEM_ALIVE, Ordering::AcqRel);
        previous & ITEM_ALIVE != 0
    }

    fn clear_queued(&self) {
        self.state.fetch_and(!ITEM_QUEUED, Ordering::AcqRel);
    }

    fn try_mark_queued(&self) -> bool {
        let mut state = self.state.load(Ordering::Acquire);
        loop {
            if state & ITEM_ALIVE == 0 || state & ITEM_QUEUED != 0 {
                return false;
            }
            match self.state.compare_exchange_weak(
                state,
                state | ITEM_QUEUED,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(next) => state = next,
            }
        }
    }

    fn current_ready_events(&self) -> u32 {
        let requested = self.events.load(Ordering::Acquire);
        // Several current File implementations discover HUP only while
        // evaluating their readable/writable branches.  Query both directions
        // and filter the result afterward so ERR/HUP remain unconditional as
        // required by epoll.
        let poll_mask =
            ((requested | EPOLLERR | EPOLLHUP) & u16::MAX as u32) as u16 | POLLIN | POLLOUT;
        let ready = self.target.poll(poll_mask) as u32;
        ready & (requested | EPOLLERR | EPOLLHUP)
    }
}

struct EpollInner {
    interests: SpinNoIrqLock<BTreeMap<EpollKey, Arc<EpollItem>>>,
    ready: SpinNoIrqLock<VecDeque<Arc<EpollItem>>>,
}

impl EpollInner {
    fn new() -> Self {
        Self {
            interests: SpinNoIrqLock::new(BTreeMap::new()),
            ready: SpinNoIrqLock::new(VecDeque::new()),
        }
    }

    fn source_id(&self) -> usize {
        self as *const Self as usize
    }

    fn enqueue(self: &Arc<Self>, item: Arc<EpollItem>) {
        {
            // Serialize the queued bit with insertion/removal.  Besides
            // preventing duplicates, this closes DEL-vs-enqueue races that
            // could otherwise leave a dead item behind in the queue.
            let mut ready = self.ready.lock();
            if !item.try_mark_queued() {
                return;
            }
            ready.push_back(item);
        }
        // The transient poll registry provides race-free task sleeping for
        // epoll_pwait and also makes an epoll fd itself pollable.
        poll::notify_poll_source(self.source_id(), POLLIN);
    }

    fn has_ready(&self) -> bool {
        !self.ready.lock().is_empty()
    }

    fn reserve_ready_capacity(&self, required: usize) {
        // A live item can occur in the queue at most once.  Reserving on ADD
        // keeps source notification paths from allocating in interrupt context.
        let mut ready = self.ready.lock();
        let additional = required.saturating_sub(ready.len());
        ready.reserve(additional);
    }

    fn remove_ready_item(&self, item: &Arc<EpollItem>) {
        self.ready
            .lock()
            .retain(|queued| !Arc::ptr_eq(queued, item));
        item.clear_queued();
    }

    fn collect_ready(self: &Arc<Self>, maxevents: usize) -> Vec<EpollEvent> {
        let capacity = maxevents.min(self.interests.lock().len());
        let mut output = Vec::with_capacity(capacity);
        let mut level_ready = Vec::with_capacity(capacity);

        while output.len() < maxevents {
            let Some(item) = self.ready.lock().pop_front() else {
                break;
            };
            item.clear_queued();
            if !item.is_alive() {
                continue;
            }
            let events = item.current_ready_events();
            if events == 0 {
                continue;
            }
            output.push(EpollEvent {
                events,
                data: item.data.load(Ordering::Acquire),
            });
            // Stage one implements level-triggered delivery.  Delay requeueing
            // until this batch is complete so one fd appears at most once.
            level_ready.push(item);
        }

        for item in level_ready {
            if item.is_alive() && item.current_ready_events() != 0 {
                self.enqueue(item);
            }
        }
        output
    }

    fn remove_closed_item(self: &Arc<Self>, item: &Arc<EpollItem>) {
        let removed = {
            let mut interests = self.interests.lock();
            if interests
                .get(&item.key)
                .is_some_and(|current| Arc::ptr_eq(current, item))
            {
                interests.remove(&item.key)
            } else {
                None
            }
        };
        if let Some(removed) = removed {
            removed.mark_dead();
            self.remove_ready_item(&removed);
            unsubscribe_item(&removed);
        }
    }
}

impl Drop for EpollInner {
    fn drop(&mut self) {
        let items = core::mem::take(&mut *self.interests.lock())
            .into_values()
            .collect::<Vec<_>>();
        self.ready.lock().clear();
        for item in items {
            item.mark_dead();
            unsubscribe_item(&item);
        }
    }
}

/// File object backing an epoll instance.
pub(crate) struct EpollFile {
    inner: Arc<EpollInner>,
}

impl EpollFile {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(EpollInner::new()),
        }
    }

    pub(crate) fn validate_events(events: u32) -> Result<(), ERRNO> {
        if events & UNSUPPORTED_MODIFIERS != 0
            || events & !(SUPPORTED_EVENTS | UNSUPPORTED_MODIFIERS) != 0
        {
            return Err(ERRNO::EINVAL);
        }
        Ok(())
    }

    pub(crate) fn ctl_add(
        &self,
        fd: i32,
        target: Arc<FileDescription>,
        event: EpollEvent,
    ) -> Result<(), ERRNO> {
        Self::validate_events(event.events)?;
        if !target.has_fd_refs() {
            return Err(ERRNO::EBADF);
        }
        if target.is_seekable() {
            return Err(ERRNO::EPERM);
        }
        if target.as_any().downcast_ref::<EpollFile>().is_some() {
            // Nested epoll and cycle detection belong to a later phase.
            return Err(ERRNO::EINVAL);
        }
        let key = EpollKey {
            fd,
            description_id: target.identity(),
        };
        let source_id = target.poll_source_id();
        let item = Arc::new(EpollItem {
            key,
            source_id,
            target,
            owner: Arc::downgrade(&self.inner),
            events: AtomicU32::new(event.events),
            data: AtomicU64::new(event.data),
            state: AtomicUsize::new(ITEM_ALIVE),
        });

        {
            let mut interests = self.inner.interests.lock();
            if interests.contains_key(&key) {
                return Err(ERRNO::EEXIST);
            }
            self.inner
                .reserve_ready_capacity(interests.len().saturating_add(1));
            interests.insert(key, Arc::clone(&item));
        }
        subscribe_item(&item);
        // A concurrent close may have dropped the final descriptor after the
        // syscall looked it up but before the persistent subscription became
        // visible to close cleanup.  Recheck after subscribing; a later close
        // will observe the item through DESCRIPTION_SUBSCRIBERS itself.
        if !item.target.has_fd_refs() {
            self.inner.remove_closed_item(&item);
            return Err(ERRNO::EBADF);
        }
        // Subscribe before checking readiness to close the ADD-vs-notify race.
        if item.current_ready_events() != 0 {
            self.inner.enqueue(item);
        }
        Ok(())
    }

    pub(crate) fn ctl_mod(
        &self,
        fd: i32,
        target: &Arc<FileDescription>,
        event: EpollEvent,
    ) -> Result<(), ERRNO> {
        Self::validate_events(event.events)?;
        let key = EpollKey {
            fd,
            description_id: target.identity(),
        };
        let item = self
            .inner
            .interests
            .lock()
            .get(&key)
            .cloned()
            .ok_or(ERRNO::ENOENT)?;
        item.data.store(event.data, Ordering::Release);
        item.events.store(event.events, Ordering::Release);
        if item.current_ready_events() != 0 {
            self.inner.enqueue(item);
        }
        Ok(())
    }

    pub(crate) fn ctl_del(&self, fd: i32, target: &Arc<FileDescription>) -> Result<(), ERRNO> {
        let key = EpollKey {
            fd,
            description_id: target.identity(),
        };
        let item = self
            .inner
            .interests
            .lock()
            .remove(&key)
            .ok_or(ERRNO::ENOENT)?;
        item.mark_dead();
        self.inner.remove_ready_item(&item);
        unsubscribe_item(&item);
        Ok(())
    }

    /// Wait for level-triggered events.  The existing transient poll key is
    /// used only to sleep this task on the epoll instance's own source id.
    pub(crate) fn wait(
        &self,
        epfd: usize,
        maxevents: usize,
        deadline_ns: Option<u64>,
    ) -> Result<Vec<EpollEvent>, ERRNO> {
        loop {
            let ready = self.inner.collect_ready(maxevents);
            if !ready.is_empty() {
                return Ok(ready);
            }
            if has_interrupting_signal() {
                return Err(ERRNO::EINTR);
            }
            let now_ns = get_time_ns();
            if deadline_ns.is_some_and(|deadline| now_ns >= deadline) {
                return Ok(Vec::new());
            }

            let task = current_task().unwrap();
            let pid = current_process().getpid();
            let interests = [(epfd, self.inner.source_id(), POLLIN)];
            let handle = match poll::register_poll_wait(pid, &task, &interests) {
                Ok(handle) => handle,
                Err(ERRNO::ENOSPC) => {
                    let sleep_until = deadline_ns
                        .map(|deadline| {
                            now_ns.saturating_add(
                                EPOLL_FALLBACK_POLL_NS.min(deadline.saturating_sub(now_ns)),
                            )
                        })
                        .unwrap_or_else(|| now_ns.saturating_add(EPOLL_FALLBACK_POLL_NS));
                    {
                        let mut task_inner = task.inner_exclusive_access();
                        task_inner.task_status = TaskStatus::Interruptible;
                        task_inner.wait_reason = Some(WaitReason::Poll);
                        task_inner.may_have_non_futex_timer = true;
                    }
                    add_current_timer_ns_preflagged(sleep_until, Arc::clone(&task));
                    block_current_and_run_next(WaitReason::Poll);
                    continue;
                }
                Err(errno) => return Err(errno),
            };

            // Recheck after publishing the wait key.  A notification before
            // registration is visible through the ready queue; a later one
            // will mark the key ready.
            if self.inner.has_ready() || has_interrupting_signal() {
                poll::cleanup_poll_wait(handle);
                continue;
            }
            if let Some(deadline) = deadline_ns {
                add_timer_with_poll_tag(deadline, Arc::clone(&task), Some(handle.timer_tag()));
            }
            poll::wait_poll_key(handle);
            let wake_state = poll::poll_wait_state(handle);
            poll::cleanup_poll_wait(handle);
            if matches!(wake_state, poll::PollWakeState::TimedOut) {
                return Ok(Vec::new());
            }
        }
    }
}

impl File for EpollFile {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        true
    }

    fn read_at_result(&self, _offset: usize, _buf: UserBuffer) -> Result<usize, ERRNO> {
        Err(ERRNO::EINVAL)
    }

    fn write_at_result(&self, _offset: usize, _buf: UserBuffer) -> Result<usize, ERRNO> {
        Err(ERRNO::EINVAL)
    }

    fn poll(&self, events: u16) -> u16 {
        if events & POLLIN != 0 && self.inner.has_ready() {
            POLLIN
        } else {
            0
        }
    }

    fn poll_source_id(&self) -> usize {
        self.inner.source_id()
    }

    fn stat(&self) -> Stat {
        Stat {
            dev: 0,
            ino: self.inner.source_id() as u64,
            mode: StatMode::FILE,
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

fn bucket_for(
    registry: &SpinNoIrqLock<BTreeMap<usize, Arc<SubscriberBucket>>>,
    id: usize,
) -> Arc<SubscriberBucket> {
    let mut registry = registry.lock();
    Arc::clone(
        registry
            .entry(id)
            .or_insert_with(|| Arc::new(SubscriberBucket::new())),
    )
}

fn existing_bucket(
    registry: &SpinNoIrqLock<BTreeMap<usize, Arc<SubscriberBucket>>>,
    id: usize,
) -> Option<Arc<SubscriberBucket>> {
    registry.lock().get(&id).cloned()
}

fn subscribe_item(item: &Arc<EpollItem>) {
    bucket_for(&SOURCE_SUBSCRIBERS, item.source_id)
        .items
        .lock()
        .push(Arc::downgrade(item));
    bucket_for(&DESCRIPTION_SUBSCRIBERS, item.key.description_id)
        .items
        .lock()
        .push(Arc::downgrade(item));
}

fn unsubscribe_from(bucket: Option<Arc<SubscriberBucket>>, item: &Arc<EpollItem>) {
    if let Some(bucket) = bucket {
        let item_weak = Arc::downgrade(item);
        bucket
            .items
            .lock()
            .retain(|weak| weak.strong_count() != 0 && !weak.ptr_eq(&item_weak));
    }
}

fn unsubscribe_item(item: &Arc<EpollItem>) {
    unsubscribe_from(existing_bucket(&SOURCE_SUBSCRIBERS, item.source_id), item);
    unsubscribe_from(
        existing_bucket(&DESCRIPTION_SUBSCRIBERS, item.key.description_id),
        item,
    );
}

/// Deliver one source notification to its persistent epoll subscribers.
pub(crate) fn notify_source(source_id: usize, ready_mask: u16) {
    let Some(bucket) = existing_bucket(&SOURCE_SUBSCRIBERS, source_id) else {
        return;
    };
    let items = bucket.items.lock();
    for weak in items.iter() {
        let Some(item) = weak.upgrade() else {
            continue;
        };
        if !item.is_alive() || !item.interested_in(ready_mask) {
            continue;
        }
        if let Some(owner) = item.owner.upgrade() {
            owner.enqueue(item);
        }
    }
}

/// Remove interests after the last fd referencing an open file description is
/// closed.  Epoll's own Arc does not count as a userspace fd reference.
pub(crate) fn notify_file_description_closed(description_id: usize) {
    let Some(bucket) = existing_bucket(&DESCRIPTION_SUBSCRIBERS, description_id) else {
        return;
    };
    // Closing is a task-context operation, so taking a snapshot is preferable
    // to holding the bucket lock while DEL acquires per-epoll interest locks.
    let items = bucket
        .items
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    for item in items {
        if !item.mark_dead() {
            continue;
        }
        if let Some(owner) = item.owner.upgrade() {
            owner.remove_closed_item(&item);
        }
    }
}
