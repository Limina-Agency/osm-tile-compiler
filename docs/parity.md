# Planetiler parity

Where the native OpenMapTiles profile stands against Planetiler 0.10.2 (the
pinned reference, profile revision `91516fdf`), measured on whole archives.
The bar below was met on 0.1.3 (2026-11-19, "Verified on 0.1.3"), and
`CompileResult::parity_verified` is `true` for the compiles that
measurement covers: Planetiler's default languages, every supplemental dataset,
a full read of the extract and zoom 14 at the top.

## Baseline: 2026-11-03

Malopolskie extract downloaded 2026-11-03 (SHA-256 `def1eb00…eb1f46c`),
bounds 18.87522 49.02835 21.43328 50.53221, zooms 0–14, four threads,
Linux x86_64 (16 cores, 15 GiB). Native build of that day, before the
fixes below. Prepared with
`benchmarks/compiler-parity/setup-linux.sh`, run with `run_omt.py`, compared
with `compare.py --all-tiles` and `diagnose_all_omt.py` over every tile.

| | Wall time | Peak RSS | Archive |
|---|---|---|---|
| native | 36.1 s | 0.87 GiB | 154.5 MB |
| Planetiler | 107.7 s | 3.32 GiB | 152.1 MB |

Both archives pass `osm-tile-compiler verify` and address the same
11,293 tiles. Native tiles are within 1% of Planetiler's in size at every zoom
except 9 and 10, where they are 5–6% larger.

### Features (all 11,293 tiles, 2.76 million features)

Every layer has the same number of features per tile. Features pair by
layer, ID and geometry type.

| Layer | Features | Geometry differs | Unpaired (each side) | Attributes differ |
|---|---|---|---|---|
| building | 23,783 | 1,500 | 6 | 4 |
| landuse | 48,338 | 508 | 1 | 0 |
| landcover | 582,782 | 52 | 2 native only | 0 |
| boundary | 14,181 | 1 | 2 | 0 |
| housenumber | 633,244 | 0 | 152 | 0 |
| transportation | 200,556 | 0 | 18 | 0 |
| transportation_name | 57,928 | 0 | 9 | 0 |
| aerodrome_label, aeroway, mountain_peak, park, place, poi, water, water_name, waterway | 1,282,254 | 0 | 0 | 0 |

- **Geometry**: polygon merging. Planetiler's `FeatureMerge` buffers nearby
  polygons out and back in with JTS; building clusters at z13 and landuse and
  landcover at low zooms come out slightly different. The largest building
  difference covers 7% of the cluster's area. Large low-zoom polygons are
  likely behind the extra bytes at z9–z10.
- **Unpaired**: the same features under different IDs. Where features are
  merged or deduplicated (house numbers within a partition, merged road
  segments, merged buildings), the two keep a different member's ID; each
  layer is short exactly as many as it has extra.
- **Attributes**: two overlapping buildings whose `render_height` 4 and 5 are
  swapped.
- **Validity**: about 9,700 polygons on each side are self-intersecting after
  tile-grid snapping, in the same places. Matching Planetiler reproduces them;
  they are a separate quality question from parity.

## After merged-feature IDs and polygon merging (2026-11-03)

The same extract, reference and comparison, with two fixes: native keeps
Planetiler's IDs where features merge, and merges nearby polygons through a
port of JTS's buffer and union. Compile 31–46 s, archive 153.6 MB (Planetiler
152.1 MB); tile sizes are within 0.7% of Planetiler's at every zoom.

| Layer | Geometry differs | Unpaired (each side) | Attributes differ |
|---|---|---|---|
| building | 1,500 → 45 | 6 → 0 | 4 → 0 |
| landuse | 508 → 30 | 1 | 0 |
| landcover | 52 | 2 native only | 0 |
| boundary | 1 | 2 → 0 | 0 |
| housenumber | 0 | 152 → 0 | 0 |
| transportation | 0 | 18 → 0 | 0 |
| transportation_name | 0 | 9 → 0 | 0 |
| all other layers | 0 | 0 | 0 |

