# Device test app

A minimal SwiftUI app that runs the compiler on an iPhone through the C ABI
(`osm-tile-compiler-ffi`), for the first on-device numbers (roadmap F6). It
is a measuring tool, not a sample app to ship.

It compiles a region with the `compile_basemap` job kind, either a whole
extract or an area of one (`area.bounds` with `read: near_area`, so only the
OSM data near the area is read), writes an offline style pack with
`write_style_pack`, and shows the archive in MapLibre Native through a
`pmtiles://file://` URL. For each job it reports the wall time, the CPU time,
the time per stage, the peak memory footprint (sampled every 100 ms) and the
least memory iOS had left for the app, and it appends every run to
`Documents/runs.jsonl`.

## Build and run

On a Mac with Xcode, rustup and XcodeGen (`brew install xcodegen`):

```sh
DEVELOPMENT_TEAM=<team id> ios/DeviceTest/bootstrap.sh
open ios/DeviceTest/OtcDeviceTest.xcodeproj
```

`bootstrap.sh` runs `scripts/build-xcframework.sh` (release builds for
`aarch64-apple-ios` and `aarch64-apple-ios-sim` into
`target/ios/OsmTileCompilerFFI.xcframework`), bundles
`fixtures/monaco.osm.pbf`, and generates the Xcode project. Pick the iPhone,
check the signing team, and Run. Run it from Xcode for the first install,
then from the home screen for measurements, without the debugger attached.

Extracts named on `bootstrap.sh`'s command line are bundled too. Larger ones
are better copied into the installed app's Documents, which keeps installs
quick:

```sh
ios/DeviceTest/push-data.sh /path/to/malopolskie-261118.osm.pbf
ios/DeviceTest/push-data.sh --sources /path/to/sources   # optional datasets
ios/DeviceTest/push-data.sh --pull                        # runs.jsonl back
```

Finder (the iPhone's Files tab) works as well. Extracts whose names start
with `malopolskie`, `berlin` or `pomorskie` also get area presets (Kraków,
Berlin Mitte, Gdańsk); see `Sources/Regions.swift`.

## Notes

- The style pack fetches glyphs and the sprite from the default servers, so
  the phone needs a network for that step; the compile needs none.
- Without `Documents/sources`, the compile leaves out the sea, Natural Earth
  and lake centerlines (`omitSources`), as the simulator's "no supplemental
  datasets" row in docs/performance.md did.
- If iOS ends the app mid-run (usually at its memory limit), the next launch
  shows how far the run got and its peak footprint until then.
- `INCREASED_MEMORY=1 bootstrap.sh` signs with
  `com.apple.developer.kernel.increased-memory-limit`, which the App ID must
  have enabled. Compare runs with and without it.
- The peak footprint counts the whole app, SwiftUI included. The map is
  closed while a run is measured.
