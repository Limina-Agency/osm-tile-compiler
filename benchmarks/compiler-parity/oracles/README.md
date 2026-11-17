# Pinned Planetiler line-merger oracle

## Bit-exact landuse buffer stages

`LanduseBitsOracle.java` replays the 87 source records in cluster 8 of
residential landuse tile `7/70/43`. It reads the original integer MVT
coordinates from `landuse-7-70-43-premerge.json` and the membership indices
from `landuse-7-70-43-java-clusters.json`. It never parses a WKT or decimal
stage dump. The output contains all positive-buffered polygons, the union,
the negative-buffer result, the final snapped polygons, and the negative
buffer's raw and noded curves. Every coordinate is `Double.doubleToRawLongBits`
of the live JTS ordinate multiplied by 16 to express it in MVT units,
formatted as 16 lowercase hexadecimal digits. Polygon, ring, curve, and
point order are preserved. The raw curves include JTS left/right location
codes and the noded edges retain their source curve index.

```powershell
$root = 'BENCH_DIR\compiler-parity-20260926\omt-opt35-polygon-capture'
$jar = 'BENCH_DIR\tools\planetiler-0.10.2.jar'
$jdk = 'BENCH_DIR\tools\java21\jdk-21.0.12.1+1\bin'
$scratch = Join-Path $root 'landuse-bits-classes'
New-Item -ItemType Directory -Force $scratch, (Join-Path $scratch 'empty-sourcepath') | Out-Null
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch benchmarks/compiler-parity/oracles/LanduseBitsOracle.java
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" LanduseBitsOracle (Join-Path $root 'landuse-7-70-43-premerge.json') (Join-Path $root 'landuse-7-70-43-java-clusters.json') 8 (Join-Path $root 'landuse-7-70-43-cluster8-ieee-bits.json')
```

The pinned JAR SHA-256 is
`f310bd0413e2e4512b27f4046d418664e8e1d3bf31603c2a70e23de06c167e4d`.
For the captured input, the premerge SHA-256 is
`033f8f7f99f5876b1711ffdc170dba1099c66304677180dcd56af793d3cdbf21`,
the cluster-index SHA-256 is
`274fe5154ea9066e15f8c2bf0b6457599d0240d12cba23dd0085755a6bd7090b`,
and the generated 198,479-byte bit trace SHA-256 is
`d24b8db632f4db60ca1112d1a32b231f6302628e135f93848b5203ed1688ea00`.
It contains 87 buffered geometries, 13 raw negative-buffer curves, and
619 noded edges. These observations describe the pinned Java implementation
for this captured cluster; they do not establish parity for other clusters.

`LanduseScaledNoderOracle.java` replays the same 13 negative-buffer raw curves
through `ScaledNoder(new SnapRoundingNoder(new PrecisionModel(1)), 1e9)`.
It decodes the IEEE-754 MVT ordinates in the trace above, divides by exactly
16 to recover tile-pixel ordinates, then gives those coordinates directly to
JTS. Its output retains source-curve and left/right side labels. Each returned
edge carries pixel-domain IEEE-754 bits and `Math.round(pixel * 1e9)` integer
grid coordinates. This is a fixed-precision noding diagnostic; the 619 edges
above came from the ordinary unscaled `MCIndexNoder` and are a different run.

```powershell
$scratch = Join-Path $root 'scaled-noder-classes'
New-Item -ItemType Directory -Force $scratch, (Join-Path $scratch 'empty-sourcepath') | Out-Null
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch benchmarks/compiler-parity/oracles/LanduseScaledNoderOracle.java
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" LanduseScaledNoderOracle (Join-Path $root 'landuse-7-70-43-cluster8-ieee-bits.json') (Join-Path $root 'landuse-7-70-43-cluster8-scaled-noder-bits.json')
```

Two runs produced byte-identical 240,000-byte output with 668 noded edges;
SHA-256:
`fb256f47bfa0e81bddf8d940ab629ec9de19d62d38bf6cc0b8efbae3f32a5653`.
The output records the source trace hash and scaling contract for downstream
bitwise comparison. The Java wrapper source SHA-256 is
`c93af256d400eee1e395a8e1a2c5858482a7e55f0e25182f5f0504c100fe1817`.

