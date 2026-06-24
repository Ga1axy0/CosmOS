//! Minimal procfs implementation for `/proc`.
//!
//! Provides:
//! - `/proc/meminfo` — basic memory statistics.
//! - `/proc/mounts`  — current mount table.
//! - `/proc/self`    — symlink to current process directory.
//! - `/proc/<pid>/exe` — symlink to process executable path.
//! - `/proc/<pid>/maps` — virtual memory regions of the process.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::fmt::Write;

use fs::errno::FS_ERRNO;
use fs::vfs::{VfsFileType, VfsNode};

use crate::config::{MAX_HARTS, PAGE_SIZE};
use crate::drivers::block::BLOCK_DEVICES;
#[cfg(feature = "io_perf_counters")]
use crate::drivers::block as block_drivers;
use crate::fs::devfs::{blkdev_major_from_name, blkdev_minor_from_name};
use crate::fs::inode::snapshot_mount_table;
#[cfg(feature = "io_perf_counters")]
use crate::fs::page_cache;
use crate::fs::PAGE_CACHE_MANAGER;
use crate::keys;
use crate::mm::{frame_allocator_stats, MapPermission, VmaKind};
#[cfg(feature = "net_perf_counters")]
use crate::net;
use crate::sched::{list_pids, pid2process};
use crate::signal::{MAX_SIG, SIG_IGN};
use crate::task::{current_process, TaskStatus};
use crate::timer::{get_time, time_to_ticks};

fn parse_pid(name: &str) -> Option<usize> {
    if name.is_empty() || !name.chars().all(|ch| ch.is_ascii_digit()) {
        return None;
    }
    name.parse().ok()
}

fn build_meminfo() -> String {
    let stats = frame_allocator_stats();
    let cached_pages = PAGE_CACHE_MANAGER.lock().cached_pages;
    let page_kb = (PAGE_SIZE as u64) / 1024;
    let mem_total = stats.total_pages as u64 * page_kb;
    let mem_free = stats.free_pages as u64 * page_kb;
    let cached = cached_pages as u64 * page_kb;
    let mem_available = mem_free.saturating_add(cached);

    let mut out = String::new();
    let _ = writeln!(&mut out, "MemTotal:       {} kB", mem_total);
    let _ = writeln!(&mut out, "MemFree:        {} kB", mem_free);
    let _ = writeln!(&mut out, "MemAvailable:   {} kB", mem_available);
    let _ = writeln!(&mut out, "Cached:         {} kB", cached);
    out
}

fn build_cpuinfo() -> String {
    let mut out = String::new();
    for hart in 0..MAX_HARTS {
        let _ = writeln!(&mut out, "processor\t: {}", hart);
        let _ = writeln!(&mut out, "model name\t: QEMU Virtual CPU");
        #[cfg(target_arch = "riscv64")]
        {
            let _ = writeln!(&mut out, "hart\t\t: {}", hart);
            let _ = writeln!(&mut out, "isa\t\t: rv64imafdc");
            let _ = writeln!(&mut out, "mmu\t\t: sv39");
        }
        #[cfg(target_arch = "loongarch64")]
        {
            let _ = writeln!(&mut out, "CPU Family\t: LoongArch");
            let _ = writeln!(&mut out, "Model Name\t: CosmOS virtual CPU");
        }
        let _ = writeln!(&mut out);
    }
    out
}

fn escape_mount_field(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        match ch {
            ' ' => out.push_str("\\040"),
            '\t' => out.push_str("\\011"),
            '\n' => out.push_str("\\012"),
            '\\' => out.push_str("\\134"),
            _ => out.push(ch),
        }
    }
    out
}

fn build_mounts() -> String {
    let mut out = String::new();
    for mount in snapshot_mount_table() {
        let _ = writeln!(
            &mut out,
            "{} {} {} {} 0 0",
            escape_mount_field(&mount.source),
            escape_mount_field(&mount.target),
            escape_mount_field(&mount.fs_type),
            escape_mount_field(&mount.options),
        );
    }
    out
}

fn build_filesystems() -> String {
    String::from("nodev\ttmpfs\nnodev\tproc\nnodev\tsysfs\nnodev\tcgroup2\n\text4\n\tvfat\n")
}

fn build_partitions() -> String {
    let mut out = String::from("major minor  #blocks  name\n\n");
    for dev_name in BLOCK_DEVICES.lock().keys() {
        if dev_name.as_str() == "loop-control" {
            continue;
        }
        let _ = writeln!(
            &mut out,
            "{:>4} {:>7} {:>10} {}",
            blkdev_major_from_name(dev_name),
            blkdev_minor_from_name(dev_name),
            524288,
            dev_name,
        );
    }
    out
}

#[cfg(feature = "io_perf_counters")]
fn reset_io_perf() {
    ::fs::block_cache::reset_perf_counters();
    ::fs::ext4::reset_perf_counters();
    block_drivers::reset_perf_counters();
    page_cache::reset_perf_counters();
}

#[cfg(feature = "io_perf_counters")]
fn build_io_perf() -> String {
    let mut out = String::new();
    out.push_str(&block_drivers::render_perf_counters());
    out.push_str(&::fs::block_cache::render_perf_counters());
    out.push_str(&::fs::ext4::render_perf_counters());
    out.push_str(&page_cache::render_perf_counters());
    out
}

fn parse_proc_u32(buf: &[u8]) -> Result<u32, FS_ERRNO> {
    let text = core::str::from_utf8(buf).map_err(|_| FS_ERRNO::EINVAL)?.trim();
    text.parse::<u32>().map_err(|_| FS_ERRNO::EINVAL)
}

fn parse_proc_usize(buf: &[u8]) -> Result<usize, FS_ERRNO> {
    let text = core::str::from_utf8(buf).map_err(|_| FS_ERRNO::EINVAL)?.trim();
    text.parse::<usize>().map_err(|_| FS_ERRNO::EINVAL)
}

fn parse_proc_i32(buf: &[u8]) -> Result<i32, FS_ERRNO> {
    let text = core::str::from_utf8(buf).map_err(|_| FS_ERRNO::EINVAL)?.trim();
    text.parse::<i32>().map_err(|_| FS_ERRNO::EINVAL)
}

fn parse_proc_bool(buf: &[u8]) -> Result<bool, FS_ERRNO> {
    match parse_proc_u32(buf)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(FS_ERRNO::EINVAL),
    }
}

fn read_string_at(data: String, offset: usize, buf: &mut [u8]) -> usize {
    if buf.is_empty() {
        return 0;
    }
    let bytes = data.as_bytes();
    if offset >= bytes.len() {
        return 0;
    }
    let end = (offset + buf.len()).min(bytes.len());
    let len = end - offset;
    buf[..len].copy_from_slice(&bytes[offset..end]);
    len
}