Feature counts per tile and zoom are unchanged and equal to Planetiler's.
Invalid polygons are as many as before on each side (9,743 native, 9,748
Planetiler), and no native polygon is invalid where Planetiler's is valid.
One known building cluster (13/4571/2774) is kept deliberately: JTS reduces
it to a near-empty triangle, native keeps the buildings.

## More regions: 2026-11-03

Two more regions, compared the same way, with the native build of that day:
Berlin, a dense city, and Pomorskie, the Baltic coast with the Gulf of
Gdańsk, the Hel peninsula, the Vistula Lagoon and the Kashubian lakes. Each was
prepared with `setup-linux.sh BENCH_DIR PBF_URL` and needed no harness change.
Other compilations shared the machine (load average 14–21 during the runs), so
times are noisy: Pomorskie's native compile took 22.4 s in an interrupted
first run and 78.1 s in the run below, writing the same archive bytes.

Polygon differences are measured on every differing pair of every differing
tile: symmetric difference as a share of Planetiler's area, and the
(vertex-based) Hausdorff distance in pixels of a 256-pixel tile; "within the
bar" means under 1% and under 1 pixel. `polygon_impact.py` samples tiles and
skips polygons over 2,000 coordinates, so it measured only 2 (Berlin) and 6
(Pomorskie) of these pairs.

### Berlin