## House-number partition iteration

`HousenumberHashOrderOracle.java` runs the pinned profile's default
`Collectors.groupingBy` operation on 256 actual partition strings extracted
from address nodes in and around tile `14/9065/5555` of the benchmark
Malopolskie PBF. The fixture has 253 distinct partitions, including Polish
non-ASCII text. These are source partitions for an iteration-order oracle,
not a claim that all 256 nodes produce features in that one tile. The Java
source uses `HashMap.computeIfAbsent` and then iterates `.values()`; the
matching Rust helper is `omt/java_hashmap.rs`.
The same two text fixtures are copied into
`osm-tile-compiler/tests/fixtures` so the Rust source
snapshot can run its tests without the benchmark harness directory.

```powershell
$jdk = 'BENCH_DIR\tools\java21\jdk-21.0.12.1+1\bin'
$scratch = Join-Path $env:TEMP 'osm-tile-house-hash-oracle'
New-Item -ItemType Directory -Force $scratch | Out-Null
$source = (Resolve-Path 'benchmarks/compiler-parity/oracles/HousenumberHashOrderOracle.java').Path
$fixture = (Resolve-Path 'benchmarks/compiler-parity/oracles/housenumber-partitions-256.txt').Path
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath $scratch -d $scratch $source
& (Join-Path $jdk 'java.exe') -cp $scratch HousenumberHashOrderOracle $fixture
```

The Java and Rust helper both return the same comma-separated sequence of
first-occurrence indices for this fixture. The complete output and standalone
Rust test execution are saved externally at
`BENCH_DIR\compiler-parity-20260926\omt-opt33\hashmap-oracle`.
Matching this partition iteration alone cannot guarantee merged feature IDs:
the upstream feature encounter order and value-byte tie ordering must also
match.

### Tree bins

A `HashMap` bin that reaches eight entries in a table of 64 or more becomes a
red-black tree, and iteration then follows a linked order that depends on the
tree's shape. The house-number partitions of tile `14/9073/5550` (1,515
partitions) produce one, and the Rust helper now mirrors JDK 21's `TreeNode`
insertion, splitting and untreeifying. `JavaHashMapOrderOracle.java` prints
the iteration order for the four ways the profile and engine fill a map:

| Mode | Java | Rust |
|---|---|---|
| `grouping-string` | `Collectors.groupingBy` over strings (Housenumber) | `grouping_by_partition_order` |
| `grouping-long` | `Collectors.groupingBy` over longs (Boundary mode) | `grouping_by_long_order` |
| `put-sized-string` | `HashMap.newHashMap(n)` and `put` (PBF tags) | `new_hash_map_put_order` |
| `hashset-long` | `new HashSet<Long>()` and `add` (Boundary regions) | `hash_set_long_order` |

The `hashmap-tree-*.txt` fixtures are synthetic keys that force tree bins:
strings and longs with equal hashes, and strings sharing low hash bits that
later resizes split apart. Their `*.expected.txt` files are the oracle's
output, copied with the inputs into `osm-tile-compiler/tests/fixtures`.
The Rust helper also matched the oracle on the partition lists of the 79
Malopolskie tiles with more than 1,000 house numbers.

```sh
jdk=~/osm-tile-benchmarks/tools/jdk21/bin
scratch=$(mktemp -d)
"$jdk/javac" -proc:none -d "$scratch" benchmarks/compiler-parity/oracles/JavaHashMapOrderOracle.java
cd benchmarks/compiler-parity/oracles
"$jdk/java" -cp "$scratch" JavaHashMapOrderOracle grouping-string hashmap-tree-strings.txt
"$jdk/java" -cp "$scratch" JavaHashMapOrderOracle put-sized-string hashmap-tree-distinct-strings.txt
"$jdk/java" -cp "$scratch" JavaHashMapOrderOracle grouping-long hashmap-tree-longs.txt
"$jdk/java" -cp "$scratch" JavaHashMapOrderOracle hashset-long hashmap-tree-longs.txt
```

`LoopLineMergerOracle.java` is our small wrapper around the unmodified
`com.onthegomap.planetiler.util.LoopLineMerger` in Planetiler 0.10.2. It checks
the actual transportation path from OSM way `176240791` (feature ID
`1762407912`) at tile `14/9065/5555`, two coordinate rounding ties, and a
length threshold on a simplified line. Coordinates printed by the wrapper are
in MVT grid units (tile-pixel coordinates multiplied by 16).