fn process_name_from_path(path: &str) -> String {
    let name = path
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(path);
    if name.is_empty() {
        String::from("unknown")
    } else {
        name.replace(')', "]")
    }
}

fn task_state_char(status: TaskStatus, is_zombie: bool) -> char {
    if is_zombie || matches!(status, TaskStatus::Zombie) {
        'Z'
    } else {
        match status {
            TaskStatus::Running | TaskStatus::Runnable => 'R',
            TaskStatus::Interruptible => 'S',
            TaskStatus::Uninterruptible => 'D',
            TaskStatus::Zombie => 'Z',
        }
    }
}

fn task_state_label(state: char) -> &'static str {
    match state {
        'R' => "running",
        'S' => "sleeping",
        'D' => "disk sleep",
        'Z' => "zombie",
        _ => "unknown",
    }
}

fn stat_priority(policy: crate::sched::SchedPolicy, nice: i32, rt_priority: u8) -> i64 {
    if policy.is_rt() {
        -(rt_priority as i64) - 1
    } else {
        (nice + 20) as i64
    }
}

fn kb_string(bytes: usize) -> String {
    ((bytes as u64) / 1024).to_string()
}

fn mask_to_cpu_list(mask: usize) -> String {
    if mask == 0 {
        return String::from("0");
    }
    let mut out = String::new();
    let mut first = true;
    let limit = MAX_HARTS.min(usize::BITS as usize);
    let mut cpu = 0usize;
    while cpu < limit {
        if (mask & (1usize << cpu)) == 0 {
            cpu += 1;
            continue;
        }
        let start = cpu;
        while cpu + 1 < limit && (mask & (1usize << (cpu + 1))) != 0 {
            cpu += 1;
        }
        if !first {
            out.push(',');
        }
        first = false;
        if start == cpu {
            let _ = write!(&mut out, "{}", start);
        } else {
            let _ = write!(&mut out, "{}-{}", start, cpu);
        }
        cpu += 1;
    }
    out
}

fn build_pid_stat(pid: usize) -> Result<String, FS_ERRNO> {
    let process = pid2process(pid).ok_or(FS_ERRNO::ENOENT)?;
    let now = get_time();
    let (
        comm,
        ppid,
        pgrp,
        session,
        is_zombie,
        num_threads,
        vsize,
        start_stack,
        start_brk,
        proc_pending,
        sig_ignored,
        sig_caught,
        text_start,
        text_end,
        main_task,
    ) = {
        let inner = process.inner_exclusive_access();
        let comm = process_name_from_path(&inner.exec_path);
        let ppid = inner
            .parent
            .as_ref()
            .and_then(|parent| parent.upgrade())
            .map(|parent| parent.getpid())
            .unwrap_or(0);
        let pgrp = inner.cred.pgid;
        let session = inner.cred.sid;
        let is_zombie = inner.is_zombie;
        let num_threads = inner.thread_count().max(if inner.is_zombie { 1 } else { 0 });
        let vsize = inner.address_space_bytes();
        let start_stack = inner.vm_layout.start_stack;
        let start_brk = inner.vm_layout.start_brk;
        let proc_pending = inner.pending_signals.bits();
        let mut sig_ignored = 0u64;
        let mut sig_caught = 0u64;
        for signum in 1..=MAX_SIG {
            let action = inner.signal_actions.table[signum];
            if action.handler == SIG_IGN {
                sig_ignored |= 1u64 << (signum - 1);
            } else if action.handler > 1 {
                sig_caught |= 1u64 << (signum - 1);
            }
        }
        let mut text_start = 0usize;
        let mut text_end = 0usize;
        for vma in inner.memory_set.vmas.values() {
            if !matches!(vma.kind, VmaKind::Elf) {
                continue;
            }
            let start = usize::from(vma.start_vpn()) * PAGE_SIZE;
            let end = usize::from(vma.end_vpn()) * PAGE_SIZE;
            if text_start == 0 || start < text_start {
                text_start = start;
            }
            if end > text_end {
                text_end = end;
            }
        }
        (
            comm,
            ppid,
            pgrp,
            session,
            is_zombie,
            num_threads,
            vsize,
            start_stack,
            start_brk,
            proc_pending,
            sig_ignored,
            sig_caught,
            text_start,
            text_end,
            inner.tasks.first().and_then(|task| task.as_ref()).cloned(),
        )
    };

    let (utime_raw, stime_raw, cutime_raw, cstime_raw) = process.times_snapshot(now);
    let utime = time_to_ticks(utime_raw);
    let stime = time_to_ticks(stime_raw);
    let cutime = time_to_ticks(cutime_raw);
    let cstime = time_to_ticks(cstime_raw);

    let (state, nice, priority, signal_mask, thread_pending, processor, rt_priority, policy, exit_code) =
        if let Some(task) = main_task {
            let task_inner = task.inner_exclusive_access();
            let state = task_state_char(task_inner.task_status, is_zombie);
            let nice = task_inner.sched.nice;
            let priority = stat_priority(task_inner.sched.policy, nice, task_inner.sched.rt_priority);
            (
                state,
                nice,
                priority,
                task_inner.signal_mask.bits(),
                task_inner.pending_signals.bits(),
                task_inner.sched.last_cpu,
                task_inner.sched.rt_priority as u64,
                task_inner.sched.policy as i32 as u64,
                task_inner.exit_code.unwrap_or(0),
            )
        } else {
            ('Z', 0, 20, 0, 0, 0, 0, 0, 0)
        };

    let signal = proc_pending | thread_pending;
    let blocked = signal_mask;
    let mut out = String::new();
    let _ = writeln!(
        &mut out,
        "{} ({}) {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}",
        pid,
        comm,
        state,
        ppid,
        pgrp,
        session,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        utime,
        stime,
        cutime,
        cstime,
        priority,
        nice,
        num_threads,
        0,
        0,
        vsize,
        0,
        0,
        text_start,
        text_end,
        start_stack,
        0,
        0,
        signal,
        blocked,
        sig_ignored,
        sig_caught,
        0,
        0,
        0,
        17,
        processor,
        rt_priority,
        policy,
        0,
        0,
        0,
        0,
        0,
        start_brk,
        0,
        0,
        0,
        0,
        exit_code,
    );
    Ok(out)
}

