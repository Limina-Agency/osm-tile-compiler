# MapLibre Tiles (MLT)

`compile --tile-format mlt` writes every tile as a MapLibre Tile instead of a
Mapbox Vector Tile. The C ABI takes `"tileFormat": "mlt"` in
`compile_basemap` and `write_style_pack` ([c-abi.md](c-abi.md)), the worker
request `tileFormat`, and Python `compile(..., tile_format="mlt")`. MVT stays
the default.

MLT is a supported output format from 1.0: the layout below and the minimum
renderer versions in [Renderers](#renderers) are part of the compatibility
promise. Earlier MapLibre releases that read MLT may draw it with small
differences, or not at all.

An MLT archive is about 20% smaller than the MVT one, gzipped (Berlin:
33.3 MB against 41.9 MB), and holds exactly the same features.

## What is written

- MLT specification v1 (frame tag `0x01`), the stable layout. v2 (`0x02`) is
  experimental upstream and is not written.
- PMTiles tile type 6; MBTiles and TileJSON metadata `format` `mlt`.
- Each tile is transcoded from the finished MVT tile, so its layers,
  features, feature order, ids, coordinates, ring winding and attributes are
  the MVT ones, and Planetiler parity, checked on MVT, carries over. A column
  whose values mix kinds (say integers and strings) is written as the one
  kind that holds them all, as MLT columns have one type.
- Only the plain encodings: varint, delta, run-length, componentwise-delta
  vertices, ORC byte-RLE booleans, and plain or dictionary strings. No
  FastPFOR, FSST, Morton, shared dictionaries or tessellation, which would
  make tiles a little smaller but give decoders more to get wrong.
- The encoder (`src/mlt/`) is this project's own, with no `mlt-core`
  dependency, whose API changes from minor version to minor version and
  whose Rust version is above this crate's.

`verify` and `inspect` decode every MLT tile. `search-index`, `--search-index`,
`building-levels` and `--building-levels` read MLT basemaps as they read MVT
ones: the index and the levels come out byte for byte the same.

## Renderers

A style for an MLT archive needs `"encoding": "mlt"` on its vector source;
`style --tile-format mlt` and style packs with `tileFormat: "mlt"` write it,
and `serve` sets it for an `.mlt` archive. Without it, MapLibre reads the
tiles as MVT and fails.

| Renderer | Supported from | Notes |
|---|---|---|
| MapLibre GL JS | 5.12 | 5.24.1 and 6.19.0 render Berlin pixel for pixel as from MVT |
| MapLibre Native, Android | 13.4.0 | reads MLT from 12.1, with the decoder issues below |
| MapLibre Native, iOS | 6.28.0 | reads MLT from 6.2, with the decoder issues below |
| MapLibre Native, Node (`@maplibre/maplibre-gl-native`) | none yet | 6.4.1 draws the same map, with polygon edges 1 to 3 levels off (see below) |
| Martin | 1.3 | serves MLT archives as they are |

MapLibre Native bundles the C++ decoder of maplibre-tile-spec. Before
maplibre-tile-spec#816 (February 2026) that decoder left polygon rings
open, so a fill's antialiased outline missed each ring's last edge: up to
0.5% of pixels 1 to 3 levels out of 255 against the MVT map. Android 13.4.0
and iOS 6.28.0 are the first releases with the fix; the Node binding, up to
6.5.0-pre.1, still has the old decoder. MLT stores rings without their
closing vertex, as the specification says, so the fix is the decoder's:
re-rendering Berlin with the closing vertex written, as a test, brings the
Node binding to the MVT map within 18 pixels of 585,000. Those releases also
have maplibre-tile-spec#1239 and #1450, which the encoder works around for
older ones: every string column has a present stream, and no run-length
stream holds more runs than values.

## How it is checked

- `src/mlt/tests.rs`: every encoding decision, and every MVT layer
  transcoded, decoded and compared with the MVT one, then transcoded back.
- The specification's synthetic fixtures (`fixtures/mlt-synthetic`,
  `fixtures/mlt-synthetic-rust`): the decoder that checks the encoder reads
  all 416 fixtures in encodings it supports. The nightly workflow runs
  `scripts/check-mlt-spec.sh` against the specification's main branch.
- `tests/mlt.rs`: Monaco compiled to MLT holds what it holds in MVT, and its
  search index and building levels are those of the MVT basemap.
- `scripts/check-mlt-conformance.sh`, in CI: the tiles decoded by the three
  reference decoders, mlt-core 0.23 (through the `mlt` tool), @maplibre/mlt
  1.3 (GL JS) and the C++ decoder at the commit MapLibre Native's newest
  releases bundle, each of which must print exactly the features the tiles
  hold. CI runs it on Monaco; on Berlin (938 tiles, 1,124,400 features) all
  three agree to the feature.