The pinned JAR is `BENCH_DIR\tools\planetiler-0.10.2.jar`.
Its SHA-256 is
`f310bd0413e2e4512b27f4046d418664e8e1d3bf31603c2a70e23de06c167e4d`.
Use Java 21. The example PowerShell commands compile only this wrapper; the
empty source path and `-proc:none` prevent javac from compiling the source
files packaged in the JAR or running its annotation processors:

```powershell
$jar = 'BENCH_DIR\tools\planetiler-0.10.2.jar'
$jdk = 'BENCH_DIR\tools\java21\jdk-21.0.12.1+1\bin'
$source = (Resolve-Path 'Tools\benchmarks\compiler-parity\oracles\LoopLineMergerOracle.java').Path
$scratch = Join-Path $env:TEMP 'osm-tile-loop-line-merger-oracle'
New-Item -ItemType Directory -Force $scratch, (Join-Path $scratch 'empty-sourcepath') | Out-Null
(Get-FileHash $jar -Algorithm SHA256).Hash.ToLowerInvariant()
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch $source
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" LoopLineMergerOracle
```

Expected output:

```text
transportation1762407912 [[4160,-1], [4120,20], [4075,41], [4049,57], [4045,65], [4045,74], [4056,88], [4074,89], [4088,78], [4101,58], [4120,20]]
half_tie_positive [[1,0], [1,16]]
half_tie_negative [[0,0], [0,16]]
length_min_20_05 [[0,0], [20,0]]
length_min_20_2 count=0
```

The path oracle removes exactly `[4061,49]` at tolerance `1/16` tile pixel
(one MVT grid unit).
The half-tie cases show that `+0.03125` rounds to `+0.0625` and `-0.03125`
rounds to zero at precision 16. The length probe has input length
`20.09975124224178` in scaled units and simplifies to length 20. Keeping it
at a 20.05 threshold and dropping it at 20.2 demonstrates that this merger
checks the original stored edge length before simplification for this case.

The wrapper, not Planetiler's source code, is tracked here. The initially
executed oracle output and JAR hash are recorded in the external benchmark
evidence at `BENCH_DIR\compiler-parity-20260926\loop-line-merger-oracle\evidence.json`.

## Polygon repair oracle

`PolygonRepairOracle.java` replays way 1525089255 from the tracked
`osm-tile-compiler/tests/fixtures/omt-meadow-1525089255.csv`.
Use the same pinned JAR, Java 21 and empty source path above:

```powershell
$source = (Resolve-Path 'benchmarks/compiler-parity/oracles/PolygonRepairOracle.java').Path
$fixture = (Resolve-Path 'osm-tile-compiler/tests/fixtures/omt-meadow-1525089255.csv').Path
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch $source
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" PolygonRepairOracle $fixture
```

The simplified tile polygon is invalid. Direct precision reduction throws
`IllegalArgumentException`; floating `buffer(0)` followed by precision reduction
produces one polygon with one hole and area **5474.5** MVT square units. Exact
rings are recorded in `omt-float-repair-meadow-1525089255.json` beside the CSV.
This oracle qualifies this repair case; it does not establish general equivalence
between the Rust buffer implementation and JTS.

The second fixture, `omt-farmland-724352898.csv`, accepts explicit tile arguments:

```powershell
$fixture = (Resolve-Path 'osm-tile-compiler/tests/fixtures/omt-farmland-724352898.csv').Path
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" PolygonRepairOracle $fixture 9 284 173
```

Expected area is **1252.5**, with one exterior and no holes. This ring exposes
orientation differences: `geo::Buffer(0)` returns empty; signed-area-selected
positive fill matches JTS. Nonzero fill retains an extra detached triangle.
The exact expected ring is in `omt-float-repair-farmland-724352898.json`.

## Polygon merge area-order oracle

`PolygonAreaOrderOracle.java` checks the pinned `FeatureMerge` ordering:
`GeoUtils.snapAndFixPolygon` runs before `extractPolygons` applies the strict
minimum-area comparison. Use the same pinned JAR and Java 21 setup above:

```powershell
$source = (Resolve-Path 'benchmarks/compiler-parity/oracles/PolygonAreaOrderOracle.java').Path
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch $source
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" PolygonAreaOrderOracle
```

The rectangle's floating area exceeds one tile pixel squared, while snapping
its coordinates to the MVT grid makes its area exactly one. The output is:

```text
float_pixels_squared=1.001250390625
snapped_pixels_squared=1.0
float_passes_min1=true
snapped_passes_min1=false
```

Filtering before snapping therefore retains a component Planetiler drops.
This isolated probe establishes the threshold ordering, not the cause of any
particular regional polygon difference.

## Captured landuse postmerge oracle

`LanduseMergeOracle.java` feeds the 257 ordered, native-rendered polygon
features in `omt-landuse-202730640-premerge.json` through the pinned
`FeatureMerge.mergeNearbyPolygons` implementation. The fixture was captured
before polygon merge from tile `12/2273/1391` in the Małopolskie regional
input; coordinates are integer MVT grid units (16 per tile pixel). The wrapper
uses the exact pinned Landuse zoom-12 parameters: minimum shell/hole area one
pixel squared, proximity 0.1 pixel, and buffer 0.1 pixel.

```powershell
$source = (Resolve-Path 'benchmarks/compiler-parity/oracles/LanduseMergeOracle.java').Path
$fixture = (Resolve-Path 'osm-tile-compiler/tests/fixtures/omt-landuse-202730640-premerge.json').Path
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch $source
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" LanduseMergeOracle $fixture
```

The pinned merge emits one feature, ID `202730640`, with 205 polygon
components, 2023 JTS points, and area `4754.98828125` tile pixels squared.
The proximity stage forms 213 clusters, 185 of which are singletons. Native
proximity grouping on the same 257 records produces those exact counts.
This is a **same-input postmerge** oracle. It does not assert that Planetiler's
own premerge inputs match the native fixture. In the sampled reference archive,
however, the oracle geometry is *exactly equal* to Planetiler's feature with
this ID: zero symmetric difference and zero Hausdorff distance (205 polygon
components, two holes, 2005 exterior points, area `1,217,277` MVT square units).
The native opt32 and opt33 outputs both have 2536 exterior points and area
`1,217,088.5` MVT square units; their symmetric difference from the oracle is
`2,529.357122` MVT square units. This isolates this feature's discrepancy to
the native postmerge operation on the captured inputs. A per-cluster replay
narrows it further: Java and native form the same 213 member sets; all 185
singleton outputs match exactly, while 27 of 28 multi-member outputs differ.
For the largest two-member cluster (source record indices 0 and 251), the
buffered/unioned/contracted geometries differ by only 0.0051/0.0067/0.0055
MVT square units before snapping. Their respective naive integer rounding
produces the different final geometries. This attributes the discrepancy to
tiny floating-point differences in the buffer/union backend crossing MVT
half-grid thresholds; it does not justify applying a generic rounding epsilon.
`geo 0.33.1` delegates polygon buffering to `i_overlay 4.5.2`, whose
`FloatPointAdapter` converts offsets to an internal i32 grid before returning
f64 coordinates. On this fixture, the largest source polygon spans 1474 MVT
units in x; its internal grid is about `2^-20` MVT units. The JTS and native
contracted floats differ by only about `3.4e-6` MVT units in Hausdorff
distance, yet rounding them independently produces the observed 838.59 MVT²
symmetric difference for the largest two-member cluster. A precision-preserving
Rust backend would have to retain raw offset vertices and reconstruct output
intersection coordinates through the overlay, or implement full-precision
buffer/overlay operations; simply increasing the i32 scale can overflow.

## Clipped farmland snap-rounding

`PolygonSnapRoundOracle.java` runs the pinned `GeoUtils.snapAndFixPolygon`
on the captured float shell in each `omt-farmland-stripe-*.json` fixture.
Coordinates in the fixture are MVT units; the wrapper converts to tile pixels
before invoking Java, and prints integer MVT coordinates afterward.

