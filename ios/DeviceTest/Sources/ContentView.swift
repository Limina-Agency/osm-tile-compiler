import SwiftUI
import UIKit

struct ContentView: View {
    @EnvironmentObject private var model: DeviceTestModel

    var body: some View {
        NavigationStack {
            Form {
                if let unfinished = model.unfinished {
                    Section("Previous run did not finish") {
                        Text("iOS most likely ended the app, usually at its memory limit.")
                            .font(.footnote)
                        ForEach(unfinished.keys.sorted(), id: \.self) { key in
                            row(key, "\(unfinished[key] ?? "")")
                        }
                        Button("Log it and dismiss") { model.dismissUnfinished() }
                    }
                }

                Section("Device and library") {
                    row("Device", Metrics.deviceModel)
                    row("System", "\(UIDevice.current.systemName) \(UIDevice.current.systemVersion)")
                    row("Cores", "\(ProcessInfo.processInfo.activeProcessorCount)")
                    row("RAM", formatMiB(ProcessInfo.processInfo.physicalMemory))
                    row("Footprint now", formatMiB(model.memoryNow.footprint))
                    row("Available to app", formatMiB(model.memoryNow.available))
                    row("Library", "\(CompilerLibrary.version), ABI \(CompilerLibrary.abiVersion)")
                    if CompilerLibrary.abiVersion != CompilerLibrary.headerAbiVersion {
                        Text("The header's ABI version is \(CompilerLibrary.headerAbiVersion): rebuild the XCFramework.")
                            .foregroundStyle(.red)
                    }
                }

                Section {
                    if model.regions.isEmpty {
                        Text("No .osm.pbf found in the app bundle's DeviceData or in Documents.")
                    } else {
                        Picker("Region", selection: $model.selectedID) {
                            ForEach(model.regions) { region in
                                Text(region.title).tag(Optional(region.id))
                            }
                        }
                        .pickerStyle(.navigationLink)
                        if let region = model.selected {
                            row("Extract", "\(region.file.lastPathComponent), \(formatMiB(region.fileBytes))")
                            if let area = region.area {
                                row("Area", String(format: "N %.3f S %.3f E %.3f W %.3f", area.bounds.north,
                                                   area.bounds.south, area.bounds.east, area.bounds.west))
                            }
                        }
                    }
                    Button("Rescan extracts") { model.refresh() }
                } header: {
                    Text("Region")
                } footer: {
                    Text("Areas read only the OSM data near them (area.read near_area). Add extracts to the app's Documents with Finder or ios/DeviceTest/push-data.sh.")
                }

                Section("Options") {
                    Stepper("Threads: \(model.threads)", value: $model.threads,
                            in: 1...ProcessInfo.processInfo.activeProcessorCount)
                    Toggle("Building levels", isOn: $model.buildingLevels)
                    Toggle("Supplemental datasets", isOn: $model.useSources)
                        .disabled(!model.hasSources)
                    if !model.hasSources {
                        Text("No datasets in Documents/sources, so sea, Natural Earth and lake centerlines are left out.")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                    Toggle("Style pack and map (needs network)", isOn: $model.stylePack)
                }
                .disabled(model.isRunning)

                Section {
                    if model.isRunning {
                        ProgressView(value: model.progress?.overall ?? model.progress?.fraction ?? 0) {
                            Text(model.status)
                        } currentValueLabel: {
                            Text(model.progress?.stage ?? model.progress?.kind ?? "")
                        }
                        Button("Cancel", role: .destructive) { model.cancel() }
                    } else {
                        Button("Compile") { model.run() }
                            .disabled(model.selected == nil)
                        if !model.status.isEmpty { Text(model.status).font(.footnote) }
                    }
                }

                if let report = model.report {
                    ReportSection(report: report)
                }
            }
            .navigationTitle("Tile compiler")
            .navigationDestination(isPresented: $model.showMap) {
                if let report = model.report, let style = report.styleURL {
                    MapView(styleURL: style, bounds: report.bounds,
                            onRendered: { model.rendered($0) },
                            onFailed: { model.renderFailed($0) })
                        .ignoresSafeArea(edges: .bottom)
                        .overlay(alignment: .top) {
                            Group {
                                if let error = model.renderError {
                                    Text("Map failed: \(error)")
                                } else if let seconds = model.renderSeconds {
                                    Text(String(format: "First full render %.2f s", seconds))
                                } else {
                                    Text("Rendering…")
                                }
                            }
                            .font(.footnote)
                            .padding(8)
                            .background(.thinMaterial, in: Capsule())
                            .padding(.top, 8)
                        }
                        .navigationTitle(report.region.area?.name ?? report.region.title)
                        .navigationBarTitleDisplayMode(.inline)
                }
            }
        }
    }
}

private struct ReportSection: View {
    @EnvironmentObject private var model: DeviceTestModel
    var report: RunReport

    var body: some View {
        Section("Last run") {
            if let error = report.error {
                Text(error).foregroundStyle(.red)
            }
            if let compile = report.compile {
                measurementRows("Compile", compile)
                if let archive = report.json["archiveMiB"] as? Double {
                    row("Archive", String(format: "%.1f MiB", archive))
                }
                if let levels = report.json["buildingLevelsMiB"] as? Double {
                    row("Building levels", String(format: "%.1f MiB", levels))
                }
                ForEach(Array(compile.stages.enumerated()), id: \.offset) { _, stage in
                    row("  \(stage.stage)", String(format: "%.2f s", stage.seconds))
                }
            }
            if let pack = report.stylePack {
                measurementRows("Style pack", pack)
            }
            if let lifetime = report.json["lifetimePeakFootprintMiB"] as? Double {
                row("App's lifetime peak footprint", String(format: "%.1f MiB", lifetime))
            }
            if let seconds = model.renderSeconds {
                row("First full map render", String(format: "%.2f s", seconds))
            }
            if report.styleURL != nil {
                Button("Show map") { model.showMap = true }
            }
            Button("Copy run JSON") { UIPasteboard.general.string = model.reportText }
            ShareLink("Share runs.jsonl", item: Storage.runLog)
            Button("Delete older runs' files", role: .destructive) { model.deleteOldRuns() }
        }
    }

    @ViewBuilder
    private func measurementRows(_ title: String, _ m: JobMeasurement) -> some View {
        row("\(title) time", String(format: "%.2f s (CPU %.1f s)", m.seconds, m.cpuSeconds))
        row("\(title) peak footprint", String(format: "%.1f MiB (from %.1f)", mib(m.peakFootprint), mib(m.startFootprint)))
        row("\(title) min. available", String(format: "%.1f MiB", mib(m.minAvailable)))
    }
}

private func row(_ label: String, _ value: String) -> some View {
    HStack {
        Text(label)
        Spacer()
        Text(value).foregroundStyle(.secondary).multilineTextAlignment(.trailing)
    }
}