fn build_pid_status(pid: usize) -> Result<String, FS_ERRNO> {
    let process = pid2process(pid).ok_or(FS_ERRNO::ENOENT)?;
    let (
        name,
        umask,
        ppid,
        uid,
        euid,
        suid,
        gid,
        egid,
        sgid,
        pgrp,
        session,
        proc_pending,
        sig_ignored,
        sig_caught,
        fd_size,
        num_threads,
        vsize,
        start_brk,
        current_brk,
        text_start,
        text_end,
        is_zombie,
        main_task,
    ) = {
        let inner = process.inner_exclusive_access();
        let name = process_name_from_path(&inner.exec_path);
        let umask = inner.umask;
        let ppid = inner
            .parent
            .as_ref()
            .and_then(|parent| parent.upgrade())
            .map(|parent| parent.getpid())
            .unwrap_or(0);
        let uid = inner.cred.uid;
        let euid = inner.cred.euid;
        let suid = inner.cred.suid;
        let gid = inner.cred.gid;
        let egid = inner.cred.egid;
        let sgid = inner.cred.sgid;
        let pgrp = inner.cred.pgid;
        let session = inner.cred.sid;
        let proc_pending = inner.pending_signals.bits();
        let fd_size = inner.fd_table.len();
        let num_threads = inner.thread_count().max(if inner.is_zombie { 1 } else { 0 });
        let vsize = inner.address_space_bytes();
        let start_brk = inner.vm_layout.start_brk;
        let current_brk = inner.vm_layout.brk;
        let is_zombie = inner.is_zombie;
        let mut sig_ignored = 0u64;
        let mut sig_caught = 0u64;
        for signum in 1..=MAX_SIG {
            let action = inner.signal_actions.table[signum];
            if action.handler == SIG_IGN {
                sig_ignored |= 1u64 << (signum - 1);
            } else if action.handler > 1 {
                sig_caught |= 1u64 << (signum - 1);
            }
        }
        let mut text_start = 0usize;
        let mut text_end = 0usize;
        for vma in inner.memory_set.vmas.values() {
            if !matches!(vma.kind, VmaKind::Elf) {
                continue;
            }
            let start = usize::from(vma.start_vpn()) * PAGE_SIZE;
            let end = usize::from(vma.end_vpn()) * PAGE_SIZE;
            if text_start == 0 || start < text_start {
                text_start = start;
            }
            if end > text_end {
                text_end = end;
            }
        }
        (
            name,
            umask,
            ppid,
            uid,
            euid,
            suid,
            gid,
            egid,
            sgid,
            pgrp,
            session,
            proc_pending,
            sig_ignored,
            sig_caught,
            fd_size,
            num_threads,
            vsize,
            start_brk,
            current_brk,
            text_start,
            text_end,
            is_zombie,
            inner.tasks.first().and_then(|task| task.as_ref()).cloned(),
        )
    };

    let tgid = pid;
    let (state, thread_pending, blocked, affinity_mask) = if let Some(task) = main_task {
        let task_inner = task.inner_exclusive_access();
        (
            task_state_char(task_inner.task_status, is_zombie),
            task_inner.pending_signals.bits(),
            task_inner.signal_mask.bits(),
            task_inner.sched.cpu_affinity_mask,
        )
    } else {
        (task_state_char(TaskStatus::Zombie, true), 0, 0, 0)
    };

    let mut out = String::new();
    let _ = writeln!(&mut out, "Name:\t{}", name);
    let _ = writeln!(&mut out, "Umask:\t{:04o}", umask);
    let _ = writeln!(&mut out, "State:\t{} ({})", state, task_state_label(state));
    let _ = writeln!(&mut out, "Tgid:\t{}", tgid);
    let _ = writeln!(&mut out, "Ngid:\t0");
    let _ = writeln!(&mut out, "Pid:\t{}", pid);
    let _ = writeln!(&mut out, "PPid:\t{}", ppid);
    let _ = writeln!(&mut out, "TracerPid:\t0");
    let _ = writeln!(&mut out, "Uid:\t{}\t{}\t{}\t{}", uid, euid, suid, euid);
    let _ = writeln!(&mut out, "Gid:\t{}\t{}\t{}\t{}", gid, egid, sgid, egid);
    let _ = writeln!(&mut out, "FDSize:\t{}", fd_size);
    let _ = writeln!(&mut out, "Groups:\t");
    let _ = writeln!(&mut out, "NStgid:\t{}", tgid);
    let _ = writeln!(&mut out, "NSpid:\t{}", pid);
    let _ = writeln!(&mut out, "NSpgid:\t{}", pgrp);
    let _ = writeln!(&mut out, "NSsid:\t{}", session);
    let _ = writeln!(&mut out, "VmPeak:\t{} kB", kb_string(vsize));
    let _ = writeln!(&mut out, "VmSize:\t{} kB", kb_string(vsize));
    let _ = writeln!(&mut out, "VmLck:\t0 kB");
    let _ = writeln!(&mut out, "VmPin:\t0 kB");
    let _ = writeln!(&mut out, "VmHWM:\t0 kB");
    let _ = writeln!(&mut out, "VmRSS:\t0 kB");
    let _ = writeln!(&mut out, "RssAnon:\t0 kB");
    let _ = writeln!(&mut out, "RssFile:\t0 kB");
    let _ = writeln!(&mut out, "RssShmem:\t0 kB");
    let _ = writeln!(&mut out, "VmData:\t{} kB", kb_string(current_brk.saturating_sub(start_brk)));
    let _ = writeln!(&mut out, "VmStk:\t0 kB");
    let _ = writeln!(&mut out, "VmExe:\t{} kB", kb_string(text_end.saturating_sub(text_start)));
    let _ = writeln!(&mut out, "VmLib:\t0 kB");
    let _ = writeln!(&mut out, "VmPTE:\t0 kB");
    let _ = writeln!(&mut out, "VmSwap:\t0 kB");
    let _ = writeln!(&mut out, "Threads:\t{}", num_threads);
    let _ = writeln!(&mut out, "SigQ:\t0/0");
    let _ = writeln!(&mut out, "SigPnd:\t{:016x}", thread_pending);
    let _ = writeln!(&mut out, "ShdPnd:\t{:016x}", proc_pending);
    let _ = writeln!(&mut out, "SigBlk:\t{:016x}", blocked);
    let _ = writeln!(&mut out, "SigIgn:\t{:016x}", sig_ignored);
    let _ = writeln!(&mut out, "SigCgt:\t{:016x}", sig_caught);
    let _ = writeln!(&mut out, "CapInh:\t0000000000000000");
    let _ = writeln!(&mut out, "CapPrm:\t0000000000000000");
    let _ = writeln!(&mut out, "CapEff:\t0000000000000000");
    let _ = writeln!(&mut out, "CapBnd:\t0000000000000000");
    let _ = writeln!(&mut out, "CapAmb:\t0000000000000000");
    let _ = writeln!(&mut out, "NoNewPrivs:\t0");
    let _ = writeln!(&mut out, "Seccomp:\t0");
    let _ = writeln!(&mut out, "Seccomp_filters:\t0");
    let _ = writeln!(&mut out, "Cpus_allowed:\t{:x}", affinity_mask);
    let _ = writeln!(&mut out, "Cpus_allowed_list:\t{}", mask_to_cpu_list(affinity_mask));
    let _ = writeln!(&mut out, "Mems_allowed:\t1");
    let _ = writeln!(&mut out, "Mems_allowed_list:\t0");
    let _ = writeln!(&mut out, "voluntary_ctxt_switches:\t0");
    let _ = writeln!(&mut out, "nonvoluntary_ctxt_switches:\t0");
    Ok(out)
}