```powershell
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch benchmarks/compiler-parity/oracles/PolygonSnapRoundOracle.java
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" PolygonSnapRoundOracle osm-tile-compiler/tests/fixtures/omt-farmland-stripe-780650215.json
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" PolygonSnapRoundOracle osm-tile-compiler/tests/fixtures/omt-farmland-stripe-1375094601.json
```

Both outputs match the archived reference rings up to cyclic start and winding.
The first float shell is valid but becomes invalid after naive rounding;
snap-rounding keeps the hot-pixel vertex `(-64,758)`. The second float shell
is invalid and first requires signed-area polygon repair; a small component
that later collapses supplies the hot pixel `(2462,4159)` on the surviving
component's edge. Rust end-to-end source-feature fixtures check both rings.

## Float repair vertex provenance

`FloatRepairProvenanceOracle.java` feeds an asymmetric self-crossing shell to
the pinned JTS `buffer(0)` path. Its original near-half-grid vertex stays at
`(0.4999999,0)` MVT units, which snaps to `(0,0)`; an i32 float adapter can
otherwise return a perturbed value on the other side of the half-grid tie.
The generated intersection is approximately `(2025.4383188566,1974.5616811434)`
and snaps to `(2025,1975)`. The native bounded repair restores only a unique
original source vertex at the same adapter key; generated intersections keep
the overlay's value. This oracle verifies the source coordinate expectation,
not complete JTS/i_overlay topology equivalence.

```powershell
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch benchmarks/compiler-parity/oracles/FloatRepairProvenanceOracle.java
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" FloatRepairProvenanceOracle
```

## Landcover generated-hole spur

`LandcoverStageOracle.java` replays OSM way 748863343 through the pinned
node-store projection, Douglas–Peucker simplification, tile slicing, and
`snapAndFixPolygon` for z9/284/173. The source and complete reference rings
are in `osm-tile-compiler/tests/fixtures/omt-landcover-way748863343.json`.
The pinned Landcover profile sets a 0.25-pixel tolerance below z10, a 2-pixel
minimum size at z9, and a 4-pixel buffer; the regional maximum zoom is 14.

```powershell
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch benchmarks/compiler-parity/oracles/LandcoverStageOracle.java
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" com.onthegomap.planetiler.render.LandcoverStageOracle osm-tile-compiler/tests/fixtures/omt-landcover-way748863343.json
```

The Java result has one polygon with a triangular hole at
`(1950,1433), (1961,1447), (1950,1435)` and exactly matches the reference
archive topologically (area 1414.5 MVT units²). The opt36 native hole also
contains `(1939,1450)`, yielding area 1403.5 and symmetric difference 11.
The oracle isolates this mismatch to source rendering before landcover
postprocessing; it does not establish the exact substage responsible.

## Valid float polygons with holes that fail after snapping

`HoleSnapOracle.java` runs three small valid float polygons through the pinned
`GeoUtils.snapAndFixPolygon` with MVT grid precision. Input and exact expected
rings are in `osm-tile-compiler/tests/fixtures/omt-valid-hole-snap.json`.

```powershell
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch benchmarks/compiler-parity/oracles/HoleSnapOracle.java
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" HoleSnapOracle osm-tile-compiler/tests/fixtures/omt-valid-hole-snap.json
```

One hole touches the outer shell after snapping and becomes a notch. A thin
valid hole collapses to zero area. The combined case shows why collapsed hole
vertices must remain available during shared hot-pixel noding: JTS removes
the tiny hole but retains its `(0,8)` hot pixel on the repaired exterior.

## Actual float-invalid landcover relation with holes

`RelationLandcoverStageOracle.java` replays OSM relation 20659308 and its five
ordered closed member ways from the Malopolskie PBF. The checked-in fixture
`osm-tile-compiler/tests/fixtures/omt-landcover-relation-20659308.json`
contains the source nodes, clipped float shell and hole, pinned Java result,
and native and reference archive rings at z9/284/173. It records the source
PBF and Planetiler JAR hashes. The Java result is topologically equal to the
reference archive; opt37 native differs by 3 MVT square units. The source
polygon is valid, but the clipped float geometry self-intersects.

```powershell
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch benchmarks/compiler-parity/oracles/RelationLandcoverStageOracle.java
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" com.onthegomap.planetiler.render.RelationLandcoverStageOracle osm-tile-compiler/tests/fixtures/omt-landcover-relation-20659308.json
```

