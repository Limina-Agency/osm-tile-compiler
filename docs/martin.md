# Serving archives with Martin (experimental)

[Martin](https://github.com/maplibre/martin) is MapLibre's tile server. It
reads PMTiles and MBTiles files as they are, so it serves this compiler's
archives with no plugin: `martin-config` writes a config and a style for each
archive, and `apply --in-place` updates an archive Martin is serving without a
restart. Checked against Martin 2.0.0-beta.7, built from source.

```sh
osm-tile-compiler martin-config live/ --output martin/   # live/ holds berlin.pmtiles
martin --config martin/martin.yaml
# style: http://localhost:3000/style/berlin   tiles: http://localhost:3000/berlin/{z}/{x}/{y}

# later, with a signed delta from the publisher:
osm-tile-compiler apply live/berlin.pmtiles update.delta --in-place --trusted-key release.key.pub
```

## What `martin-config` writes

- `martin.yaml`: a directory given is listed under `pmtiles.paths` (or
  `mbtiles.paths`), which Martin watches, so archives added to it, replaced in
  it or removed from it are served at once. A file given is listed under
  `sources`, named by its file name without the extension.
- `styles/<NAME>.json`: OSM Bright for each archive found, with its source
  pointing at Martin's TileJSON (`/<NAME>`), `"encoding": "mlt"` for MapLibre
  Tiles, the `sar` layers when the archive has the `sar` layer, and the
  archive's center. The URLs are relative; Martin makes them absolute for
  each client from the address it reached.
- `--fonts DIR` serves the glyphs from font files (Martin builds glyph ranges
  from `.ttf`/`.otf`); the style needs Noto Sans Regular, Bold and Italic.
  `--sprites DIR` builds the sprite from SVG icons. Without them the style
  loads both from the network, as `style` prints it.

It warns about archives Martin refuses (see MLT below).

## Updates and signed deltas

`apply --in-place` writes the new archive beside the old one
(`<archive>.partial`, which Martin ignores) and renames it over the old one
only after the delta's signature (with `--trusted-key`) and the result's
SHA-256 have been checked. Martin never sees an unverified or half-written
archive, and a failed apply leaves the old archive in place.

Measured on Berlin (42.5 MB, 13 hours of planet diffs, a 120 kB signed delta
changing 64 tiles):

- In a watched directory, Martin served the new tiles 12 to 35 ms after
  `apply` finished, and invalidated its tile cache for the archive.
- 16 clients requesting tiles across the swap, with Martin's cache off, three
  rounds, about 11,000 requests each: every tile matched either the old or the
  new archive, with no errors.
- Afterwards all 963 tiles matched the new archive byte for byte.

A file listed under `sources` (not in a watched directory) is also reloaded,
but only when a request misses Martin's tile cache; until then cached tiles
from the old archive are served. Keep live archives in a watched directory.

The detached signature of the old archive (`<archive>.sig`) does not sign the
new one; the delta's signature is what vouched for it.

Not checked: Windows, where renaming over a file another process is reading
can fail; Martin 1.x.

## MLT

Martin 2.0.0-beta.7 refuses PMTiles archives of compressed MapLibre Tiles: it
accepts tile compression for MVT only. This compiler gzips MLT tiles, so an
MLT archive is refused, and with the default `on_invalid` Martin does not
start at all. Until that changes upstream (a one-line check in
`martin-core/src/tiles/pmtiles/source.rs`):

- compile with `--tile-compression none`, which stores each tile as
  encoded. Stock Martin serves those (Berlin: all 938 tiles identical to the
  gzipped archive's, unzipped). The archive is larger (54.4 MB against
  33.3 MB), and Martin sends the tiles uncompressed unless a proxy in front
  of it gzips them;
- or serve the MVT archive and set `convert_to_mlt: auto`: Martin converts per
  request for clients that send `Accept: application/vnd.maplibre-tile`. On
  Berlin, 684 tiles/s on 8 connections; the tiles are sent uncompressed
  (37.7 MB in all, against 33.3 MB for this compiler's gzipped MLT and 42.4 MB
  for gzipped MVT). MapLibre does not send that header yet, so clients add it
  with `transformRequest`;
- or serve gzipped MLT archives with `serve`.

Martin names the MLT media type `application/vnd.maplibre-tile`; `serve` and
the `pmtiles` crate say `application/vnd.maplibre-vector-tile`.

## What Martin does not do

- The search index: serve it with `serve`, or search on the device.
- Deltas: they are static files, for any web server or object store.
- A style pack as `style --offline` writes it (glyph `.pbf` ranges and a PNG
  sprite): Martin builds glyphs and sprites from fonts and SVGs instead.