/// Render the permission column (`rwxp`) for a `/proc/<pid>/maps` line.
/// File mappings and `MAP_SHARED|MAP_ANONYMOUS` both report `s`.
fn maps_perm_string(perm: MapPermission, shared: bool) -> [u8; 4] {
    [
        if perm.contains(MapPermission::R) { b'r' } else { b'-' },
        if perm.contains(MapPermission::W) { b'w' } else { b'-' },
        if perm.contains(MapPermission::X) { b'x' } else { b'-' },
        if shared { b's' } else { b'p' },
    ]
}

/// Build the contents of `/proc/<pid>/maps`: one line per user VMA, formatted
/// like Linux (`start-end perms offset dev inode path`).
fn build_pid_maps(pid: usize) -> Result<String, FS_ERRNO> {
    let process = pid2process(pid).ok_or(FS_ERRNO::ENOENT)?;
    let inner = process.inner_exclusive_access();
    let mut out = String::new();
    for vma in inner.memory_set.vmas.values() {
        // Kernel-only regions are not part of the user address space view.
        if matches!(vma.kind, VmaKind::Kernel) {
            continue;
        }
        let start = usize::from(vma.start_vpn()) * PAGE_SIZE;
        let end = usize::from(vma.end_vpn()) * PAGE_SIZE;
        let shared = vma.file.as_ref().map(|f| f.shared).unwrap_or(false) || vma.shared_anon;
        let pgoff = vma.file.as_ref().map(|f| f.pgoff * PAGE_SIZE).unwrap_or(0);
        let perms = maps_perm_string(vma.map_perm, shared);
        let perms = core::str::from_utf8(&perms).unwrap_or("----");
        let label = match vma.kind {
            VmaKind::Heap => "[heap]",
            VmaKind::UserStack { .. } => "[stack]",
            VmaKind::Vdso => "[vdso]",
            _ => "",
        };
        let _ = write!(
            &mut out,
            "{:08x}-{:08x} {} {:08x} 00:00 0 ",
            start, end, perms, pgoff
        );
        if !label.is_empty() {
            let _ = writeln!(&mut out, "                  {}", label);
        } else {
            let _ = writeln!(&mut out);
        }
    }
    Ok(out)
}


/// `/proc` root directory node.
#[derive(Default, Debug)]
pub struct ProcRootNode;

impl ProcRootNode {
    /// Create a new procfs root node.
    pub fn new() -> Self {
        Self
    }
}

impl VfsNode for ProcRootNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        let mut entries = Vec::new();
        entries.push((String::from("self"), VfsFileType::Symlink));
        entries.push((String::from("cpuinfo"), VfsFileType::Regular));
        entries.push((String::from("filesystems"), VfsFileType::Regular));
        entries.push((String::from("meminfo"), VfsFileType::Regular));
        entries.push((String::from("mounts"), VfsFileType::Regular));
        entries.push((String::from("partitions"), VfsFileType::Regular));
        #[cfg(feature = "io_perf_counters")]
        entries.push((String::from("io_perf"), VfsFileType::Regular));
        #[cfg(feature = "net_perf_counters")]
        entries.push((String::from("net_perf"), VfsFileType::Regular));
        entries.push((String::from("key-users"), VfsFileType::Regular));
        entries.push((String::from("sys"), VfsFileType::Directory));
        for pid in list_pids() {
            entries.push((alloc::format!("{}", pid), VfsFileType::Directory));
        }
        entries
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        match name {
            "self" => Some(Arc::new(ProcSelfLinkNode::new()) as Arc<dyn VfsNode>),
            "cpuinfo" => Some(Arc::new(ProcCpuinfoNode::new()) as Arc<dyn VfsNode>),
            "filesystems" => Some(Arc::new(ProcFilesystemsNode::new()) as Arc<dyn VfsNode>),
            "meminfo" => Some(Arc::new(ProcMeminfoNode::new()) as Arc<dyn VfsNode>),
            "mounts" => Some(Arc::new(ProcMountsNode::new()) as Arc<dyn VfsNode>),
            "partitions" => Some(Arc::new(ProcPartitionsNode::new()) as Arc<dyn VfsNode>),
            #[cfg(feature = "io_perf_counters")]
            "io_perf" => Some(Arc::new(ProcIoPerfNode::new()) as Arc<dyn VfsNode>),
            #[cfg(feature = "net_perf_counters")]
            "net_perf" => Some(Arc::new(ProcNetPerfNode::new()) as Arc<dyn VfsNode>),
            "key-users" => Some(Arc::new(ProcKeyUsersNode::new()) as Arc<dyn VfsNode>),
            "sys" => Some(Arc::new(ProcStaticDirNode::new(ProcStaticDirKind::Sys)) as Arc<dyn VfsNode>),
            _ => {
                let pid = parse_pid(name)?;
                if pid2process(pid).is_some() {
                    Some(Arc::new(ProcPidDirNode::new(pid)) as Arc<dyn VfsNode>)
                } else {
                    None
                }
            }
        }
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

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

#[derive(Debug, Clone, Copy)]
enum ProcStaticDirKind {
    Sys,
    Kernel,
    Keys,
    Net,
    NetIpv4,
    NetIpv4Conf,
    NetIpv4ConfAll,
    NetIpv4ConfDefault,
    NetIpv4ConfLo,
}

#[derive(Debug)]
struct ProcStaticDirNode {
    kind: ProcStaticDirKind,
}

impl ProcStaticDirNode {
    fn new(kind: ProcStaticDirKind) -> Self {
        Self { kind }
    }
}