## Actual multipolygon relation assembly and ring nesting

`RelationAssemblyOracle.java` uses pinned `OsmMultipolygon.build` on ordered
source member ways and their original node IDs, then runs projection,
simplification, tile clipping and polygon snapping. For relation 19960721 at
z13/4535/2772, Java assembles two components: one 86-point shell with nine
holes and one 28-point shell. Its snapped tile geometry has area 344,037.5
MVT units² and zero symmetric difference from the reference archive. The
opt39 native geometry has area 400,000.5 and differs by 56,002.32 MVT units².
`omt-landcover-relation-19960721.json` contains the eleven ordered closed
members and Java expected component/hole counts. Several inner rings start on
their parent's boundary; classifying by the first vertex misidentifies them
as shells. Planetiler sorts rings by projected area and tests containment of
the entire projected polygon. The native ingest regression test checks this
assembly outcome without adding a JVM dependency to production.

```powershell
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch benchmarks/compiler-parity/oracles/RelationAssemblyOracle.java
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" com.onthegomap.planetiler.render.RelationAssemblyOracle BENCH_DIR/compiler-parity-20260926/omt-opt39/relation19960721-source.json
```

The second residual relation, 1438482, has 462 mostly fragmented member
ways. Direct-member assembly in this standalone oracle still differs from
the reference tile, so it is not used as an exact output fixture.

## Buffer-only landcover fragments

`LandcoverStageOracle.java` accepts optional `zoom`, `x`, `y`,
`tolerance_pixels`, and `min_size_pixels` fields alongside `coordinates_lonlat`.
Omitted fields preserve the original z9/284/173 fixture configuration. Compile
with `-sourcepath` pointing to an empty directory so dependencies come from the
pinned JAR rather than its bundled Java source files.

`omt-landcover-buffer-fragments.json` under the Rust compiler's test fixtures
records two further source ways and their pinned stage outputs. Pass each
case's `source` object as a standalone JSON input. Ways 246762912 at z13/4550/2764
and 1495796299 at z14/9149/5576 survive float clipping, but the pinned grid repair
returns empty geometry. Native output consists of tiny triangles outside the
visible tile at the x=-64 buffer edge. These are precision diagnostics, not
evidence for introducing a post-clip minimum-area filter: upstream applies its
minimum-size filter before clipping.

## JTS buffer curve noding

`BufferNodingOracle.java` inspects the pinned JTS buffer stages on an input
union group. The input JSON is an array of polygons with `exterior` and
`holes`, each ring a list of `{ "x": number, "y": number }` MVT coordinates.
Pass the buffer distance in MVT units as the second argument and an output
JSON path as the third. Output contains `raw_curves` and `noded_edges`,
including source-curve and left/right JTS location codes. The captured
eighth landuse union cluster at erosion distance `-4` is saved externally at
`BENCH_DIR\compiler-parity-20260926\omt-opt39\cluster8-java-noded-erosion.json`.

```powershell
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath (Join-Path $scratch 'empty-sourcepath') -cp $jar -d $scratch benchmarks/compiler-parity/oracles/BufferNodingOracle.java
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" BufferNodingOracle 'path/to/union-groups.json' -4 'path/to/output.json'
```

## Small building buffer divergence

`BuildingBufferStageOracle.java` replays the four ordered z13 building records in
`../fixtures/building-z13-4571-2774-four-records.json` through the pinned
Planetiler proximity merge and records positive buffer, union, negative buffer,
and snapped geometry. It accepts only nonempty z13 building records with empty
attributes that form one cluster. The expected output is
`../fixtures/building-z13-4571-2774-java-stages.json`; corresponding native
stage coordinates and source/archive hashes are in the adjacent native-stages
and provenance JSON files.

```powershell
$jar = 'BENCH_DIR\tools\planetiler-0.10.2.jar'
$jdk = 'BENCH_DIR\tools\java21\jdk-21.0.12.1+1\bin'
$scratch = Join-Path $env:TEMP 'osm-tile-building-oracle'
New-Item -ItemType Directory -Force $scratch | Out-Null
& (Join-Path $jdk 'javac.exe') -proc:none -sourcepath '' -cp $jar -d $scratch benchmarks/compiler-parity/oracles/BuildingBufferStageOracle.java
& (Join-Path $jdk 'java.exe') -cp "$scratch;$jar" BuildingBufferStageOracle benchmarks/compiler-parity/fixtures/building-z13-4571-2774-four-records.json (Join-Path $scratch 'building-stages.json')
```