[berlin-latest.osm.pbf](https://download.geofabrik.de/europe/germany/berlin-latest.osm.pbf)
downloaded 2026-11-03 (replication timestamp 2026-11-02T21:21:26Z), SHA-256
`18fa3487…a27988f1`, bounds 13.08283 52.33446 13.762245 52.6783.

| | Wall time | Peak RSS | Archive |
|---|---|---|---|
| native | 41.4 s | 0.63 GiB | 41.9 MB |
| Planetiler | 123.2 s | 2.46 GiB | 41.1 MB |

Both archives pass `verify` and address the same 942 tiles; 92 differ (76 of
them at z13–14). Native tiles are 0.1–1.7% smaller at z0–8 and 0.6–2.0%
larger at z9–14.

| Layer | Features (native / Planetiler) | Geometry differs (within the bar) | Unpaired | Attributes differ |
|---|---|---|---|---|
| building | 11,849 / 11,848 | 59 (51) | 3 native, 2 Planetiler | 0 |
| landuse | 33,684 | 10 (10) | 0 | 0 |
| landcover | 60,455 | 1 (0) | 0 | 0 |
| transportation | 156,050 / 156,048 | 1 line | 2 native only | 0 |
| transportation_name | 30,496 | 0 | 0 | 26 |
| poi | 658,436 | 0 | 10 each side | 10 |
| the other 10 layers | 171,413 | 0 | 0 | 0 |

Counts are features summed over tiles; per tile they are equal except in the
three tiles holding the building and transportation extras.

- **building**: merged clusters, 52 at z13 and 7 at z14. Of the 8 outside the
  bar, the largest are 13/4398/2687 id 710366660 (1.2% of 15,100 px², 5.8 px),
  13/4394/2685 id 249277070 (13% of 612 px², 5.2 px) and 13/4401/2686 id
  3692698860 (0.2%, 7.2 px). The z14 ones are merged groups that are invalid
  on both sides and differ by 1–8 vertices at equal area.
- **building, unpaired**: all in 14/8801/5376. Native emits sub-pixel
  triangles (1–2.5 tile units²) that Planetiler drops: one as its own feature
  (way 365943217) and others inside `building:part` ways 338961787 and
  365433925, which become merged groups (IDs ending in 0) where Planetiler
  keeps the single ways (IDs ending in 2).
- **landuse**: z6–10, all within the bar (at most 0.04% and 0.57 px).
- **landcover**: 10/550/336 id 28373970, 0.7% and 1.08 px; native has one hole
  fewer.
- **transportation**: native emits way 405475171 (`man_made=bridge`,
  `public_transport=platform_access`) as a bridge polygon at z13–14. Cause:
  Planetiler's `Transportation.highwayClass` falls back to `man_made` only when
  `highway` and `public_transport` are both empty; native's `class()` in
  `transport.rs` falls back whenever `highway` is empty. Also one motorway line
  at 5/17/10 (id 45367920): same 147 vertices, 0.125 px apart.
- **transportation_name**: `layer` is missing on 5 `motorway_junction` nodes
  tagged `layer=2` or `-1`, 26 times across z10–14. Cause: Planetiler sets
  `layer` on junction points, native's junction branch in `transport.rs` does
  not.
- **poi, unpaired**: 5 bus stops (nodes 769360167, 769364226, 769364240,
  3870475219, 3870475220), 10 times across tiles, identical in content. Planetiler's IDs
  end in 0, which native gives to merged groups; native keeps the node IDs
  (ending in 1). Cause not established.
- **poi, attributes**: 4 POIs, 10 times across tiles. Three lack `name:latin` in native:
  "Thobenʼs Backwaren", "Aminʼs Barber Shop", "Café µ". Cause: U+02BC and µ are
  letters outside the Latin script, so the name is not "only Latin" and both
  sides transliterate. ICU leaves the text unchanged and Planetiler emits it;
  native's `transliterate_name` in `labels.rs` emits only a changed string.
  The fourth is a Han name romanised with different spacing (native "yī bēi
  norāmen", Planetiler "yī bēinorāmen") in `name:latin` and `name_int`: ICU4X
  and ICU4J differ.
- **Validity**: 3,540 native invalid polygons (3,265 building, 275 landuse),
  3,550 Planetiler. No native polygon is invalid where Planetiler's is valid;
  10 are the other way round.

### Pomorskie

[pomorskie-latest.osm.pbf](https://download.geofabrik.de/europe/poland/pomorskie-latest.osm.pbf)
downloaded 2026-11-03 (replication timestamp 2026-11-02T21:21:26Z), SHA-256
`e11604b8…67d6633`, bounds 16.68672 53.4806 19.66069 55.08543.

| | Wall time | Peak RSS | Archive |
|---|---|---|---|
| native | 78.1 s (22.4 s in the first run) | 0.63 GiB | 101.4 MB |
| Planetiler | 119.0 s | 2.58 GiB | 100.4 MB |

Both archives pass `verify` and address the same 21,286 tiles; 7,689 differ,
7,345 of them only by ocean IDs. Native tiles are within 0.9% of Planetiler's
at z1–13, 1.5% smaller at z0 and 1.3% larger at z14.

| Layer | Features (native / Planetiler) | Geometry differs (within the bar) | Unpaired | Attributes differ |
|---|---|---|---|---|
| water | 76,219 | 155 (155), see below | 7,572 each side | 0 |
| landcover | 394,229 | 102 (18) | 0 | 0 |
| water_name | 17,382 | 55 lines | 0 | 0 |
| building | 25,144 | 25 (24) | 0 | 0 |
| landuse | 46,473 | 13 (12) | 0 | 0 |
| transportation | 173,350 / 173,348 | 0 | 2 native only | 0 |
| transportation_name | 63,240 | 0 | 0 | 1 |
| the other 9 layers | 965,823 | 0 | 0 | 0 |

- **water**: every ocean polygon from `water-polygons-split-3857` has another
  ID. Native numbers them by record in the whole shapefile (`(index + 1) * 10`
  in `supplements.rs`, here 5530, 26440 and 51130); Planetiler numbers only
  the polygons it reads inside the bounds (10, 20, 30). Paired across IDs,
  7,417 are identical, 153 are full-tile squares that native writes with 7 or
  9 vertices (extra collinear points) against Planetiler's 5, and 2 at
  9/283/163–164 differ by under 0.05 px.
- **landcover**: z7–14. All but one of the 84 outside the bar are under 1%
  of the area and fail on distance (one is too large to measure). Native has
  more holes in 62 of the 102 and more parts in 44: small holes and pieces
  that Planetiler's merge removes. Largest: 14/8986/5261 relation 13340268 (grassland), where
  native keeps a pond's inner ring of 451,594 units² that Planetiler lacks
  (32% of the area, 2.8 px). Others, counting the parts that differ:
  13/4496/2628 id 207437110 (forest, 19 holes against 9, 77 px at 0.01%);
  12/2243/1316 id 92309970 (40 holes against 18, 34 px); 10/560/329 id 29335030 (112 parts and 210 holes against 105 and
  134, 12 px).
- **water_name**: the label line of relation 8259509 (`natural=bay`, Zatoka
  Gdańska) on 55 tiles at z13–14, with the same vertex count. One vertex
  differs: at 13/4530/2602 native has (5950, 4342), Planetiler (5858, 4289),
  106 tile units apart.
- **building**: 24 at z13 within the bar; 14/9004/5249 id 2063118220 is 0.7%
  and 8.1 px, invalid on both sides.
- **landuse**: z6–10; 10/564/326 id 131452320 is 0.06% and 2.4 px, the rest
  within the bar.
- **transportation**: native emits way 1052128904 (`highway=footway`,
  `area=yes`, `layer=-1;0`) as a polygon at 13/4520/2616 and 14/9040/5233.
  Cause: native's polygon filter reads `layer` strictly (`number` gives 0 and
  passes `layer >= 0`); Planetiler reads its numeric prefix, -1, and rejects
  it. Native's own `layer` attribute on the feature already reads -1.
- **transportation_name**: at 14/8964/5260 id 2687754222 the two hiking routes
  (PM-1521y Szlak Doliny Brdy, SŁ-162s Szlak Doliny Wieprzy i Studnicy) are
  numbered `route_1`/`route_2` in opposite order.
- **Validity**: 10,566 native invalid polygons (9,361 building, 1,201 landuse,
  4 landcover), 10,570 Planetiler. Two native polygons are invalid where
  Planetiler's is valid: landcover id 22104720 (a merged forest group led by
  relation 2210472) at 9/281/164 and 10/561/328, "Hole lies outside shell":
  the merge attaches a hole to a shell that does not contain it. 6 are the
  other way round.

## After the region, landcover and tile-edge fixes (2026-11-03)

Native after three more fixes: the region differences below, landcover
through the JTS port, and merged polygons that fill the tile become Planetiler's canonical
square, and invalid tile slices are snapped with the JTS repair, whose empty
result is kept. Same references and comparison as above.

| | Małopolskie (every tile) | Berlin (every tile) | Pomorskie (water and landcover only) |
|---|---|---|---|
| building geometry | 14 | 41 | not re-measured |
| landuse geometry | 7 | 6 | not re-measured |
| landcover geometry | 0 | 0 | 151 |
| water geometry | 0 | 0 | 2 |
| unpaired features | 0 | 0 | 0 in water and landcover |
| attributes | 0 | 4 (Han romanisation) | not re-measured |
| other geometry | 0 | 1 transportation line | not re-measured |
| native invalid where Planetiler is valid | 0 | 0 | 0 in landcover |

Validity counts are unchanged on every side. Pomorskie's full comparison
takes over 10 minutes and was not rerun; its water and landcover were
compared tile by tile with Planetiler's (landcover 2,182 differing features
after the region fixes, 1,915 after the landcover port, 151 now; water 211,
54, 2).

