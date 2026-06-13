
use crate::config::USER_STACK_SIZE;
use crate::sched::pid2process;
use crate::syscall::{Pod, read_pod_from_user, write_pod_to_user};
use crate::syscall::errno::ERRNO;
use crate::syscall_body;
use crate::task::current_process;

pub const RLIM_INFINITY: u64 = !0u64;

/// C-compatible representation of `struct rlimit`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub struct rlimit {
    /// soft limit
    pub rlim_cur: u64,
    /// hard limit
    pub rlim_max: u64,
}

impl Pod for rlimit {}

impl rlimit {
    pub const fn unlimited() -> Self {
        Self {
            rlim_cur: RLIM_INFINITY,
            rlim_max: RLIM_INFINITY,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Resource {
    /// Max stack size
    Stack = 3,
    /// Max core file size (`RLIMIT_CORE`)
    Core = 4,
    /// Number of open files
    Nofile = 7,
    /// Max locked memory (`RLIMIT_MEMLOCK`)
    Memlock = 8,
    /// Address space limit
    As = 9,
}

impl Resource {
    fn from_raw(raw: usize) -> Option<Self> {
        match raw {
            3 => Some(Self::Stack),
            4 => Some(Self::Core),
            7 => Some(Self::Nofile),
            8 => Some(Self::Memlock),
            9 => Some(Self::As),
            _ => {
                warn!("Unsupported resource type: {}", raw);
                None
            },
        }
    }
}

/// 当前进程支持的资源限制集合。
#[derive(Copy, Clone, Debug)]
pub struct ResourceLimits {
    pub stack: rlimit,
    /// `RLIMIT_CORE`
    pub core: rlimit,
    /// `RLIMIT_NOFILE`
    pub nofile: rlimit,
    /// `RLIMIT_MEMLOCK`
    pub memlock: rlimit,
    /// `RLIMIT_AS`
    pub address_space: rlimit,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            stack: rlimit {
                rlim_cur: USER_STACK_SIZE as u64,
                rlim_max: USER_STACK_SIZE as u64,
            },
            // Core dumps are not produced, but the limit is reported as
            // unlimited so userspace (e.g. LTP) can raise/lower it freely.
            core: rlimit::unlimited(),
            nofile: rlimit {
                rlim_cur: 1024,
                rlim_max: 1024,
            },
            memlock: rlimit {
                rlim_cur: 64 * 1024,
                rlim_max: RLIM_INFINITY,
            },
            address_space: rlimit::unlimited(),
        }
    }
}

impl ResourceLimits {
    fn get(&self, resource: Resource) -> rlimit {
        match resource {
            Resource::Stack => self.stack,
            Resource::Core => self.core,
            Resource::Nofile => self.nofile,
            Resource::Memlock => self.memlock,
            Resource::As => self.address_space,
        }
    }

    fn get_mut(&mut self, resource: Resource) -> &mut rlimit {
        match resource {
            Resource::Stack => &mut self.stack,
            Resource::Core => &mut self.core,
            Resource::Nofile => &mut self.nofile,
            Resource::Memlock => &mut self.memlock,
            Resource::As => &mut self.address_space,
        }
    }
}

pub fn sys_getrlimit(resource: usize, rlim: *mut rlimit) -> isize {
    syscall_body!({
        let resource = Resource::from_raw(resource).ok_or(ERRNO::EINVAL)?;
        let process = current_process();
        let limit = {
            let inner = process.inner_exclusive_access();
            inner.resource_limits.get(resource)
        };
        write_pod_to_user(rlim, &limit)?;
        Ok(0)
    })
}

pub fn sys_setrlimit(resource: usize, rlim: *const rlimit) -> isize {
    syscall_body!({
        let resource = Resource::from_raw(resource).ok_or(ERRNO::EINVAL)?;
        let new_limit = read_pod_from_user(rlim)?;
        if new_limit.rlim_cur > new_limit.rlim_max {
            return Err(ERRNO::EINVAL);
        }

        let process = current_process();
        let mut inner = process.inner_exclusive_access();
        let current = inner.resource_limits.get(resource);
        if new_limit.rlim_max > current.rlim_max {
            return Err(ERRNO::EPERM);
        }
        *inner.resource_limits.get_mut(resource) = new_limit;
        Ok(0)
    })
}

pub fn sys_prlimit64(pid: i32, resource: usize, new_limit: *const rlimit, old_limit: *mut rlimit) -> isize {
    syscall_body!({
        if pid < 0 {
            return Err(ERRNO::EINVAL);
        }
        let resource = Resource::from_raw(resource).ok_or(ERRNO::EINVAL)?;

        let current = current_process();
        let target = if pid == 0 || pid as usize == current.getpid() {
            current.clone()
        } else {
            pid2process(pid as usize).ok_or(ERRNO::ESRCH)?
        };

        // Minimal permission model: only self/uid-matched processes (or root) are allowed.
        if !core::ptr::eq(&*target, &*current) {
            let current_uid = current.getuid();
            let current_euid = current.geteuid();
            let target_uid = target.getuid();
            let target_euid = target.geteuid();
            let allowed = current_euid == 0
                || (current_uid == target_uid
                    && current_uid == target_euid
                    && current_euid == target_uid
                    && current_euid == target_euid);
            if !allowed {
                return Err(ERRNO::EPERM);
            }
        }

        let requested_new = if new_limit.is_null() {
            None
        } else {
            Some(read_pod_from_user(new_limit)?)
        };

        let old = {
            let mut inner = target.inner_exclusive_access();
            let old = inner.resource_limits.get(resource);
            if let Some(new_limit) = requested_new {
                if new_limit.rlim_cur > new_limit.rlim_max {
                    return Err(ERRNO::EINVAL);
                }
                if new_limit.rlim_max > old.rlim_max {
                    return Err(ERRNO::EPERM);
                }
                *inner.resource_limits.get_mut(resource) = new_limit;
            }
            old
        };

        if !old_limit.is_null() {
            write_pod_to_user(old_limit, &old)?;
        }

        Ok(0)
    })
}
