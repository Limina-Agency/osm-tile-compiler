# Fixtures

`monaco.osm.pbf` is a small Monaco extract, copied from Planetiler's test
resources. `osm-tile-compiler/tests/monaco.rs` compiles it end to end
and compares the archive with `monaco-features.json`: the header and
metadata, tile and feature counts, and for every tile and layer a digest of
the features' IDs and attributes and one of their geometries. Any change to
the output, down to one coordinate, fails the test.

To record an intended change, rerun the test with `UPDATE_MONACO_FEATURES=1`
and commit the file:

```sh
UPDATE_MONACO_FEATURES=1 cargo test -p osm-tile-compiler --test monaco
```

To see what changed, set `MONACO_DUMP=<file>` before and after the change: it
writes every feature as one line of text (tile, layer, ID, attributes,
geometry), and the two dumps diff feature by feature.

`geofabrik-index-v1-nogeom.json` is a few entries of Geofabrik's extract
index (https://download.geofabrik.de/index-v1-nogeom.json), in its format:
Monaco, the regions above it, and names the lookup must tell apart.
`osm-tile-compiler/tests/area_download.rs` serves it and `monaco.osm.pbf`
from a local HTTP server to test `compile --area`. The nightly workflow runs
the same command against Geofabrik and checks that Monaco's entry here still
matches the live index. The comparison is of the parsed entry, so a change
of form alone, such as the live index giving ISO codes as a string where this
file has an array, needs no update here; `extracts.rs` reads both forms.

The record is made on Linux x86_64, as CI runs. The digests do not depend on
the platform's math library: with glibc's replaced by the `libm` crate's
(musl's algorithms), which round 3–24% of results differently, the output is
identical. It contains OpenStreetMap data, © OpenStreetMap contributors, under
the Open Database License 1.0: https://www.openstreetmap.org/copyright.