Accepted differences, not counted against the bar:

- **Han romanisation** (Berlin POIs): ICU4X and ICU4J space syllables
  differently; the profile cannot change that.
- **Java's `exp`, `sin` and `cos`**: HotSpot intrinsics differ from the
  platform's in the last bit for a small share of arguments; where that
  decides a rounding tie, a coordinate can differ by one unit.
- **13/4571/2774** (Małopolskie building cluster): JTS reduces it to a
  near-empty triangle; native keeps the buildings.

## Measured against the bar (2026-11-03)

Native after the fixes above and the three below, against the same Planetiler references,
every tile of all three regions. Each region was compared whole (`compare.py
--all-tiles`), and each differing pair measured (`pairdiff.py`; the one pair
too large for its distance cap was measured separately). Earlier counts
above that were taken without ring normalisation overstate the difference;
Pomorskie's "151 landcover" was such a count.

The three fixes:

- **Route order** (`transport.rs`): Planetiler's `RELATION_ORDERING` compares
  `ref` by Java string length, in UTF-16 units; native counted UTF-8 bytes,
  so "SŁ-162s" (7 units, 8 bytes) sorted after "PM-1521y". This fixes Pomorskie
  14/8964/5260.
- **Depth of a separate ring in the JTS buffer port** (`jts_buffer.rs`):
  JTS's `SubgraphDepthLocater` takes the left depth of the least stabbed
  segment by `DepthSegment.compareTo`. That order is not transitive, so
  `Collections.min` depends on the order it meets the segments in, and JTS
  can drop a hole even from a valid polygon. Native computed the true
  winding number instead. It now follows JTS.