impl VfsNode for ProcStaticDirNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        match self.kind {
            ProcStaticDirKind::Sys => alloc::vec![
                (String::from("kernel"), VfsFileType::Directory),
                (String::from("net"), VfsFileType::Directory),
            ],
            ProcStaticDirKind::Kernel => alloc::vec![
                (String::from("keys"), VfsFileType::Directory),
                (String::from("pid_max"), VfsFileType::Regular),
                (String::from("sched_autogroup_enabled"), VfsFileType::Regular),
                (String::from("tainted"), VfsFileType::Regular),
            ],
            ProcStaticDirKind::Keys => alloc::vec![
                (String::from("gc_delay"), VfsFileType::Regular),
                (String::from("maxkeys"), VfsFileType::Regular),
                (String::from("maxbytes"), VfsFileType::Regular),
            ],
            ProcStaticDirKind::Net => {
                alloc::vec![(String::from("ipv4"), VfsFileType::Directory)]
            }
            ProcStaticDirKind::NetIpv4 => {
                alloc::vec![(String::from("conf"), VfsFileType::Directory)]
            }
            ProcStaticDirKind::NetIpv4Conf => alloc::vec![
                (String::from("all"), VfsFileType::Directory),
                (String::from("default"), VfsFileType::Directory),
                (String::from("lo"), VfsFileType::Directory),
            ],
            ProcStaticDirKind::NetIpv4ConfAll
            | ProcStaticDirKind::NetIpv4ConfDefault
            | ProcStaticDirKind::NetIpv4ConfLo => {
                alloc::vec![(String::from("tag"), VfsFileType::Regular)]
            }
        }
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        match (self.kind, name) {
            (ProcStaticDirKind::Sys, "kernel") => {
                Some(Arc::new(ProcStaticDirNode::new(ProcStaticDirKind::Kernel)) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::Sys, "net") => {
                Some(Arc::new(ProcStaticDirNode::new(ProcStaticDirKind::Net)) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::Kernel, "keys") => {
                Some(Arc::new(ProcStaticDirNode::new(ProcStaticDirKind::Keys)) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::Kernel, "pid_max") => {
                Some(Arc::new(ProcKernelPidMaxNode::new()) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::Kernel, "sched_autogroup_enabled") => Some(
                Arc::new(ProcKernelSysctlNode::new(ProcKernelSysctlKind::SchedAutogroupEnabled))
                    as Arc<dyn VfsNode>,
            ),
            (ProcStaticDirKind::Kernel, "tainted") => {
                Some(Arc::new(ProcKernelTaintedNode::new()) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::Keys, "gc_delay") => {
                Some(Arc::new(ProcKeySysctlNode::new(ProcKeySysctlKind::GcDelay)) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::Keys, "maxkeys") => {
                Some(Arc::new(ProcKeySysctlNode::new(ProcKeySysctlKind::MaxKeys)) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::Keys, "maxbytes") => {
                Some(Arc::new(ProcKeySysctlNode::new(ProcKeySysctlKind::MaxBytes)) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::Net, "ipv4") => {
                Some(Arc::new(ProcStaticDirNode::new(ProcStaticDirKind::NetIpv4)) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::NetIpv4, "conf") => {
                Some(Arc::new(ProcStaticDirNode::new(ProcStaticDirKind::NetIpv4Conf)) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::NetIpv4Conf, "all") => {
                Some(Arc::new(ProcStaticDirNode::new(ProcStaticDirKind::NetIpv4ConfAll)) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::NetIpv4Conf, "default") => {
                Some(Arc::new(ProcStaticDirNode::new(ProcStaticDirKind::NetIpv4ConfDefault)) as Arc<dyn VfsNode>)
            }
            (ProcStaticDirKind::NetIpv4Conf, "lo") => {
                Some(Arc::new(ProcStaticDirNode::new(ProcStaticDirKind::NetIpv4ConfLo)) as Arc<dyn VfsNode>)
            }
            (
                ProcStaticDirKind::NetIpv4ConfAll
                | ProcStaticDirKind::NetIpv4ConfDefault
                | ProcStaticDirKind::NetIpv4ConfLo,
                "tag",
            ) => {
                let kind = match self.kind {
                    ProcStaticDirKind::NetIpv4ConfDefault => ProcNetIpv4ConfTagKind::Default,
                    _ => ProcNetIpv4ConfTagKind::Loopback,
                };
                Some(Arc::new(ProcNetIpv4ConfTagNode::new(kind)) as Arc<dyn VfsNode>)
            }
            _ => None,
        }
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

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

#[derive(Default, Debug)]
struct ProcKernelPidMaxNode;

impl ProcKernelPidMaxNode {
    fn new() -> Self {
        Self
    }

    fn render(&self) -> String {
        alloc::format!("{}\n", crate::task::PID_MAX)
    }
}

impl VfsNode for ProcKernelPidMaxNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        self.render().len()
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(self.render(), offset, buf)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

#[derive(Default, Debug)]
struct ProcKernelTaintedNode;

impl ProcKernelTaintedNode {
    fn new() -> Self {
        Self
    }
}

impl VfsNode for ProcKernelTaintedNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        2
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(String::from("0\n"), offset, buf)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }
}

#[derive(Debug, Clone, Copy)]
enum ProcKernelSysctlKind {
    SchedAutogroupEnabled,
}

#[derive(Debug)]
struct ProcKernelSysctlNode {
    kind: ProcKernelSysctlKind,
}

impl ProcKernelSysctlNode {
    fn new(kind: ProcKernelSysctlKind) -> Self {
        Self { kind }
    }

    fn render(&self) -> String {
        match self.kind {
            ProcKernelSysctlKind::SchedAutogroupEnabled => {
                alloc::format!("{}\n", crate::sched::autogroup_enabled() as u8)
            }
        }
    }
}

impl VfsNode for ProcKernelSysctlNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        self.render().len()
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

    fn truncate(&self, _new_size: usize) -> Result<(), FS_ERRNO> {
        Ok(())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(self.render(), offset, buf)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> usize {
        let result = match self.kind {
            ProcKernelSysctlKind::SchedAutogroupEnabled => {
                parse_proc_bool(buf).map(crate::sched::set_autogroup_enabled)
            }
        };
        if result.is_ok() {
            buf.len()
        } else {
            0
        }
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

#[derive(Debug)]
struct ProcNetIpv4ConfTagNode {
    kind: ProcNetIpv4ConfTagKind,
}

#[derive(Clone, Copy, Debug)]
enum ProcNetIpv4ConfTagKind {
    Loopback,
    Default,
}

impl ProcNetIpv4ConfTagNode {
    fn new(kind: ProcNetIpv4ConfTagKind) -> Self {
        Self { kind }
    }

    fn render(&self) -> String {
        let tag = match self.kind {
            ProcNetIpv4ConfTagKind::Loopback => current_process().netns_loopback_tag(),
            ProcNetIpv4ConfTagKind::Default => current_process().netns_default_tag(),
        };
        alloc::format!("{}\n", tag)
    }
}

impl VfsNode for ProcNetIpv4ConfTagNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        self.render().len()
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

    fn truncate(&self, _new_size: usize) -> Result<(), FS_ERRNO> {
        Ok(())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(self.render(), offset, buf)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> usize {
        match parse_proc_u32(buf) {
            Ok(tag) => {
                match self.kind {
                    ProcNetIpv4ConfTagKind::Loopback => {
                        current_process().set_netns_loopback_tag(tag);
                    }
                    ProcNetIpv4ConfTagKind::Default => {
                        current_process().set_netns_default_tag(tag);
                    }
                }
                buf.len()
            }
            Err(_) => 0,
        }
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

#[derive(Default, Debug)]
struct ProcKeyUsersNode;

impl ProcKeyUsersNode {
    fn new() -> Self {
        Self
    }
}

impl VfsNode for ProcKeyUsersNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        keys::render_key_users().len()
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(keys::render_key_users(), offset, buf)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

#[derive(Debug, Clone, Copy)]
enum ProcKeySysctlKind {
    GcDelay,
    MaxKeys,
    MaxBytes,
}

#[derive(Debug)]
struct ProcKeySysctlNode {
    kind: ProcKeySysctlKind,
}

impl ProcKeySysctlNode {
    fn new(kind: ProcKeySysctlKind) -> Self {
        Self { kind }
    }

    fn render(&self) -> String {
        match self.kind {
            ProcKeySysctlKind::GcDelay => alloc::format!("{}\n", keys::gc_delay()),
            ProcKeySysctlKind::MaxKeys => alloc::format!("{}\n", keys::max_keys()),
            ProcKeySysctlKind::MaxBytes => alloc::format!("{}\n", keys::max_bytes()),
        }
    }
}

impl VfsNode for ProcKeySysctlNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        self.render().len()
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

    fn truncate(&self, _new_size: usize) -> Result<(), FS_ERRNO> {
        Ok(())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(self.render(), offset, buf)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> usize {
        let result = match self.kind {
            ProcKeySysctlKind::GcDelay => parse_proc_u32(buf).map(keys::set_gc_delay),
            ProcKeySysctlKind::MaxKeys => parse_proc_u32(buf).map(keys::set_max_keys),
            ProcKeySysctlKind::MaxBytes => parse_proc_usize(buf).map(keys::set_max_bytes),
        };
        if result.is_ok() {
            buf.len()
        } else {
            0
        }
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/cpuinfo` node.
#[derive(Default, Debug)]
pub struct ProcCpuinfoNode;

impl ProcCpuinfoNode {
    /// Create a new cpuinfo node.
    pub fn new() -> Self {
        Self
    }
}

impl VfsNode for ProcCpuinfoNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        build_cpuinfo().len()
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(build_cpuinfo(), offset, buf)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/filesystems` node.
#[derive(Default, Debug)]
pub struct ProcFilesystemsNode;

impl ProcFilesystemsNode {
    /// Create a new filesystems node.
    pub fn new() -> Self {
        Self
    }
}

impl VfsNode for ProcFilesystemsNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        build_filesystems().len()
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(build_filesystems(), offset, buf)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/meminfo` node.
#[derive(Default, Debug)]
pub struct ProcMeminfoNode;

impl ProcMeminfoNode {
    /// Create a new meminfo node.
    pub fn new() -> Self {
        Self
    }
}

impl VfsNode for ProcMeminfoNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        build_meminfo().len()
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(build_meminfo(), offset, buf)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/mounts` node.
#[derive(Default, Debug)]
pub struct ProcMountsNode;

impl ProcMountsNode {
    /// Create a new mounts node.
    pub fn new() -> Self {
        Self
    }
}

impl VfsNode for ProcMountsNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        build_mounts().len()
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(build_mounts(), offset, buf)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/partitions` node.
#[derive(Default, Debug)]
pub struct ProcPartitionsNode;

impl ProcPartitionsNode {
    /// Create a new partitions node.
    pub fn new() -> Self {
        Self
    }
}

impl VfsNode for ProcPartitionsNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        build_partitions().len()
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(build_partitions(), offset, buf)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/io_perf` node.
#[derive(Default, Debug)]
#[cfg(feature = "io_perf_counters")]
pub struct ProcIoPerfNode;

#[cfg(feature = "io_perf_counters")]
impl ProcIoPerfNode {
    /// Create a new `/proc/io_perf` node.
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "io_perf_counters")]
impl VfsNode for ProcIoPerfNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        build_io_perf().len()
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

    fn clear(&self) {
        reset_io_perf();
    }

    fn truncate(&self, _new_size: usize) -> Result<(), FS_ERRNO> {
        reset_io_perf();
        Ok(())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(build_io_perf(), offset, buf)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> usize {
        reset_io_perf();
        buf.len()
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/net_perf` node.
#[derive(Default, Debug)]
#[cfg(feature = "net_perf_counters")]
pub struct ProcNetPerfNode;

#[cfg(feature = "net_perf_counters")]
impl ProcNetPerfNode {
    /// Create a new `/proc/net_perf` node.
    pub fn new() -> Self {
        Self
    }
}

#[cfg(feature = "net_perf_counters")]
impl VfsNode for ProcNetPerfNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        net::render_perf_counters().len()
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

    fn clear(&self) {
        net::reset_perf_counters();
    }

    fn truncate(&self, _new_size: usize) -> Result<(), FS_ERRNO> {
        net::reset_perf_counters();
        Ok(())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(net::render_perf_counters(), offset, buf)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> usize {
        net::reset_perf_counters();
        buf.len()
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/self` symlink node.
#[derive(Default, Debug)]
pub struct ProcSelfLinkNode;

impl ProcSelfLinkNode {
    /// Create a new self symlink node.
    pub fn new() -> Self {
        Self
    }

    fn link_target(&self) -> String {
        let pid = current_process().getpid();
        alloc::format!("/proc/{}", pid)
    }
}

impl VfsNode for ProcSelfLinkNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Symlink
    }

    fn size(&self) -> usize {
        self.link_target().len()
    }

    fn read_link(&self) -> Result<String, FS_ERRNO> {
        Ok(self.link_target())
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

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/<pid>` directory node.
#[derive(Debug)]
pub struct ProcPidDirNode {
    pid: usize,
}

impl ProcPidDirNode {
    /// Create a new `/proc/<pid>` node.
    pub fn new(pid: usize) -> Self {
        Self { pid }
    }
}

impl VfsNode for ProcPidDirNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        if pid2process(self.pid).is_none() {
            return Vec::new();
        }
        alloc::vec![
            (String::from("exe"), VfsFileType::Symlink),
            (String::from("maps"), VfsFileType::Regular),
            (String::from("mounts"), VfsFileType::Regular),
            (String::from("ns"), VfsFileType::Directory),
            (String::from("stat"), VfsFileType::Regular),
            (String::from("status"), VfsFileType::Regular),
            (String::from("timens_offsets"), VfsFileType::Regular),
            (String::from("setgroups"), VfsFileType::Regular),
            (String::from("uid_map"), VfsFileType::Regular),
            (String::from("gid_map"), VfsFileType::Regular),
            (String::from("oom_score_adj"), VfsFileType::Regular),
        ]
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        pid2process(self.pid)?;
        match name {
            "exe" => Some(Arc::new(ProcPidExeLinkNode::new(self.pid)) as Arc<dyn VfsNode>),
            "maps" => Some(Arc::new(ProcPidMapsNode::new(self.pid)) as Arc<dyn VfsNode>),
            "mounts" => Some(Arc::new(ProcMountsNode::new()) as Arc<dyn VfsNode>),
            "ns" => Some(Arc::new(ProcPidNsDirNode::new(self.pid)) as Arc<dyn VfsNode>),
            "stat" => Some(Arc::new(ProcPidStatNode::new(self.pid)) as Arc<dyn VfsNode>),
            "status" => Some(Arc::new(ProcPidStatusNode::new(self.pid)) as Arc<dyn VfsNode>),
            "timens_offsets" => Some(Arc::new(ProcPidTimensOffsetsNode::new(self.pid)) as Arc<dyn VfsNode>),
            "setgroups" => {
                Some(Arc::new(ProcPidUsernsNode::new(self.pid, ProcPidUsernsKind::Setgroups)) as Arc<dyn VfsNode>)
            }
            "uid_map" => Some(Arc::new(ProcPidUsernsNode::new(self.pid, ProcPidUsernsKind::UidMap)) as Arc<dyn VfsNode>),
            "gid_map" => Some(Arc::new(ProcPidUsernsNode::new(self.pid, ProcPidUsernsKind::GidMap)) as Arc<dyn VfsNode>),
            "oom_score_adj" => Some(Arc::new(ProcPidOomScoreAdjNode::new(self.pid)) as Arc<dyn VfsNode>),
            _ => None,
        }
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

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

#[derive(Debug)]
struct ProcPidTimensOffsetsNode {
    pid: usize,
}

impl ProcPidTimensOffsetsNode {
    fn new(pid: usize) -> Self {
        Self { pid }
    }

    fn content(&self) -> String {
        let (monotonic_ns, boottime_ns) = pid2process(self.pid)
            .map(|process| {
                (
                    process.child_timens_monotonic_offset_ns(),
                    process.child_timens_boottime_offset_ns(),
                )
            })
            .unwrap_or((0, 0));
        let monotonic_sec = monotonic_ns.div_euclid(1_000_000_000);
        let monotonic_nsec = monotonic_ns.rem_euclid(1_000_000_000);
        let boottime_sec = boottime_ns.div_euclid(1_000_000_000);
        let boottime_nsec = boottime_ns.rem_euclid(1_000_000_000);
        alloc::format!(
            "monotonic {} {}\nboottime {} {}\n",
            monotonic_sec,
            monotonic_nsec,
            boottime_sec,
            boottime_nsec
        )
    }
}

impl VfsNode for ProcPidTimensOffsetsNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        self.content().len()
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

    fn truncate(&self, _new_size: usize) -> Result<(), FS_ERRNO> {
        Ok(())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        read_string_at(self.content(), offset, buf)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> usize {
        let Ok(text) = core::str::from_utf8(buf) else {
            return 0;
        };
        let mut parts = text.split_whitespace();
        let Some(clock) = parts.next().and_then(|part| part.parse::<i32>().ok()) else {
            return 0;
        };
        let Some(sec) = parts.next().and_then(|part| part.parse::<i128>().ok()) else {
            return 0;
        };
        let Some(nsec) = parts.next().and_then(|part| part.parse::<i128>().ok()) else {
            return 0;
        };
        if !(0..1_000_000_000).contains(&nsec) {
            return 0;
        }
        let Some(process) = pid2process(self.pid) else {
            return 0;
        };
        let offset_ns = sec.saturating_mul(1_000_000_000).saturating_add(nsec);
        match clock {
            1 => process.set_child_timens_monotonic_offset_ns(offset_ns),
            7 => process.set_child_timens_boottime_offset_ns(offset_ns),
            _ => return 0,
        }
        buf.len()
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/<pid>/oom_score_adj` node.
#[derive(Debug)]
struct ProcPidOomScoreAdjNode {
    pid: usize,
}

impl ProcPidOomScoreAdjNode {
    fn new(pid: usize) -> Self {
        Self { pid }
    }

    fn content(&self) -> Result<String, FS_ERRNO> {
        let process = pid2process(self.pid).ok_or(FS_ERRNO::ENOENT)?;
        Ok(alloc::format!("{}\n", process.oom_score_adj()))
    }
}

impl VfsNode for ProcPidOomScoreAdjNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        self.content().map(|data| data.len()).unwrap_or(0)
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

    fn truncate(&self, _new_size: usize) -> Result<(), FS_ERRNO> {
        Ok(())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.content()
            .map(|data| read_string_at(data, offset, buf))
            .unwrap_or(0)
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> usize {
        self.write_at_result(offset, buf).unwrap_or(0)
    }

    fn write_at_result(&self, _offset: usize, buf: &[u8]) -> Result<usize, FS_ERRNO> {
        let value = parse_proc_i32(buf)?;
        if !(-1000..=1000).contains(&value) {
            return Err(FS_ERRNO::EINVAL);
        }
        let process = pid2process(self.pid).ok_or(FS_ERRNO::ENOENT)?;
        process.set_oom_score_adj(value);
        Ok(buf.len())
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

#[derive(Debug, Clone, Copy)]
enum ProcPidUsernsKind {
    Setgroups,
    UidMap,
    GidMap,
}

/// User namespace setup files used by LTP helpers.
#[derive(Debug)]
struct ProcPidUsernsNode {
    pid: usize,
    kind: ProcPidUsernsKind,
}

impl ProcPidUsernsNode {
    fn new(pid: usize, kind: ProcPidUsernsKind) -> Self {
        Self { pid, kind }
    }

    fn content(&self) -> Result<&'static str, FS_ERRNO> {
        pid2process(self.pid).ok_or(FS_ERRNO::ENOENT)?;
        Ok(match self.kind {
            ProcPidUsernsKind::Setgroups => "allow\n",
            ProcPidUsernsKind::UidMap | ProcPidUsernsKind::GidMap => "0 0 4294967295\n",
        })
    }
}

impl VfsNode for ProcPidUsernsNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        self.content().map(|data| data.len()).unwrap_or(0)
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

    fn truncate(&self, _new_size: usize) -> Result<(), FS_ERRNO> {
        Ok(())
    }

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.content()
            .map(|data| read_string_at(data.to_string(), offset, buf))
            .unwrap_or(0)
    }

    fn write_at(&self, _offset: usize, buf: &[u8]) -> usize {
        if pid2process(self.pid).is_none() {
            return 0;
        }
        buf.len()
    }

    fn write_at_result(&self, offset: usize, buf: &[u8]) -> Result<usize, FS_ERRNO> {
        Ok(self.write_at(offset, buf))
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/<pid>/ns` directory node.
#[derive(Debug)]
pub struct ProcPidNsDirNode {
    pid: usize,
}

impl ProcPidNsDirNode {
    /// Create a new `/proc/<pid>/ns` directory node.
    pub fn new(pid: usize) -> Self {
        Self { pid }
    }
}

impl VfsNode for ProcPidNsDirNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Directory
    }

    fn ls(&self) -> Vec<(String, VfsFileType)> {
        if pid2process(self.pid).is_none() {
            return Vec::new();
        }
        alloc::vec![
            (String::from("mnt"), VfsFileType::Regular),
            (String::from("net"), VfsFileType::Regular),
            (String::from("time_for_children"), VfsFileType::Regular),
        ]
    }

    fn find(&self, name: &str) -> Option<Arc<dyn VfsNode>> {
        pid2process(self.pid)?;
        match name {
            "mnt" => Some(Arc::new(ProcPidNsEntryNode::new(self.pid, "mnt")) as Arc<dyn VfsNode>),
            "net" => Some(Arc::new(ProcPidNsEntryNode::new(self.pid, "net")) as Arc<dyn VfsNode>),
            "time_for_children" => {
                Some(Arc::new(ProcPidNsEntryNode::new(self.pid, "time")) as Arc<dyn VfsNode>)
            }
            _ => None,
        }
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

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/<pid>/ns/<kind>` placeholder node.
#[derive(Debug)]
pub struct ProcPidNsEntryNode {
    pid: usize,
    kind: &'static str,
}

impl ProcPidNsEntryNode {
    /// Create a new `/proc/<pid>/ns/<kind>` placeholder node.
    pub fn new(pid: usize, kind: &'static str) -> Self {
        Self { pid, kind }
    }

    fn content(&self) -> Result<String, FS_ERRNO> {
        pid2process(self.pid).ok_or(FS_ERRNO::ENOENT)?;
        Ok(alloc::format!("{}:[{}]\n", self.kind, self.pid))
    }
}

impl VfsNode for ProcPidNsEntryNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        self.content().map(|data| data.len()).unwrap_or(0)
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        self.content()
            .map(|data| read_string_at(data, offset, buf))
            .unwrap_or(0)
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/<pid>/exe` symlink node.
#[derive(Debug)]
pub struct ProcPidExeLinkNode {
    pid: usize,
}

impl ProcPidExeLinkNode {
    /// Create a new `/proc/<pid>/exe` symlink node.
    pub fn new(pid: usize) -> Self {
        Self { pid }
    }

    fn link_target(&self) -> Result<String, FS_ERRNO> {
        let process = pid2process(self.pid).ok_or(FS_ERRNO::ENOENT)?;
        let path = process.exec_path();
        if path.is_empty() {
            return Err(FS_ERRNO::ENOENT);
        }
        Ok(path)
    }
}

impl VfsNode for ProcPidExeLinkNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Symlink
    }

    fn size(&self) -> usize {
        self.link_target().map(|path| path.len()).unwrap_or(0)
    }

    fn read_link(&self) -> Result<String, FS_ERRNO> {
        self.link_target()
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

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/<pid>/maps` node.
#[derive(Debug)]
pub struct ProcPidMapsNode {
    pid: usize,
}

impl ProcPidMapsNode {
    /// Create a new `/proc/<pid>/maps` node.
    pub fn new(pid: usize) -> Self {
        Self { pid }
    }
}

impl VfsNode for ProcPidMapsNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        build_pid_maps(self.pid).map(|data| data.len()).unwrap_or(0)
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        match build_pid_maps(self.pid) {
            Ok(data) => read_string_at(data, offset, buf),
            Err(_) => 0,
        }
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/<pid>/stat` node.
#[derive(Debug)]
pub struct ProcPidStatNode {
    pid: usize,
}

impl ProcPidStatNode {
    /// Create a new `/proc/<pid>/stat` node.
    pub fn new(pid: usize) -> Self {
        Self { pid }
    }
}

impl VfsNode for ProcPidStatNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        build_pid_stat(self.pid).map(|data| data.len()).unwrap_or(0)
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        match build_pid_stat(self.pid) {
            Ok(data) => read_string_at(data, offset, buf),
            Err(_) => 0,
        }
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}

/// `/proc/<pid>/status` node.
#[derive(Debug)]
pub struct ProcPidStatusNode {
    pid: usize,
}

impl ProcPidStatusNode {
    /// Create a new `/proc/<pid>/status` node.
    pub fn new(pid: usize) -> Self {
        Self { pid }
    }
}

impl VfsNode for ProcPidStatusNode {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn file_type(&self) -> VfsFileType {
        VfsFileType::Regular
    }

    fn size(&self) -> usize {
        build_pid_status(self.pid).map(|data| data.len()).unwrap_or(0)
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

    fn read_at(&self, offset: usize, buf: &mut [u8]) -> usize {
        match build_pid_status(self.pid) {
            Ok(data) => read_string_at(data, offset, buf),
            Err(_) => 0,
        }
    }

    fn write_at(&self, _offset: usize, _buf: &[u8]) -> usize {
        0
    }

    fn statfs(&self) -> Result<fs::VfsStatFs, fs::errno::FS_ERRNO> {
        Ok(crate::fs::empty_statfs(
            fs::STATFS_MAGIC_PROC,
            crate::config::PAGE_SIZE as u64,
            0x9fa0,
            255,
        ))
    }
}
