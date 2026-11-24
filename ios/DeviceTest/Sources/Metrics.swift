import Darwin
import Foundation
import os
import UIKit

/// What the kernel says about this process's memory at one moment.
struct MemorySample {
    /// `phys_footprint`: what iOS's memory limit (jetsam) counts.
    var footprint: UInt64
    /// The footprint's peak over the process's whole life.
    var lifetimePeakFootprint: UInt64
    /// Peak resident size over the process's life, as the simulator table in
    /// docs/performance.md reports.
    var residentPeak: UInt64
    /// `os_proc_available_memory()`: how far the footprint is from the limit.
    var available: UInt64
}

enum Metrics {
    static func memory() -> MemorySample {
        var vm = task_vm_info_data_t()
        var vmCount = mach_msg_type_number_t(MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<natural_t>.size)
        let vmStatus = withUnsafeMutablePointer(to: &vm) { pointer in
            pointer.withMemoryRebound(to: integer_t.self, capacity: Int(vmCount)) {
                task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &vmCount)
            }
        }
        var basic = mach_task_basic_info()
        var basicCount = mach_msg_type_number_t(MemoryLayout<mach_task_basic_info>.size / MemoryLayout<natural_t>.size)
        let basicStatus = withUnsafeMutablePointer(to: &basic) { pointer in
            pointer.withMemoryRebound(to: integer_t.self, capacity: Int(basicCount)) {
                task_info(mach_task_self_, task_flavor_t(MACH_TASK_BASIC_INFO), $0, &basicCount)
            }
        }
        let vmOK = vmStatus == KERN_SUCCESS
        return MemorySample(
            footprint: vmOK ? vm.phys_footprint : 0,
            lifetimePeakFootprint: vmOK ? UInt64(max(0, vm.ledger_phys_footprint_peak)) : 0,
            residentPeak: basicStatus == KERN_SUCCESS ? UInt64(basic.resident_size_max) : 0,
            available: UInt64(os_proc_available_memory())
        )
    }

    /// User plus system CPU time of the whole process, in seconds.
    static func cpuSeconds() -> Double {
        var usage = rusage()
        getrusage(RUSAGE_SELF, &usage)
        func seconds(_ t: timeval) -> Double { Double(t.tv_sec) + Double(t.tv_usec) / 1_000_000 }
        return seconds(usage.ru_utime) + seconds(usage.ru_stime)
    }

    /// The hardware model, such as `iPhone17,1`.
    static var deviceModel: String {
        var info = utsname()
        uname(&info)
        return withUnsafeBytes(of: &info.machine) { raw in
            String(cString: raw.bindMemory(to: CChar.self).baseAddress!)
        }
    }

    static var thermalState: String {
        switch ProcessInfo.processInfo.thermalState {
        case .nominal: return "nominal"
        case .fair: return "fair"
        case .serious: return "serious"
        case .critical: return "critical"
        @unknown default: return "unknown"
        }
    }

    static var device: [String: Any] {
        [
            "model": deviceModel,
            "system": "\(UIDevice.current.systemName) \(UIDevice.current.systemVersion)",
            "cores": ProcessInfo.processInfo.activeProcessorCount,
            "physicalMemoryMiB": mib(ProcessInfo.processInfo.physicalMemory),
            "lowPowerMode": ProcessInfo.processInfo.isLowPowerModeEnabled,
        ]
    }
}

func mib(_ bytes: UInt64) -> Double { (Double(bytes) / 1_048_576 * 10).rounded() / 10 }

func formatMiB(_ bytes: UInt64) -> String { String(format: "%.1f MiB", Double(bytes) / 1_048_576) }
