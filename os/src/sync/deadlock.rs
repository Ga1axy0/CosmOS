use alloc::vec;
use alloc::vec::Vec;

/// A lightweight detector that tracks resource availability per process.
///
/// `resource_id` maps to a mutex/semaphore id.
/// `thread_id` maps to a tid in the process.
pub struct DeadlockDetector {
    available: Vec<usize>,
    allocation: Vec<Vec<usize>>,
    need: Vec<Vec<usize>>,
}

impl DeadlockDetector {
    /// Create an empty deadlock detector.
    pub fn new() -> Self {
        Self {
            available: Vec::new(),
            allocation: Vec::new(),
            need: Vec::new(),
        }
    }

    fn ensure_thread(&mut self, tid: usize) {
        if self.allocation.len() <= tid {
            let resource_n = self.available.len();
            self.allocation.resize_with(tid + 1, || vec![0; resource_n]);
            self.need.resize_with(tid + 1, || vec![0; resource_n]);
        }
    }

    fn ensure_resource(&mut self, rid: usize) {
        if self.available.len() <= rid {
            let new_len = rid + 1;
            self.available.resize(new_len, 0);
            for row in self.allocation.iter_mut() {
                row.resize(new_len, 0);
            }
            for row in self.need.iter_mut() {
                row.resize(new_len, 0);
            }
        }
    }

    /// Initialize total available units for a resource id.
    /// Existing allocations are preserved and available is adjusted accordingly.
    pub fn init_resource(&mut self, rid: usize, total: usize) {
        self.ensure_resource(rid);
        let allocated = self
            .allocation
            .iter()
            .map(|row| row[rid])
            .fold(0usize, |a, b| a.saturating_add(b));
        self.available[rid] = total.saturating_sub(allocated);
    }

    /// Record a pending request and check if current state is still safe.
    /// If unsafe, request is rolled back and `false` is returned.
    pub fn begin_request(&mut self, tid: usize, rid: usize) -> bool {
        self.ensure_thread(tid);
        self.ensure_resource(rid);
        self.need[tid][rid] = 1;
        if self.is_safe_state() {
            true
        } else {
            self.need[tid][rid] = 0;
            false
        }
    }

    /// Mark request completed (resource granted after lock/down returns).
    pub fn finish_request(&mut self, tid: usize, rid: usize) {
        self.ensure_thread(tid);
        self.ensure_resource(rid);
        self.need[tid][rid] = 0;
        self.available[rid] = self.available[rid].saturating_sub(1);
        self.allocation[tid][rid] = self.allocation[tid][rid].saturating_add(1);
    }

    /// Release one unit from the thread.
    /// If the thread does not own this resource, treat it as semaphore signal.
    pub fn release(&mut self, tid: usize, rid: usize) {
        self.ensure_thread(tid);
        self.ensure_resource(rid);
        if self.allocation[tid][rid] > 0 {
            self.allocation[tid][rid] -= 1;
        }
        self.available[rid] = self.available[rid].saturating_add(1);
    }

    /// Cleanup a thread row on thread exit.
    pub fn clear_thread(&mut self, tid: usize) {
        if tid >= self.allocation.len() {
            return;
        }
        for rid in 0..self.available.len() {
            self.available[rid] = self.available[rid].saturating_add(self.allocation[tid][rid]);
            self.allocation[tid][rid] = 0;
            self.need[tid][rid] = 0;
        }
    }

    /// Check whether current `(Available, Allocation, Need)` is safe.
    pub fn is_safe_state(&self) -> bool {
        let thread_n = self.allocation.len();
        let resource_n = self.available.len();
        let mut work = self.available.clone();
        let mut finish = vec![false; thread_n];
        loop {
            let mut progressed = false;
            for i in 0..thread_n {
                if finish[i] {
                    continue;
                }
                let can_finish = (0..resource_n).all(|j| self.need[i][j] <= work[j]);
                if can_finish {
                    for j in 0..resource_n {
                        work[j] = work[j].saturating_add(self.allocation[i][j]);
                    }
                    finish[i] = true;
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        finish.into_iter().all(|done| done)
    }
}
