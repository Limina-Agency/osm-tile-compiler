# OMT line-stroke oracle

`omt-line-stroke-boundary-399309380.json` records three actual decoded arms at the junction `(3168,3205)` in boundary feature 399309380, tile z5/17/10. The native opt32 tile leaves those arms separate; the pinned Planetiler tile joins the spur ending at `(3168,3205)` to the eastward arm. The full tile has other path differences; this fixture isolates one demonstrated stroke operation and does not claim full feature equality.

Pinned source: Planetiler 0.10.2 `FeatureMerge.java` calls `LoopLineMerger.setMergeStrokes(true)`; `LoopLineMerger.java` selects descending angle pairs at each node in `strokeMerge` (lines 201–227) and preserves an input edge's direction in `mergeTwoEdges` (lines 181–198). The pinned JAR SHA-256 is `f310bd0413e2e4512b27f4046d418664e8e1d3bf31603c2a70e23de06c167e4d`. Opt32 native and reference archive hashes are in the JSON fixture.

With Java 21 installed, reproduce the two oracle cases using the pinned JAR:

```powershell
java --class-path BENCH_DIR/tools/planetiler-0.10.2.jar osm-tile-compiler/tests/fixtures/LineMergeStrokeOracle.java
```

The wrapper prints the joined actual boundary spur/east arm and the remaining south arm. Its second case checks that a shared interior point is noded before straight horizontal and vertical arms are paired. The Rust helper implements only noding, exact duplicate removal, and stroke pairing. It leaves closed-loop junctions unchanged; it does not port Planetiler's loop-breaking or short-stub removal stages.
