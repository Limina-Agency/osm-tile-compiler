# Roadmap

Feature work is planned in lanes that can proceed in parallel. The first
direction is the basemap engine that apps embed; lane G (general compiler)
needs an explicit go/no-go decision.

**Rule:** lane A owns every change to tile content. Other lanes either leave the
output unchanged (lane 0's fingerprint proves it) or add their change behind an
opt-in flag.

## Lane 0 — Output regression guard

`tests/monaco.rs` records, in `fixtures/monaco-features.json`, the archive
header and metadata and a fingerprint of every decoded tile's content per
layer, so a one-coordinate change fails CI. A change to the fingerprint must be
deliberate and explained in its pull request.

## Lane A — Planetiler parity

1. ~~Re-establish the comparison laboratory on Linux~~: done
   (`benchmarks/compiler-parity/setup-linux.sh`); results in
   `docs/parity.md`.
2. ~~Define the parity bar~~: proposed in `docs/parity.md`.
3. ~~Close the differences the 2026-11-03 baseline lists~~; what is left, and the accepted differences, are in
   `docs/parity.md`.
4. ~~Add two or three regions to the comparison~~: Berlin and Pomorskie, in
   `docs/parity.md`, with the differences they add.
5. ~~Set `parity_verified` once the bar is met, citing the run that met it~~:
   met on 0.1.3, 2026-11-19 ("Verified on 0.1.3" in `docs/parity.md`).

## Lane B — Area and extent (no content change)

1. ~~`--polygon <file.geojson|.poly>`: emit only tiles touching the polygon, as
   Planetiler does; also filter the supplemental datasets by it.~~
2. ~~Bounds crossing the antimeridian (`west > east`): two tile ranges.~~
   Also polygons crossing it, and wrapped PBF header boxes.
3. ~~Archive center and center zoom derived from the area, with `--center` to
   override, instead of the mean of the zoom range.~~
4. ~~The same options in the worker request.~~

## Lane C — Tile size observability (no content change)

1. ~~Per-tile, per-layer and per-zoom size statistics in `Store::finish`.~~
2. ~~Warnings above a threshold (1 MB by default) in the compile result.~~
3. ~~Statistics in `--json` and the worker manifest.~~
4. Optional, flagged, lane A sign-off: a size budget that drops low-priority
   features.

## Lane D — Output and archive tooling (no content change)

1. ~~Configurable metadata (name, description, attribution); one layer list.~~
2. ~~Selectable gzip level.~~
3. ~~`inspect` and `verify` commands.~~
4. MBTiles output, only if the desktop app needs it.

## Lane E — Configurability

1. ~~`--layers` / `--exclude-layers`; selected layers stay identical to a full
   build.~~
2. ~~Language selection for `name:*` fields.~~
3. One schema description driving rendering and metadata.
4. A `Profile` trait separating the OMT schema from the engine (refactor only;
   byte-identical output).

## Lane F — Scale and performance

1. Country-scale benchmarks (Poland, then Germany) against Planetiler: peak
   memory, scratch disk, time.
2. Document the memory model and hardware needed per extract size.
3. Fix the worst bottleneck found (possibly in `osm-pbf-ingest`).
4. A planet-scale feasibility note.

## Lane G — General compiler (deferred; needs a decision)

Depends on lane E step 4: GeoJSON, FlatGeobuf and GeoPackage overlays as
extra layers; a second schema (Shortbread or Protomaps); Wikidata name
translations; declarative profiles.

## Sequencing

| Phase | In parallel |
|---|---|
| 1 | Lane 0 |
| 2 | A (continuous) · B · C · D1–D2 |
| 3 | A · E1–E3 · D3 · F1–F2 |
| 4 | A sign-off · E4 · F3–F4 · G decision |

`omt/store.rs` is shared by lanes A, C, D and E: land C and D1 (additive,
around `finish` and `metadata`) before lane A's larger refactors. Lanes B, D
and E all add worker-request fields; agree on one versioned request shape
first.
