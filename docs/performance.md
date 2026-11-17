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
decoder state. Building levels read the whole basemap into memory.

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
  scratch and the archive as it is written. It peaks in `write_tiles`, at
  5.4 times the extract.
- Stages with 2 threads: `index_osm` 2.0 s, `read_osm` 29.4 s, `write_tiles`
  24.1 s, `building_levels` 2.4 s, and 2.6 s before them (starting, the
  supplemental datasets). With 16: 1.9, 8.8, 9.3 and 1.5 s.
- `building_levels` is the compile's memory peak (546 MiB with 2 threads,
  against 333 MiB in `read_osm`; 799 against 462 with 4, 1174 against 905
  with 16).
- The supplemental datasets, once unpacked and cached, cost an inland
  region almost nothing in time or memory; they cost disk: 1.45 GB of
  downloads unpacked into another 2.23 GB.

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
