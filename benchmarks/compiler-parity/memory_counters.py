"""Windows lifetime counters; individual peaks are never a simultaneous tree peak."""
import ctypes
import os
from ctypes import wintypes

class ProcessMemoryCountersEx(ctypes.Structure):
    _fields_ = [('cb', wintypes.DWORD), ('PageFaultCount', wintypes.DWORD)] + [
        (name, ctypes.c_size_t) for name in (
            'PeakWorkingSetSize', 'WorkingSetSize', 'QuotaPeakPagedPoolUsage',
            'QuotaPagedPoolUsage', 'QuotaPeakNonPagedPoolUsage', 'QuotaNonPagedPoolUsage',
            'PagefileUsage', 'PeakPagefileUsage', 'PrivateUsage')]

class WindowsMemory:
    def __init__(self):
        self.handles = {}
        self.records = {}
        self.errors = []
        self.enabled = os.name == 'nt'
        if self.enabled:
            self.kernel = ctypes.WinDLL('kernel32', use_last_error=True)
            self.psapi = ctypes.WinDLL('psapi', use_last_error=True)
            self.kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
            self.kernel.OpenProcess.restype = wintypes.HANDLE
            self.kernel.CloseHandle.argtypes = [wintypes.HANDLE]
            self.kernel.CloseHandle.restype = wintypes.BOOL
            self.kernel.DuplicateHandle.argtypes = [wintypes.HANDLE, wintypes.HANDLE, wintypes.HANDLE, ctypes.POINTER(wintypes.HANDLE), wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
            self.kernel.DuplicateHandle.restype = wintypes.BOOL
            self.kernel.GetCurrentProcess.restype = wintypes.HANDLE
            self.psapi.GetProcessMemoryInfo.argtypes = [wintypes.HANDLE, ctypes.POINTER(ProcessMemoryCountersEx), wintypes.DWORD]
            self.psapi.GetProcessMemoryInfo.restype = wintypes.BOOL

    def error(self, operation, pid, error):
        message = str(error)
        for item in self.errors:
            if (item['operation'], item['pid'], item['error']) == (operation, pid, message):
                item['count'] += 1
                return
        self.errors.append({'operation': operation, 'pid': pid, 'error': message, 'count': 1})

    def attach(self, key, child=None):
        if not self.enabled or key in self.records:
            return
        pid, created = key
        self.records[key] = {'pid': pid, 'create_time': created, 'is_root': child is not None}
        # Duplicate Popen's already-retained handle: the process identity and OS peak counters remain queryable after exit.
        # Descendants can only be retained once discovered.
        if child is not None:
            handle = wintypes.HANDLE()
            current = self.kernel.GetCurrentProcess()
            ok = self.kernel.DuplicateHandle(current, int(child._handle), current, ctypes.byref(handle), 0, False, 2)
            handle = handle.value if ok else None
        else:
            handle = self.kernel.OpenProcess(0x0400 | 0x0010, False, pid)
        if not handle:
            self.error('retain_handle', pid, ctypes.WinError(ctypes.get_last_error()))
        else:
            self.handles[key] = handle

    def sample(self):
        if not self.enabled:
            return
        for key, handle in self.handles.items():
            counters = ProcessMemoryCountersEx()
            counters.cb = ctypes.sizeof(counters)
            if not self.psapi.GetProcessMemoryInfo(handle, ctypes.byref(counters), counters.cb):
                self.error('GetProcessMemoryInfo', key[0], ctypes.WinError(ctypes.get_last_error()))
                continue
            record = self.records[key]
            for field, value in (
                ('os_peak_working_set_bytes', counters.PeakWorkingSetSize),
                ('os_peak_private_commit_bytes', counters.PeakPagefileUsage),
                ('sampled_peak_private_commit_bytes', counters.PrivateUsage)):
                record[field] = max(record.get(field, 0), int(value))
            record['last_working_set_bytes'] = int(counters.WorkingSetSize)
            record['last_private_commit_bytes'] = int(counters.PrivateUsage)

    def finish(self):
        self.sample()
        for key, handle in self.handles.items():
            if not self.kernel.CloseHandle(handle):
                self.error('CloseHandle', key[0], ctypes.WinError(ctypes.get_last_error()))
        self.handles.clear()
        return {'available': self.enabled, 'processes': list(self.records.values()), 'errors': self.errors,
                'semantics': 'OS peak working set includes resident shared pages; peak private commit is committed private memory, not RSS or physical RAM. Per-process lifetime peaks are not a simultaneous process-tree peak. Retained handles permit a final OS peak query after exit; failed queries are reported. Descendants exiting before discovery remain unobserved.'}
