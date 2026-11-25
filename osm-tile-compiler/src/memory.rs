//! How much memory the machine can still give this process, for sizing
//! buffers that would otherwise use a fixed, conservative size.

/// Bytes this process can still allocate without the system swapping or
/// reclaiming, as far as the platform says; `None` where it can't tell.
///
/// Linux: `MemAvailable`, lowered to the room left under a cgroup memory
/// limit. macOS: half the installed memory, since the kernel has no cheap
/// equivalent of `MemAvailable`. Windows: the available physical memory.
/// Phones say `None`: an app's share of memory there is far below what the
/// system reports, and going over it gets the app killed.
pub(crate) fn available_bytes() -> Option<u64> {
    platform::available_bytes().filter(|&bytes| bytes > 0)
}

#[cfg(target_os = "linux")]
mod platform {
    use std::fs;

    pub(super) fn available_bytes() -> Option<u64> {
        let meminfo = fs::read_to_string("/proc/meminfo").ok();
        let system = meminfo.as_deref().and_then(mem_available);
        let cgroup = fs::read_to_string("/proc/self/cgroup")
            .ok()
            .and_then(|groups| cgroup_room(&groups, |path| fs::read_to_string(path).ok()));
        match (system, cgroup) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// `MemAvailable` from `/proc/meminfo`, in bytes.
    pub(super) fn mem_available(meminfo: &str) -> Option<u64> {
        let line = meminfo
            .lines()
            .find_map(|line| line.strip_prefix("MemAvailable:"))?;
        let kib = line.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()?;
        kib.checked_mul(1024)
    }

    /// The room left under this process's cgroup memory limit, from its
    /// `/proc/self/cgroup` lines; `None` when no limit applies. `read` reads
    /// a control file.
    ///
    /// The group's usage counts the page cache charged to it: every file the
    /// group read or wrote, which the kernel drops as soon as the group needs
    /// the room. Taken as used, it made a group that had just written a few
    /// archives look full, and the sort buffers stayed at their floor. So the
    /// file-backed pages (`active_file` and `inactive_file` in `memory.stat`)
    /// are left out of the usage, as `MemAvailable` leaves them out of the
    /// machine's.
    pub(super) fn cgroup_room(groups: &str, read: impl Fn(&str) -> Option<String>) -> Option<u64> {
        let number = |path: &str| read(path)?.trim().parse::<u64>().ok();
        // The bytes of the `memory.stat` keys `keys`, summed; 0 without the file.
        let stat = |path: &str, keys: [&str; 2]| {
            read(path).map_or(0, |stat| {
                stat.lines()
                    .filter_map(|line| line.split_once(' '))
                    .filter(|(key, _)| keys.contains(key))
                    .filter_map(|(_, value)| value.trim().parse::<u64>().ok())
                    .fold(0u64, u64::saturating_add)
            })
        };
        for line in groups.lines() {
            let mut fields = line.splitn(3, ':');
            let (Some(id), Some(controllers), Some(path)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let path = path.trim_end_matches('/');
            if id == "0" && controllers.is_empty() {
                // cgroup v2: "max" when unlimited, which fails to parse. On a
                // hybrid host the v2 group has no memory controller (no
                // `memory.max`), and a v1 line may follow.
                let base = format!("/sys/fs/cgroup{path}");
                let Some(limit) = read(&format!("{base}/memory.max")) else {
                    continue;
                };
                let limit = limit.trim().parse::<u64>().ok()?;
                let used = number(&format!("{base}/memory.current")).unwrap_or(0);
                let cache = stat(
                    &format!("{base}/memory.stat"),
                    ["active_file", "inactive_file"],
                );
                return Some(limit.saturating_sub(used.saturating_sub(cache)));
            }
            if controllers.split(',').any(|c| c == "memory") {
                // cgroup v1: an unlimited group reports a limit near 2^63.
                let base = format!("/sys/fs/cgroup/memory{path}");
                let limit = number(&format!("{base}/memory.limit_in_bytes"))?;
                if limit >= 1 << 60 {
                    return None;
                }
                let used = number(&format!("{base}/memory.usage_in_bytes")).unwrap_or(0);
                // The `total_` keys include the group's descendants, as the
                // usage does.
                let cache = stat(
                    &format!("{base}/memory.stat"),
                    ["total_active_file", "total_inactive_file"],
                );
                return Some(limit.saturating_sub(used.saturating_sub(cache)));
            }
        }
        None
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::{c_char, c_int, c_void};

    unsafe extern "C" {
        fn sysctlbyname(
            name: *const c_char,
            old: *mut c_void,
            old_len: *mut usize,
            new: *mut c_void,
            new_len: usize,
        ) -> c_int;
    }

    pub(super) fn available_bytes() -> Option<u64> {
        let mut total = 0u64;
        let mut len = size_of::<u64>();
        // SAFETY: `hw.memsize` is a 64-bit integer; `total` and `len` describe
        // a buffer of that size, and no new value is set.
        let status = unsafe {
            sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&raw mut total).cast(),
                &raw mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        (status == 0 && len == size_of::<u64>()).then_some(total / 2)
    }
}

#[cfg(windows)]
mod platform {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }

    pub(super) fn available_bytes() -> Option<u64> {
        let mut status = MemoryStatusEx {
            length: size_of::<MemoryStatusEx>() as u32,
            memory_load: 0,
            total_phys: 0,
            avail_phys: 0,
            total_page_file: 0,
            avail_page_file: 0,
            total_virtual: 0,
            avail_virtual: 0,
            avail_extended_virtual: 0,
        };
        // SAFETY: `status` is a MEMORYSTATUSEX with its length set, as the
        // call requires.
        let ok = unsafe { GlobalMemoryStatusEx(&raw mut status) };
        (ok != 0).then_some(status.avail_phys)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    /// Android, iOS and other systems: unknown, so callers keep their fixed
    /// sizes.
    pub(super) fn available_bytes() -> Option<u64> {
        None
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::platform::{cgroup_room, mem_available};
    use std::collections::HashMap;

    #[test]
    fn reads_mem_available() {
        let meminfo =
            "MemTotal:       16000000 kB\nMemFree:  100 kB\nMemAvailable:   12000000 kB\n";
        assert_eq!(mem_available(meminfo), Some(12_000_000 * 1024));
        assert_eq!(mem_available("MemTotal: 1 kB\n"), None);
    }

    #[test]
    fn reads_cgroup_limits() {
        let files = HashMap::from([
            ("/sys/fs/cgroup/job/memory.max", "1000\n"),
            ("/sys/fs/cgroup/job/memory.current", "400\n"),
            ("/sys/fs/cgroup/free/memory.max", "max\n"),
            ("/sys/fs/cgroup/memory/v1/memory.limit_in_bytes", "2000\n"),
            ("/sys/fs/cgroup/memory/v1/memory.usage_in_bytes", "500\n"),
            (
                "/sys/fs/cgroup/memory/v1free/memory.limit_in_bytes",
                "9223372036854771712\n",
            ),
        ]);
        let read = |path: &str| files.get(path).map(|s| s.to_string());
        assert_eq!(cgroup_room("0::/job\n", read), Some(600));
        assert_eq!(cgroup_room("0::/free\n", read), None);
        assert_eq!(cgroup_room("4:memory:/v1\n1:cpu:/x\n", read), Some(1500));
        assert_eq!(cgroup_room("4:cpu,memory:/v1free\n", read), None);
        assert_eq!(cgroup_room("", read), None);
        // Hybrid: the v2 line has no memory controller, the v1 line does.
        assert_eq!(cgroup_room("0::/\n4:memory:/v1\n", read), Some(1500));
    }

    #[test]
    fn leaves_page_cache_out_of_cgroup_usage() {
        // A group at its limit, most of it files it read or wrote.
        let files = HashMap::from([
            ("/sys/fs/cgroup/job/memory.max", "1000\n"),
            ("/sys/fs/cgroup/job/memory.current", "1000\n"),
            (
                "/sys/fs/cgroup/job/memory.stat",
                "anon 300\nfile 700\nactive_anon 300\ninactive_file 500\nactive_file 150\n",
            ),
            ("/sys/fs/cgroup/memory/v1/memory.limit_in_bytes", "2000\n"),
            ("/sys/fs/cgroup/memory/v1/memory.usage_in_bytes", "2000\n"),
            (
                "/sys/fs/cgroup/memory/v1/memory.stat",
                "cache 1700\nrss 300\ninactive_file 10\ntotal_cache 1700\n\
                 total_rss 300\ntotal_inactive_file 1200\ntotal_active_file 400\n",
            ),
            ("/sys/fs/cgroup/odd/memory.max", "100\n"),
            ("/sys/fs/cgroup/odd/memory.current", "50\n"),
            ("/sys/fs/cgroup/odd/memory.stat", "inactive_file 80\n"),
        ]);
        let read = |path: &str| files.get(path).map(|s| s.to_string());
        assert_eq!(cgroup_room("0::/job\n", read), Some(650));
        assert_eq!(cgroup_room("4:memory:/v1\n", read), Some(1600));
        // More cache than usage (counters read at different moments): the
        // whole limit, not more.
        assert_eq!(cgroup_room("0::/odd\n", read), Some(100));
    }

    #[test]
    fn this_machine_reports_memory() {
        assert!(super::available_bytes().is_some_and(|bytes| bytes > 0));
    }
}
