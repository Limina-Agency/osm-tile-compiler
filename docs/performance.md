# Performance

Measurements of the compiler's time, memory and disk use. The comparison with
Planetiler is in `parity.md`.

## Memory

Peak resident memory of a whole process (`ru_maxrss`), from two runs each
on 2026-11-04 on a Linux x86-64 laptop (mimalloc), with the small synthetic
supplemental datasets of the tests rather than the real 1.5 GB:

| Compile | 2 threads | 4 threads |
|---|---|---|
| Monaco (`fixtures/monaco.osm.pbf`, 0.4 MB), command line | 82–85 MiB | 98–114 MiB |
| Monaco, `compile_basemap` (`examples/compile_basemap.rs`) | 91 MiB | 101–105 MiB |

Most of that is fixed: per-thread sort buffers (8 MiB each), queues and
decoder state. Building levels read the basemap a tile at a time (see
"Building levels" below).

## Cost in an app's process

Measured once on 2026-11-04 on a laptop: AMD Ryzen 7 5800H (8 cores, 16
threads), 15 GiB of RAM, Linux, NVMe (btrfs), load under 3 at the start of
every run. One compile per process, through an app's compile host, which
calls `compile_basemap` with building levels on, the system allocator (no
mimalloc) and the real supplemental datasets, already unpacked and in the
page cache. These are Linux numbers; Apple silicon follows below.

| Extract | Threads | Time | Peak RSS | Peak anonymous | Peak scratch disk | Basemap + building levels |
|---|---|---|---|---|---|---|
| Monaco (`fixtures/monaco.osm.pbf`, 0.4 MiB) | 2 | 0.5 s | 72 MiB | – | under 1 MiB | 0.27 + 0.06 MiB |
| Monaco | 4 | 0.5 s | 81 MiB | – | under 1 MiB | 0.27 + 0.06 MiB |
| Monaco | 16 | 0.5 s | 86 MiB | – | under 1 MiB | 0.27 + 0.06 MiB |
| Monaco, no supplemental datasets | 2 | 0.2 s | 39 MiB | – | under 1 MiB | 0.24 + 0.06 MiB |
| Małopolskie (Geofabrik, 194 MiB) | 2 | 60.7 s | 713 MiB | 546 MiB | 1050 MiB | 146.5 + 26.3 MiB |
| Małopolskie | 4 | 34.0 s | 841 MiB | 799 MiB | 1038 MiB | |
| Małopolskie | 16 | 21.7 s | 1285 MiB | 1174 MiB | 1050 MiB | |
| Małopolskie, no supplemental datasets | 16 | 22.4 s | 1297 MiB | 1121 MiB | 1040 MiB | 146.3 + 26.3 MiB |

- Peak RSS is `ru_maxrss`; about 380 MiB of it is file-backed (the
  memory-mapped extract and the program), which a phone can drop. Peak
  anonymous is `RssAnon` sampled every 0.5 s, nearer what counts against an
  iPhone's memory limit (– where the run was too short to sample).
- Scratch disk is the whole staging directory, sampled every 0.5 s: the
  scratch and the archive as it is written. In 0.1.2 it peaks in
  `write_tiles`, at 5.4 times the extract; see Scratch disk below for what
  changed since.
- Stages with 2 threads: `index_osm` 2.0 s, `read_osm` 29.4 s, `write_tiles`
  24.1 s, `building_levels` 2.4 s, and 2.6 s before them (starting, the
  supplemental datasets). With 16: 1.9, 8.8, 9.3 and 1.5 s.
- `building_levels` is the compile's memory peak (546 MiB with 2 threads,
  against 333 MiB in `read_osm`; 799 against 462 with 4, 1174 against 905
  with 16).
- The supplemental datasets, once unpacked and cached, cost an inland
  region almost nothing in time or memory; they cost disk: 1.45 GB of
  downloads unpacked into another 2.23 GB.

## Scratch disk

