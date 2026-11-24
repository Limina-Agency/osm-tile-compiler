import Foundation
import OsmTileCompilerFFI

struct RunOptions {
    var threads: Int
    var buildingLevels: Bool
    var useSources: Bool
    var stylePack: Bool
}

/// What one job cost, measured from the host's side while it ran.
struct JobMeasurement {
    var seconds: Double
    var cpuSeconds: Double
    var stages: [(stage: String, seconds: Double)]
    var startFootprint: UInt64
    var peakFootprint: UInt64
    var minAvailable: UInt64
    var outcome: Result<[String: Any], JobFailure>

    var json: [String: Any] {
        var out: [String: Any] = [
            "seconds": round3(seconds),
            "cpuSeconds": round3(cpuSeconds),
            "startFootprintMiB": mib(startFootprint),
            "peakFootprintMiB": mib(peakFootprint),
            "minAvailableMiB": mib(minAvailable),
            "stages": stages.map { ["stage": $0.stage, "seconds": round3($0.seconds)] },
        ]
        switch outcome {
        case .success(let result): out["ok"] = true; out["result"] = result
        case .failure(let error): out["ok"] = false; out["error"] = error.json
        }
        return out
    }
}

/// The outcome of a run, for the UI and the map.
struct RunReport {
    var id: String
    var region: Region
    var json: [String: Any]
    var compile: JobMeasurement?
    var stylePack: JobMeasurement?
    var styleURL: URL?
    var bounds: Bounds?
    var error: String?
}

private let inFlightKey = "inFlightRun"

/// Runs a compile and a style pack through the C ABI, sampling memory every
/// 100 ms, and logs each run to Documents/runs.jsonl.
final class Runner: @unchecked Sendable {
    private let lock = NSLock()
    private var current: CompilerJob?
    private var cancelled = false

    func cancel() {
        lock.lock()
        cancelled = true
        let job = current
        lock.unlock()
        job?.cancel()
    }

    /// A run that started and never logged an end: iOS most likely killed the
    /// app, usually at its memory limit.
    static func unfinishedRun() -> [String: Any]? {
        UserDefaults.standard.dictionary(forKey: inFlightKey)
    }

    static func clearUnfinishedRun() {
        UserDefaults.standard.removeObject(forKey: inFlightKey)
    }

    func run(region: Region, options: RunOptions, progress: @escaping (String, JobProgress?) -> Void) -> RunReport {
        lock.lock(); cancelled = false; lock.unlock()
        let stamp = ISO8601DateFormatter.string(from: Date(), timeZone: .current,
                                                formatOptions: [.withFullDate, .withTime, .withTimeZone])
        let runId = "\(stamp.replacingOccurrences(of: ":", with: ""))-\(region.slug)"
        let fm = FileManager.default
        let dir = Storage.runs.appendingPathComponent(runId, isDirectory: true)
        let temp = dir.appendingPathComponent("tmp", isDirectory: true)
        let output = dir.appendingPathComponent("\(region.slug).pmtiles")
        let styleDir = dir.appendingPathComponent("style", isDirectory: true)
        try? fm.createDirectory(at: temp, withIntermediateDirectories: true)
        try? fm.createDirectory(at: Storage.sources, withIntermediateDirectories: true)

        var json: [String: Any] = [
            "event": "run",
            "id": runId,
            "startedAt": ISO8601DateFormatter().string(from: Date()),
            "device": Metrics.device,
            "library": ["version": CompilerLibrary.version, "abiVersion": CompilerLibrary.abiVersion],
            "region": [
                "name": region.title,
                "file": region.file.lastPathComponent,
                "fileMiB": mib(region.fileBytes),
                "bounds": region.area.map { $0.bounds.json as Any } ?? NSNull(),
            ],
            "options": [
                "threads": options.threads,
                "buildingLevels": options.buildingLevels,
                "supplementalDatasets": options.useSources,
            ],
            "thermalStart": Metrics.thermalState,
        ]
        UserDefaults.standard.set([
            "id": runId, "region": region.title, "threads": options.threads,
            "startedAt": json["startedAt"] as? String ?? "",
        ] as [String: Any], forKey: inFlightKey)

        var report = RunReport(id: runId, region: region, json: json)

        // The compile.
        var request: [String: Any] = [
            "inputPbf": region.file.path,
            "output": output.path,
            "sources": Storage.sources.path,
            "tempDir": temp.path,
            "sourcesPolicy": "use_present",
            "threads": options.threads,
            "buildingLevels": options.buildingLevels,
            "archive": ["name": region.title],
        ]
        if !options.useSources {
            request["omitSources"] = ["water_polygons", "natural_earth", "lake_centerlines"]
        }
        if let area = region.area {
            request["area"] = ["bounds": area.bounds.json, "read": "near_area"]
        }
        progress("Starting the compile", nil)
        let compile = measure(kind: "compile_basemap", request: request, runId: runId, progress: progress)
        report.compile = compile
        json["compile"] = compile.json
        if case .success(let result) = compile.outcome {
            json["archiveMiB"] = mib(fileSize(output))
            if let levels = result["buildingLevels"] as? String {
                json["buildingLevelsMiB"] = mib(fileSize(URL(fileURLWithPath: levels)))
            }
            report.bounds = Bounds(json: result["bounds"]) ?? region.area?.bounds
        } else if case .failure(let error) = compile.outcome {
            report.error = "Compile: \(error.localizedDescription)"
        }
        let leftovers = (try? fm.contentsOfDirectory(atPath: temp.path)) ?? []
        json["scratchLeftovers"] = leftovers.count
        try? fm.removeItem(at: temp)

        // The style pack, which fetches glyphs and the sprite over the network.
        if report.error == nil, options.stylePack, !isCancelled {
            let fileURL = output.absoluteURL.absoluteString // file:///…
            let packRequest: [String: Any] = [
                "directory": styleDir.path,
                "archiveUrl": "pmtiles://\(fileURL)",
                "archiveFile": output.path,
                "baseUrl": styleDir.absoluteURL.absoluteString,
            ]
            progress("Writing the style pack", nil)
            let pack = measure(kind: "write_style_pack", request: packRequest, runId: runId, progress: progress)
            report.stylePack = pack
            json["stylePack"] = pack.json
            switch pack.outcome {
            case .success(let result):
                if let style = result["style"] as? String { report.styleURL = URL(fileURLWithPath: style) }
            case .failure(let error):
                report.error = "Style pack: \(error.localizedDescription)"
            }
        }

        json["thermalEnd"] = Metrics.thermalState
        let memory = Metrics.memory()
        json["lifetimePeakFootprintMiB"] = mib(memory.lifetimePeakFootprint)
        json["lifetimePeakResidentMiB"] = mib(memory.residentPeak)
        json["finishedAt"] = ISO8601DateFormatter().string(from: Date())
        report.json = json
        Storage.appendToLog(json)
        Runner.clearUnfinishedRun()
        return report
    }

