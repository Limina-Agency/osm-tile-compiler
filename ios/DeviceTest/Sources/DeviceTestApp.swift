import SwiftUI
import UIKit

@main
struct DeviceTestApp: App {
    @StateObject private var model = DeviceTestModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environmentObject(model)
        }
    }
}

@MainActor
final class DeviceTestModel: ObservableObject {
    @Published var regions: [Region] = []
    @Published var selectedID: String?
    @Published var threads = min(4, ProcessInfo.processInfo.activeProcessorCount)
    @Published var buildingLevels = false
    @Published var useSources = false
    @Published var stylePack = true

    @Published var isRunning = false
    @Published var status = ""
    @Published var progress: JobProgress?
    @Published var report: RunReport?
    @Published var showMap = false
    @Published var renderSeconds: Double?
    @Published var renderError: String?
    @Published var unfinished: [String: Any]?
    @Published var memoryNow = Metrics.memory()

    private let runner = Runner()

    init() {
        unfinished = Runner.unfinishedRun()
        refresh()
    }

    var selected: Region? { regions.first { $0.id == selectedID } }
    var hasSources: Bool { Storage.hasSources }

    func refresh() {
        regions = Storage.regions()
        if selected == nil { selectedID = regions.first?.id }
        memoryNow = Metrics.memory()
    }

    func dismissUnfinished() {
        if let unfinished {
            var entry = unfinished
            entry["event"] = "killed"
            Storage.appendToLog(entry)
        }
        Runner.clearUnfinishedRun()
        unfinished = nil
    }

    func run() {
        guard let region = selected, !isRunning else { return }
        if unfinished != nil { dismissUnfinished() }
        // Measure without a map on screen: MapLibre's memory is not the compile's.
        showMap = false
        report = nil
        renderSeconds = nil
        renderError = nil
        isRunning = true
        status = "Starting"
        progress = nil
        UIApplication.shared.isIdleTimerDisabled = true
        let options = RunOptions(threads: threads, buildingLevels: buildingLevels,
                                 useSources: useSources && hasSources, stylePack: stylePack)
        let runner = self.runner
        let thread = Thread {
            let report = runner.run(region: region, options: options) { message, progress in
                DispatchQueue.main.async {
                    self.status = message
                    self.progress = progress
                }
            }
            DispatchQueue.main.async {
                self.finish(report)
            }
        }
        thread.name = "device-test-run"
        thread.stackSize = 8 << 20
        thread.start()
    }

    func cancel() { runner.cancel() }

    private func finish(_ report: RunReport) {
        UIApplication.shared.isIdleTimerDisabled = false
        isRunning = false
        self.report = report
        status = report.error ?? "Done"
        memoryNow = Metrics.memory()
        if report.styleURL != nil { showMap = true }
    }

    func rendered(_ seconds: Double) {
        renderSeconds = seconds
        if let report {
            Storage.appendToLog(["event": "render", "id": report.id, "firstFullRenderSeconds": round3(seconds)])
        }
    }

    func renderFailed(_ message: String) {
        renderError = message
        if let report {
            Storage.appendToLog(["event": "render", "id": report.id, "error": message])
        }
    }

    var reportText: String {
        guard let report,
              let data = try? JSONSerialization.data(withJSONObject: report.json,
                                                     options: [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes])
        else { return "" }
        return String(decoding: data, as: UTF8.self)
    }

    /// Removes every run's folder but the latest, to free the phone's disk.
    func deleteOldRuns() {
        let fm = FileManager.default
        let keep = report?.id
        for name in (try? fm.contentsOfDirectory(atPath: Storage.runs.path)) ?? [] where name != keep {
            try? fm.removeItem(at: Storage.runs.appendingPathComponent(name))
        }
    }
}