Measured on 2026-11-19 on a cloud VM: 4 vCPUs (Intel Xeon, 2.1 GHz), 15 GiB
of RAM, Linux, ext4. Command line (mimalloc), `--omit-sources all`,
Małopolskie (Geofabrik, 2026-11-18, 194 MiB). Each figure is the median of
three runs, interleaved so that every build saw the same machine. Peak RSS is
`ru_maxrss`, peak anonymous is `RssAnon`, and scratch disk is the output's
directory (scratch and partial archive), both sampled every 0.25 s.

| Build | Threads | Time | Peak RSS | Peak anonymous | Peak scratch disk |
|---|---|---|---|---|---|
| 0.1.2 | 2 | 51.4 s | 750 MiB | 371 MiB | 1049 MiB (5.4×) |
| Node store deleted after `read_osm` | 2 | 51.4 s | 759 MiB | 379 MiB | 898 MiB (4.6×) |
| And zstd sort runs | 2 | 52.1 s | 751 MiB | 371 MiB | 590 MiB (3.0×) |
| 0.1.2 | 4 | 28.4 s | 872 MiB | 491 MiB | 1047 MiB (5.4×) |
| Node store deleted after `read_osm` | 4 | 28.4 s | 867 MiB | 487 MiB | 890 MiB (4.6×) |
| And zstd sort runs | 4 | 28.6 s | 876 MiB | 496 MiB | 581 MiB (3.0×) |

- Every run wrote the same archive, byte for byte (146 MiB).
- What fills the scratch directory, at 4 threads: the node store, 370 MiB,
  from `index_osm` to the end of `read_osm`; the sort runs, 534 MiB raw or
  about 220 MiB compressed, from `read_osm` to the end of `write_tiles`; and
  the archive, written in `write_tiles`. 0.1.2 kept the node store to the
  end, so all three met in `write_tiles`.
- The peak is now at the end of `read_osm`: the node store and the
  compressed runs. Smaller still needs a smaller node store, which lives in
  `osm-pbf-ingest`.
- The runs compress 2.4 times at zstd level 1; level 3 gets 2.5 and 1 MiB
  blocks no more than 64 KiB ones, so the fastest level and small blocks
  (64 KiB of memory for each run a merge has open) stay.
- 16 threads were not measured here.

### Columnar sort runs

Measured on 2026-11-19 on the same kind of cloud VM (4 vCPUs, 15 GiB, ext4),
command line (mimalloc), `--omit-sources all`, Małopolskie (2026-11-18,
194 MiB), this time with the packed node store of `osm-pbf-ingest` 0.3. Each
figure is the median of three runs, interleaved; scratch disk is the output's
directory, sampled every 0.1 s.

| Build | Threads | Time | Peak scratch disk | Sort runs |
|---|---|---|---|---|
| 0.1.3 | 2 | 52.8 s | 369 MiB (2.0×) | 225 MiB |
| Columnar sort runs | 2 | 52.5 s | 312 MiB (1.7×) | 168 MiB |
| 0.1.3 | 4 | 29.3 s | 369 MiB (2.0×) | 225 MiB |
| Columnar sort runs | 4 | 29.9 s | 312 MiB (1.7×) | 168 MiB |

- Every run wrote the same archive, byte for byte (146 MiB).
- 0.1.3 already peaked at 2.0 times the extract, not 3.0: the node store is
  146 MiB, not 370. The peak is at the end of `read_osm` (node store and
  runs) and, about as high, at the end of `write_tiles` (runs and archive).
- The runs hold 535 MiB of records as frames. zstd on the frame stream made
  that 225 MiB (2.4×); the columnar blocks make it 168 MiB (3.2×). A block
  spreads its records over seven streams, each its own zstd frame: sort keys
  and insertion sequences as deltas from the previous record, then the
  payload's fixed fields, counts, coordinates, attribute keys and attribute
  values. A polygon ring's last point, always its first again, is left out.