    private var isCancelled: Bool {
        lock.lock(); defer { lock.unlock() }
        return cancelled
    }

    private func measure(kind: String, request: [String: Any], runId: String,
                         progress: @escaping (String, JobProgress?) -> Void) -> JobMeasurement {
        let startMemory = Metrics.memory()
        let startCPU = Metrics.cpuSeconds()
        let start = Date()
        var peak = startMemory.footprint
        var minAvailable = startMemory.available
        var stageStarts: [(String, Date)] = []

        let job: CompilerJob
        do {
            job = try CompilerJob(kind: kind, request: request)
        } catch {
            let failure = error as? JobFailure ?? JobFailure(kind: "internal", message: "\(error)")
            return JobMeasurement(seconds: 0, cpuSeconds: 0, stages: [], startFootprint: startMemory.footprint,
                                  peakFootprint: peak, minAvailable: minAvailable, outcome: .failure(failure))
        }
        lock.lock()
        current = job
        let cancelNow = cancelled
        lock.unlock()
        if cancelNow { job.cancel() }

        var lastUpdates = -1
        var lastSaved = Date.distantPast
        while true {
            let state = job.wait(milliseconds: 100)
            let memory = Metrics.memory()
            peak = max(peak, memory.footprint)
            minAvailable = min(minAvailable, memory.available)
            if let p = job.poll() {
                if let stage = p.stage, stageStarts.last?.0 != stage {
                    stageStarts.append((stage, Date()))
                }
                if p.updates != lastUpdates {
                    lastUpdates = p.updates
                    progress(p.message.isEmpty ? kind : p.message, p)
                }
            }
            if Date().timeIntervalSince(lastSaved) > 1 {
                // Survives a kill at the memory limit, so the next launch can say how far it got.
                var inFlight = UserDefaults.standard.dictionary(forKey: inFlightKey) ?? [:]
                inFlight["job"] = kind
                inFlight["stage"] = stageStarts.last?.0 ?? "starting"
                inFlight["seconds"] = round3(Date().timeIntervalSince(start))
                inFlight["peakFootprintMiB"] = mib(peak)
                inFlight["minAvailableMiB"] = mib(minAvailable)
                UserDefaults.standard.set(inFlight, forKey: inFlightKey)
                lastSaved = Date()
            }
            if state != OTC_JOB_RUNNING { break }
        }
        let end = Date()
        let cpu = Metrics.cpuSeconds() - startCPU
        lock.lock(); current = nil; lock.unlock()

        var stages: [(String, Double)] = []
        for (index, entry) in stageStarts.enumerated() {
            let next = index + 1 < stageStarts.count ? stageStarts[index + 1].1 : end
            stages.append((entry.0, next.timeIntervalSince(entry.1)))
        }
        let outcome: Result<[String: Any], JobFailure>
        do { outcome = .success(try job.result()) } catch {
            outcome = .failure(error as? JobFailure ?? JobFailure(kind: "internal", message: "\(error)"))
        }
        return JobMeasurement(seconds: end.timeIntervalSince(start), cpuSeconds: cpu, stages: stages,
                              startFootprint: startMemory.footprint, peakFootprint: peak,
                              minAvailable: minAvailable, outcome: outcome)
    }
}

private func fileSize(_ url: URL) -> UInt64 {
    (try? FileManager.default.attributesOfItem(atPath: url.path)[.size] as? UInt64) ?? 0
}

func round3(_ value: Double) -> Double { (value * 1000).rounded() / 1000 }