- **Ring winding before slicing** (`render.rs`): Planetiler's
  `extractGroups` turns shells and holes counter-clockwise, and
  `reassemblePolygon` reverses the holes after slicing. JTS's edge order, and
  so the choice above, follows the winding. Native now winds rings the same
  way. Together with the depth rule this fixes Pomorskie 14/8986/5261:
  relation 13340268 loses a pond hole in Planetiler's repair of an invalid
  tile slice. It also brings several Berlin buildings and Małopolskie and
  Pomorskie landuse polygons inside the bar.

| | Małopolskie | Berlin | Pomorskie |
|---|---|---|---|
| differing tiles | 19 | 47 | 15 |
| polygon geometry differs | 19 | 44 | 15 |
| of which outside the bar | 1 (accepted, 13/4571/2774) | 1 | 0 |
| line geometry differs | 0 | 1 | 0 |
| unpaired features | 0 | 1 water polygon without ID at 5/17/10 | 0 |
| attributes differ | 0 | 2 (Han romanisation, accepted) | 0 |
| native invalid where Planetiler is valid | 0 | 0 | 0 |

Outside the bar, and not accepted:

- **Berlin 13/4402/2684**, building group 287215350: 0.17% of the area
  but 1.41 px Hausdorff distance (2,824 vertices against 2,839).