The pinned JTS negative buffer collapses the positive union to a microscopic
triangle, and snapping removes it. The native negative buffer preserves about
47.2 screen pixels². The input buildings are real OSM features in both engines;
the fixture documents precision-sensitive topology rather than a reason to
discard native coverage.

## Merged clusters against the pinned FeatureMerge

`MergeClustersOracle.java` replays each case of
`osm-tile-compiler/tests/fixtures/omt-merge-jts-clusters.json` through
the pinned `FeatureMerge.mergeNearbyPolygons` with the case's parameters and
checks that the result equals the case's `expected` polygons (integer MVT
units). The records are native premerge records of one proximity cluster from
the Małopolskie extract; on the whole extract the same Java replay of native
premerge records reproduces the reference archive's z13 buildings (1,528 of
1,529 merged features). The Rust test `merged_clusters_match_pinned_feature_merge`
checks the native merge against the same expected polygons.

```sh
jar=~/osm-tile-benchmarks/tools/planetiler-0.10.2.jar
jdk=~/osm-tile-benchmarks/tools/jdk21/bin
scratch=$(mktemp -d); mkdir -p "$scratch/empty"
"$jdk/javac" -proc:none -sourcepath "$scratch/empty" -cp "$jar" -d "$scratch" benchmarks/compiler-parity/oracles/MergeClustersOracle.java
"$jdk/java" -cp "$scratch:$jar" MergeClustersOracle osm-tile-compiler/tests/fixtures/omt-merge-jts-clusters.json
```

Each case isolates one JTS 1.20 behaviour the native port in
`omt/jts_buffer.rs` follows:

- `building-union-ring-start`: the union's ring starts on a nearly collinear
  vertex, which `BufferInputLineSimplifier` never deletes, so it survives the
  erosion.
- `building-reduced-precision-erosion`: the full-precision erosion throws a
  `TopologyException`, and `BufferOp` retries on a 1e-10 pixel grid with
  `ScaledNoder(SnapRoundingNoder)`.
- `building-precision-reducer-touching-hole`: pointwise rounding makes the
  erosion invalid, so `snapAndFixPolygon` uses `GeometryPrecisionReducer`, and
  the snapped hole touches its shell and stays a hole (then falls below the
  minimum hole area).
- `landuse-edge-end-replacement`: two edge ends leave a node in the same
  direction; `EdgeEndStar`'s TreeMap keeps only the later one, depths no longer
  agree, and the erosion is retried at reduced precision.
- `landuse-reducer-input-repair`: the erosion leaves a hole outside its shell,
  touching it at one vertex. Pointwise rounding is invalid and so is the
  unrounded erosion, so `snapAndFixPolygon` first repairs it with a zero
  buffer (`GeoUtils.fixPolygon`), which drops the hole, and only then runs
  `GeometryPrecisionReducer`.
- `landuse-collapsed-vertex-node`: on the 1e-9 pixel grid of the
  reduced-precision erosion, a curve snaps to a vertex between two equal
  nodes. `SegmentNodeList.addCollapsedNodes` makes that vertex a node too, so
  the curve's doubling back becomes two opposite edges and the depths agree;
  without it the erosion would be retried on a coarser grid.
- `landuse-limited-mitre-bevel`: an acute corner takes
  `addLimitedMitreJoin`, whose bevel midpoint is
  `project(corner, -mitreLimit * distance, bisector)` through
  `Angle.cosSnap` and `sinSnap`, with Java's `Math.atan2` (fdlibm) and
  `Math.sin`/`Math.cos`, which round differently from the platform's in the
  last place.
- `landcover-zero-buffer`: `Landcover.postProcess` merges through
  `mergeOverlappingPolygons`, a zero-distance `BufferOp` of each cluster's
  collection. Pointwise rounding collapses a sliver hole of that buffer to
  three collinear points; JTS's `isValid` rejects the ring, so the precision
  reducer runs and the hole disappears.