- What is left is mostly coordinates, which zstd barely shrinks (138 MiB of
  varints to 118 MiB, before closing points were left out). Splitting x from y, or low from high bytes, saved under 2%;
  256 KiB blocks save 2.5% and cost each open reader four times the memory.

## Building levels

Measured on 2026-11-19 on the cloud VM of "Scratch disk" (4 vCPUs, 15 GiB),
command line (mimalloc), `building-levels` on the Małopolskie basemap above
(146 MiB). Each figure is the median of three runs, interleaved. Both builds
wrote the same archive, byte for byte, and the same report.

| Build | Threads | Time | Peak RSS |
|---|---|---|---|
| 0.1.3 | 1 | 5.87 s | 310 MiB |
| Tile at a time, grids by quadrant | 1 | 4.93 s | 88 MiB |
| 0.1.3 | 2 | 3.24 s | 400 MiB |
| Tile at a time, grids by quadrant | 2 | 2.81 s | 132 MiB |
| 0.1.3 | 4 | 1.57 s | 541 MiB |
| Tile at a time, grids by quadrant | 4 | 1.29 s | 195 MiB |
| 0.1.3 | 16 | 1.70 s | 1179 MiB |
| Tile at a time, grids by quadrant | 16 | 1.44 s | 544 MiB |

- 0.1.3 read the whole basemap into memory, and each thread held every grid
  of its z11 tile at once: sixteen z13, four z12 and one z11, 2.5 MiB each.
  Now tiles are read one at a time, a z11 tile's z14 tiles go quadrant by
  quadrant, so one grid a zoom is open, and finished grids are filled again
  rather than freed. The gzipped tiles, held until the archive is written,
  no longer keep the encoder's spare room.
- What is left grows with threads mostly through mimalloc keeping freed
  memory for a while: with the system allocator the same pass peaks at
  74 MiB on 4 threads and 206 MiB on 16.
- In `compile --building-levels` on 4 threads, peak anonymous memory went
  from 537 to 469 MiB (two runs each), and building levels are no longer
  the compile's peak.

## On Apple silicon

Measured on 2026-11-17 on an Apple M5 (10 cores, 16 GB, macOS 27.2.1), in
a throwaway SwiftUI app on the iOS 27 simulator (iPhone 18 Pro), which runs
on the Mac's CPU. The app calls `compile_basemap` through a static library
(system allocator) on 4 threads, with the extract and the supplemental
datasets in its Documents folder. Małopolskie is Geofabrik's extract of
2026-11-02 (194 MiB). Peak RSS is the whole app's, SwiftUI included.

| Run | Time | Peak RSS | Basemap + building levels |
|---|---|---|---|
| Datasets and building levels, first run (extracts the datasets) | 26.7 s | 890 MiB | 146.3 + 26.3 MiB |
| Datasets and building levels, datasets already extracted | 20.5 s | 871 MiB | 146.3 + 26.3 MiB |
| No supplemental datasets, no building levels | 20.9 s | 845 MiB | 146.1 MiB |
| macOS command line (mimalloc), as the second run | 16.1 s | 783 MiB | the same bytes as the simulator's |

- Stages of the second run: `read_sources` 0.9 s (8.7 s in the first,
  extracting), `index_osm` 0.6 s, `read_osm` 8.8 s, `write_tiles` 8.9 s,
  `building_levels` 1.1 s.
- Every archive passed `verify`'s checks, and the scratch directory was
  empty afterwards.
- Monaco (`fixtures/monaco.osm.pbf`, no supplemental datasets) took 0.13 s
  in the simulator, again byte-identical to macOS.
- A phone has not been measured: its CPU is slower than the Mac's and its
  memory limit stricter.

## Cancelling

Cancelling Małopolskie on 4 threads with `examples/compile_basemap.rs`
(`--cancel-at STAGE --after SECONDS`) returned in 0.17 s (`index_osm`),
0.11 s (`read_osm`), 0.10–0.41 s (`write_tiles`, the longest while the
heavy low-zoom tiles are encoded) and 0.08 s (`building_levels`), leaving
no files.
