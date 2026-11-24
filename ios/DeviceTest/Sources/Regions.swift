import Foundation

struct Bounds: Equatable {
    var north: Double
    var south: Double
    var east: Double
    var west: Double

    var json: [String: Any] { ["north": north, "south": south, "east": east, "west": west] }

    init(north: Double, south: Double, east: Double, west: Double) {
        self.north = north
        self.south = south
        self.east = east
        self.west = west
    }

    init?(json: Any?) {
        guard let object = json as? [String: Any],
              let north = object["north"] as? Double, let south = object["south"] as? Double,
              let east = object["east"] as? Double, let west = object["west"] as? Double
        else { return nil }
        self.init(north: north, south: south, east: east, west: west)
    }
}

/// An area inside an extract that the app can compile on its own, reading only
/// the OSM data near it (`area.read: near_area`).
struct AreaPreset {
    /// Matches extract file names that start with it, such as `malopolskie`.
    var filePrefix: String
    var name: String
    var bounds: Bounds
}

let areaPresets: [AreaPreset] = [
    AreaPreset(filePrefix: "malopolskie", name: "Kraków centre",
               bounds: Bounds(north: 50.090, south: 50.030, east: 20.010, west: 19.880)),
    AreaPreset(filePrefix: "malopolskie", name: "Kraków (city)",
               bounds: Bounds(north: 50.130, south: 49.970, east: 20.220, west: 19.790)),
    AreaPreset(filePrefix: "berlin", name: "Berlin Mitte",
               bounds: Bounds(north: 52.540, south: 52.495, east: 13.430, west: 13.350)),
    AreaPreset(filePrefix: "pomorskie", name: "Gdańsk centre",
               bounds: Bounds(north: 54.380, south: 54.330, east: 18.700, west: 18.600)),
]

/// One thing the app can compile: a whole extract, or an area of one.
struct Region: Identifiable, Hashable {
    var id: String { "\(file.path)#\(area?.name ?? "whole")" }
    var file: URL
    var fileBytes: UInt64
    var area: AreaPreset?

    var title: String {
        let base = file.deletingPathExtension().deletingPathExtension().lastPathComponent
        if let area { return "\(area.name) (from \(base))" }
        return "\(base), whole extract"
    }

    var slug: String {
        let text = (area?.name ?? file.deletingPathExtension().deletingPathExtension().lastPathComponent)
            .folding(options: .diacriticInsensitive, locale: nil)
            .lowercased()
        return String(text.map { $0.isLetter || $0.isNumber ? $0 : "-" })
    }

    static func == (a: Region, b: Region) -> Bool { a.id == b.id }
    func hash(into hasher: inout Hasher) { hasher.combine(id) }
}

enum Storage {
    static var documents: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    }

    /// Where `scripts/` or Finder put larger extracts: Documents, or the
    /// app bundle's DeviceData folder (filled by ios/DeviceTest/bootstrap.sh).
    static var extractFolders: [URL] {
        var folders = [documents]
        if let bundled = Bundle.main.url(forResource: "DeviceData", withExtension: nil) {
            folders.append(bundled)
        }
        return folders
    }

    /// Supplemental datasets, as `osm-tile-compiler fetch-sources` writes
    /// them, copied to Documents/sources.
    static var sources: URL { documents.appendingPathComponent("sources", isDirectory: true) }

    static var hasSources: Bool {
        let items = (try? FileManager.default.contentsOfDirectory(atPath: sources.path)) ?? []
        return !items.filter { !$0.hasPrefix(".") }.isEmpty
    }

    static var runs: URL { documents.appendingPathComponent("runs", isDirectory: true) }
    static var runLog: URL { documents.appendingPathComponent("runs.jsonl") }

    static func regions() -> [Region] {
        var seen = Set<String>()
        var out: [Region] = []
        for folder in extractFolders {
            let names = (try? FileManager.default.contentsOfDirectory(atPath: folder.path)) ?? []
            for name in names.sorted() where name.hasSuffix(".osm.pbf") && !seen.contains(name) {
                seen.insert(name)
                let file = folder.appendingPathComponent(name)
                let bytes = (try? FileManager.default.attributesOfItem(atPath: file.path)[.size] as? UInt64) ?? 0
                for area in areaPresets where name.lowercased().hasPrefix(area.filePrefix) {
                    out.append(Region(file: file, fileBytes: bytes, area: area))
                }
                out.append(Region(file: file, fileBytes: bytes, area: nil))
            }
        }
        return out
    }

    static func appendToLog(_ object: [String: Any]) {
        guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.sortedKeys, .withoutEscapingSlashes])
        else { return }
        let line = data + Data("\n".utf8)
        if let handle = try? FileHandle(forWritingTo: runLog) {
            handle.seekToEndOfFile()
            handle.write(line)
            try? handle.close()
        } else {
            try? line.write(to: runLog)
        }
    }
}
