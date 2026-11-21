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
    pub(super) fn cgroup_room(groups: &str, read: impl Fn(&str) -> Option<String>) -> Option<u64> {
        let number = |path: String| read(&path)?.trim().parse::<u64>().ok();
        for line in groups.lines() {
            let mut fields = line.splitn(3, ':');
            let (Some(id), Some(controllers), Some(path)) =
                (fields.next(), fields.next(), fields.next())
            else {
                continue;
            };
            let path = path.trim_end_matches('/');
            if id == "0" && controllers.is_empty() {
                // cgroup v2: "max" when unlimited, which fails to parse.
                let base = format!("/sys/fs/cgroup{path}");
                let limit = number(format!("{base}/memory.max"))?;
                let used = number(format!("{base}/memory.current")).unwrap_or(0);
                return Some(limit.saturating_sub(used));
            }
            if controllers.split(',').any(|c| c == "memory") {
                // cgroup v1: an unlimited group reports a limit near 2^63.
                let base = format!("/sys/fs/cgroup/memory{path}");
                let limit = number(format!("{base}/memory.limit_in_bytes"))?;
                if limit >= 1 << 60 {
                    return None;
                }
                let used = number(format!("{base}/memory.usage_in_bytes")).unwrap_or(0);
                return Some(limit.saturating_sub(used));
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
    }

    #[test]
    fn this_machine_reports_memory() {
        assert!(super::available_bytes().is_some_and(|bytes| bytes > 0));
    }
}