- **Berlin 5/17/10**: motorway 45367920, with the same 147 vertices, is
  0.125 px (2 units) away. An ocean polygon with no ID, unpaired by the
  tool for that reason, differs on the same tile. Both are likely
  last-bit differences in the projection maths (Java's `exp`, `sin`, `cos`),
  which the accepted differences below cover; this is not yet confirmed.

On Monaco with the real supplemental sources, native was already
geometrically identical to Planetiler in every tile. With the three fixes, the
landcover and transportation features whose encoding changed are now
byte-identical to Planetiler's, where before they were not.

## Release check: 2026-11-17

Małopolskie extract downloaded 2026-11-17, native at the 0.1.0 release code
(fast-mvt encoding, `osm-pbf-ingest` 0.1), against Planetiler 0.10.2 with the
same bounds, zooms 0–14, four threads, Linux (WSL). Compared with this
directory's harness over every tile.

| | Wall time | Peak RSS | Archive |
|---|---|---|---|
| native | 41.6 s | 0.86 GiB | 153.7 MB |
| Planetiler | 93.0 s | 2.9 GiB | 152.3 MB |

- **Tiles**: both archives pass `verify` and hold the same 11,293 tiles.
- **Features**: every one of the 16 layers has the same features, 2,846,782
  on each side. None is unpaired and no attribute differs.
- **Geometry**: 18 tiles differ, in 18 merged polygons: 14 building
  clusters at z13 and 4 landuse polygons at z6–9, the polygon merging
  described above.
- **Validity**: about 9,770 polygons on each side are self-intersecting, in
  the same places. None is invalid only in native; 5 are invalid only in
  Planetiler.

The extract is two weeks newer than the one above, so the counts are not
like for like, but against the 19 differing polygons of the last
measurement nothing has got worse.

## Verified on 0.1.3: 2026-11-19

Native at `45dee3c` (main, 0.1.3) against Planetiler 0.10.2, all three
regions, every tile, zooms 0–14, four threads, Linux x86_64 (4 cores, 15 GiB),
bounds from each extract's header. Prepared with `setup-linux.sh`, run with
`run_omt.py`, compared with `compare.py --all-tiles` and measured with
`pairdiff.py --accepted accepted.json`, which exits 0 on all three. Extracts of
2026-11-18; water polygons SHA-256 `3e5b7e91…e38c9c2`, the other two datasets
at their pinned hashes.

| | Małopolskie | Berlin | Pomorskie |
|---|---|---|---|
| extract SHA-256 | `777306c5…dd2fbc20` | `c00600eb…eafa563f` | `eaa1a9ea…a8130496` |
| tiles (same on both sides) | 11,293 | 942 | 21,286 |
| differing tiles | 18 | 47 | 15 |
| polygon geometry differs | 14 building, 4 landuse | 38 building, 6 landuse | 11 building, 4 landuse |
| of which outside the bar | 1 (accepted) | 1 (accepted) | 0 |
| line geometry differs | 0 | 1 (accepted) | 0 |
| unpaired features | 0 | 0 | 0 |
| attributes differ | 0 | 2 (one POI, accepted) | 0 |
| invalid only in native | 0 | 0 | 0 |
| invalid native / Planetiler | 9,768 / 9,773 | 3,546 / 3,556 | 10,575 / 10,579 |
| native wall time / peak RSS | 43.5 s / 0.63 GiB | 78.6 s / 0.51 GiB | 16.4 s / 0.49 GiB |
| Planetiler wall time / peak RSS | 119.5 s / 3.14 GiB | 79.2 s / 2.78 GiB | 77.7 s / 2.86 GiB |

Times are from one run on a shared machine and are noisy.

Every difference outside the bar is one already known on 2026-11-03,
unchanged, and is accepted in `benchmarks/compiler-parity/accepted.json`:

- **Małopolskie 13/4571/2774**, building cluster 2252911880: JTS's merge
  leaves less of the cluster than native (6.9% of the area, 10.5 px).
- **Berlin 13/4402/2684**, building group 287215350: 0.17% of the area,
  1.41 px (2,824 vertices against 2,839). Accepted on 2026-11-19.
- **Berlin 5/17/10**, motorway 45367920: one of 147 vertices is 2 tile units
  (0.125 px) away, a last-bit difference in the projection maths. Accepted on
  2026-11-19. The ocean polygon without ID that differed on this tile on
  2026-11-03 no longer does.
- **Berlin POI 124958633681** on 14/8804/5373–5374: Han romanisation,
  "yī bēi norāmen" against "yī bēinorāmen" in `name:latin` and `name_int`.

All other differing polygons are inside the bar. Polygons invalid in native
but valid in Planetiler are checked over every tile, not only the differing
ones.

## The bar

`parity_verified` becomes `true` when, on Malopolskie and two more regions of
different character (a dense city and a coast with large water bodies), over
every tile:

1. every layer has the same features per tile, paired by ID, with the same
   typed attributes;
2. every line and point geometry is identical;
3. every polygon either is identical or differs from Planetiler's by less than
   1% of its area and 1 pixel (of 256) Hausdorff distance;
4. no polygon is invalid where Planetiler's is valid.

Timing and size are reported but not part of the bar.

## Order of work

1. ~~IDs of merged and deduplicated features~~.
2. ~~Building merge geometry~~.
3. ~~Landuse and landcover merging~~; ~~the boundary~~.
4. ~~Two more regions~~ and ~~their profile differences~~:
   ocean IDs, `name:latin`, junction `layer`, bridge class, `layer`
   parsing, bus stop IDs, the bay label.
5. ~~Full-tile squares and tile-edge slivers~~.
6. ~~Measure the rest against the bar~~ (see "Measured against the bar"); route order, JTS depth order and ring winding fixed. Left: Berlin
   13/4402/2684 and 5/17/10.
7. ~~A full Pomorskie rerun~~ (every tile, above). ~~Then decide
   `parity_verified`~~: the two Berlin cases are accepted and the bar is met
   on 0.1.3 (see "Verified on 0.1.3").
