# Performance

Measurements of the compiler's time, memory and disk use. The comparison with
Planetiler is in `parity.md`.

## Baseline for 0.1.3

The numbers later changes are measured against. Measured on 2026-11-19 on a
laptop: AMD Ryzen 7 5800H (8 cores, 16 threads), 16 GiB of RAM, Windows 11,
NVMe SSD (NTFS), otherwise idle. Command line (mimalloc) built from `main`
at 0.1.3 (`45dee3c`), `--building-levels`, the real supplemental datasets
(already unpacked and cached). Extracts from Geofabrik: Małopolskie
(2026-11-17, 194 MiB) and Poland (2026-11-18, 2022 MiB). Each figure is the
median of three runs (four for the area-only compile), interleaved so that
every thread count saw the same machine; 16 threads is all the cores.

- Time is wall-clock for the whole process. Peak RSS is the peak working
  set, which counts the memory-mapped extract; peak private is the peak
  commit charge (`PeakPagefileUsage`), the nearer figure to anonymous memory.
- Peak scratch is the output's directory (scratch, archive and building
  levels as they are written), sampled every 0.25 s.
- Every run of a compile wrote the same archive, byte for byte, at every
  thread count.

### Małopolskie

| Threads | Time | Peak RSS | Peak private | Peak scratch |
|---|---|---|---|---|
| 2 | 46.0 s | 474 MiB | 435 MiB | 365 MiB (1.9×) |
| 4 | 24.9 s | 570 MiB | 558 MiB | 361 MiB (1.9×) |
| 8 | 16.7 s | 735 MiB | 804 MiB | 357 MiB (1.8×) |
| 16 | 15.5 s | 1190 MiB | 1364 MiB | 335 MiB (1.7×) |

- 146.6 MiB archive (11,293 tiles) and 26.3 MiB of building levels.
- Runs varied by under 1% in time.
- Stages with 2 threads: `index_osm` 0.8 s, `read_osm` 24.7 s,
  `write_tiles` 17.8 s, `building_levels` 2.4 s. With 16: 0.3, 6.5, 6.8 and
  1.6 s.
- The scratch peaks twice, about equally: at the end of `read_osm` (the
  packed node store, about 125 MiB, and the compressed sort runs) and at the
  end of `write_tiles` (the runs and the finished archive).

### Poland, whole extract

| Threads | Time | Peak RSS | Peak private | Peak scratch |
|---|---|---|---|---|
| 2 | 537.1 s | 2891 MiB | 2536 MiB | 4598 MiB (2.3×) |
| 4 | 295.6 s | 3003 MiB | 2703 MiB | 4639 MiB (2.3×) |
| 8 | 213.7 s | 3229 MiB | 3073 MiB | 4631 MiB (2.3×) |
| 16 | 200.5 s | 3636 MiB | 3550 MiB | 4639 MiB (2.3×) |

- 1654 MiB archive (222,127 tiles, 58.9 million features) and 305.5 MiB of
  building levels.
- The first two rounds agreed within 6%; the third ran 20–30% slower at 4,
  8 and 16 threads, probably from other work on the machine, so the median is the
  slower of the first two.
- Stages with 2 threads: `index_osm` 7.7 s, `read_osm` 253 s, `write_tiles`
  235 s, `building_levels` 32 s. With 16: 2.9, 70, 90 and 26 s.
- Past 8 threads, time barely improves while memory still grows.

### Poland, `--area-only` of a small box

`--bounds 19.90 50.04 19.98 50.08 --area-only` (central Kraków, about 6 by
4 km) from the whole Poland extract:

| Threads | Time (range) | Peak RSS | Peak private | Peak scratch |
|---|---|---|---|---|
| 2 | 31.6 s (22.8–45.2) | 2700 MiB | 1341 MiB | 1556 MiB |
| 4 | 15.1 s (13.8–17.2) | 2772 MiB | 1447 MiB | 1550 MiB |
| 8 | 11.6 s (11.0–45.2) | 2935 MiB | 1673 MiB | 1550 MiB |
| 16 | 12.9 s (11.4–29.2) | 3204 MiB | 2033 MiB | 1550 MiB |

- 4.0 MiB archive (44 tiles) and 0.7 MiB of building levels.
- Times spread widely because most of the work is reading the 2 GiB extract:
  the slow runs found it out of the page cache (one 2-thread run spent 7 s
  reading the supplemental datasets and 18 s indexing, against 0.3 s and
  8 s). With a warm cache, `read_osm` takes 7.5 s on 16 threads and
  `write_tiles` under 0.5 s.
- A small area-only compile still uses more memory and scratch than the
  whole of Małopolskie: the cost scales with the extract read, not the area.

## Stage profile

`compile --profile <FILE.json>` (and `building-levels --profile`) measures
each stage of a compile, prints a table on stderr and writes the report as
JSON. Library users call `stage_profile::start` and `stage_profile::finish`.
The archive is the same byte for byte with or without it, and the cost is
within run-to-run noise (Berlin at 4 threads, three runs each: 15.24 s
without, 15.27 s with).

For each stage it reports:

- `wall_seconds`, and `cpu_seconds` for the whole process. `busy_threads` is
  CPU / wall, the threads busy on average; `efficiency` is that over the
  threads given.
- `driver_busy`: the CPU / wall of the thread that runs the stage. Near 100%
  the driver is the bottleneck however many workers there are.
- On Linux, `thread_groups`: CPU per group of threads (`omt-render`,
  `omt-encode`, `compile-pool`, `driver`, and `unnamed` for the threads the
  driver starts: the PBF reader and decoders, the intermediate merge), and the
  stage's peak RSS, sampled every 50 ms. Elsewhere these are left out; on
  Windows the CPU times are too.
- `counters`: work timed inside the stage, summed over threads:
  `render_busy`, `render_queue_full_wait` (the driver waiting for a render
  worker), `spill_sort_write`, `encode_tile` and `archive_write`.

The stages run one after another: `read_sources`, `index_osm` (pass 1),
`ways` (pass 2), `relations`, `render_drain`, `intermediate_merge`,
`final_merge` (the merge, encode and write of `write_tiles`),
`finalize_archive`, then `building_levels.scan`, `.levels` and `.write`.

### First profiles, 1 to 4 threads

Measured on 2026-11-21 on a 4-core cloud VM (Intel Xeon @ 2.10 GHz, 4
hardware threads, 16 GiB, Linux), `--building-levels`, the real supplemental
datasets, from `main` at `54d4d6a` plus the profiler. One run each; every
thread count wrote the same archive. Stages under 0.3 s at 1 thread are left
out (`read_sources` 0.2 s; `relations`, `render_drain`, the intermediate
merge, finalizing and the building levels' scan and write together under
0.3 s). The reports are in the project files under `stage-profile/`.

| Region | Stage | 1 thread | 2 | 4 | 1→4 | Busy at 4 | Driver at 4 |
|---|---|---|---|---|---|---|---|
| Berlin | total | 52.7 s | 26.5 s | 15.2 s | 3.5× | | |
| | `index_osm` | 1.0 s | 0.6 s | 0.4 s | 2.5× | 2.59 | 41% |
| | `ways` | 24.9 s | 12.4 s | 6.7 s | 3.7× | 3.88 | 3% |
| | `final_merge` | 24.6 s | 12.2 s | 7.3 s | 3.4× | 3.93 | 37% |
| | `building_levels.levels` | 2.0 s | 1.0 s | 0.5 s | 3.6× | 3.53 | 99% |
| Pomorskie | total | 71.0 s | 37.2 s | 23.3 s | 3.0× | | |
| | `index_osm` | 1.1 s | 0.7 s | 0.5 s | 2.4× | 3.32 | 28% |
| | `ways` | 36.5 s | 18.9 s | 10.8 s | 3.4× | 3.91 | 1% |
| | `final_merge` | 29.2 s | 15.1 s | 10.4 s | 2.8× | 3.56 | 44% |
| | `building_levels.levels` | 3.7 s | 2.0 s | 1.1 s | 3.2× | 3.59 | 100% |
| Małopolskie | total | 128.8 s | 64.8 s | 36.0 s | 3.6× | | |
| | `index_osm` | 2.0 s | 1.1 s | 0.7 s | 2.7× | 3.28 | 28% |
| | `ways` | 69.7 s | 34.7 s | 18.2 s | 3.8× | 3.92 | 1% |
| | `final_merge` | 51.4 s | 25.7 s | 15.3 s | 3.4× | 3.76 | 44% |
| | `building_levels.levels` | 5.1 s | 2.9 s | 1.3 s | 3.8× | 3.94 | 98% |

Peak RSS at 1, 2 and 4 threads: Berlin 246, 295 and 394 MiB; Pomorskie 243,
300 and 387 MiB; Małopolskie 371, 418 and 501 MiB.

What they show:

- `ways` is bound by the render workers: they are busy 3.7 of 4 threads,
  the driver waits on a full render queue for nearly the whole stage, and
  the PBF decoders use about 0.2 of a thread. Pass 2 starting as many
  decoders as render workers costs little CPU.
- `final_merge` has a serial floor: the driver merges the runs and feeds the
  encoders on one thread, 6.8 s of CPU on Małopolskie (4.5 s on Pomorskie,
  2.7 s on Berlin) at any thread count. At 4 threads it is busy 44% of the
  stage; with about 51 s of encoding on Małopolskie it saturates near 8
  encoders, so the stage cannot drop under about 7 s however many threads
  it gets. That is what the parallel final merge removes.
- `relations` takes under 0.15 s on all three regions; its tail matters
  only for extracts with large relations, such as Poland.
- The building levels scale 3.2–3.8× to 4 threads, and their scan and write
  take under 0.05 s here; Poland's flat 2→16 thread time needs a profile
  on Poland itself.
- What fitting cannot show here: the 2–8 thread loss and SMT on the 8-core
  laptop. A 4-core VM scales almost linearly; the same command on the
  baseline laptop at 2, 8 and 16 threads is the next profile.

## Memory

Peak resident memory of a whole process (`ru_maxrss`), from two runs each
on 2026-11-04 on a Linux x86-64 laptop (mimalloc), with the small synthetic
supplemental datasets of the tests rather than the real 1.5 GB:

| Compile | 2 threads | 4 threads |
|---|---|---|
| Monaco (`fixtures/monaco.osm.pbf`, 0.4 MB), command line | 82–85 MiB | 98–114 MiB |
| Monaco, `compile_basemap` (`examples/compile_basemap.rs`) | 91 MiB | 101–105 MiB |

Most of that is fixed: per-thread sort buffers (8 MiB each; see "Sort
runs" for when they grow), queues and
decoder state. Building levels read the basemap a tile at a time (see
"Building levels" below).

## Sort runs

The render workers sort tile records in 8 MiB buffers and spill each full
buffer to a run in the scratch directory; the final merge reads up to 128
runs at once. A planet spills about 240 GB of records, which at 8 MiB is
some 30,000 runs, and those used to be merged twice in full (30,000 to 235
to 2) before the final merge.

The buffers grow only as far as the compile needs to end with about 96
runs. The compile expects to spill three times its input PBF's size (2.5 to
2.9 for the extracts measured here); once the workers have spilled 16 runs
between them, each worker's next buffer holds the bytes still expected,
spread over the runs left to 96. Near and past the estimate, a quarter of
what was spilled is taken to be still to come. A buffer at most doubles at
once and never exceeds its share of the sort memory: half of what is
available when rendering starts (Linux: `MemAvailable` and cgroup limits;
Windows: available physical memory; macOS: half the installed memory;
elsewhere the buffers stay at 8 MiB). `--sort-memory <MIB>` or
`BasemapRequest::sort_memory` sets it instead. When memory caps the
buffers, as on a planet, an intermediate pass merges only the smallest runs
needed to leave 128, which are the small early ones. Extracts that spill
less than 96 runs at 8 MiB (Berlin, Pomorskie and Małopolskie spill 30 to
67) are unchanged.

Before that, from 2026-11-21, each buffer grew past 64 runs to everything
its worker had spilled so far, so half the compile's records were in memory
at the end. A Poland compile with 2 threads peaked at 6.5 GiB against
0.1.3's 2.5 GiB; see "Sizing sort runs from the input" below.

Measured on 2026-11-21 in a cloud container (4 cores, 15 GiB, Linux), 4
threads, `--omit-sources all`, Geofabrik extracts of 2026-11-18. Every
archive is byte-identical to main's.

| Compile | main | memory-sized runs |
|---|---|---|
| Berlin | 12.4 s, 358 MiB, 31 runs | 12.3 s, 355 MiB, 30 runs |
| Pomorskie | 17.3 s, 373 MiB, 41 runs | 16.7 s, 387 MiB, 42 runs |
| Małopolskie | 28.5 s, 497 MiB, 67 runs | 27.9 s, 498 MiB, 67 runs |

To see the planet's case on a small file, the 8 MiB start was scaled down
to 18 KiB in a build for this measurement only, so that Małopolskie spills
as many runs as a planet does. `--sort-memory 16` matches, at that scale,
this machine's 4 workers with half of its 15 GiB on a planet; 64 matches a
machine with four times the memory per worker.

| Małopolskie, 18 KiB start | Runs | Intermediate passes | Time | Peak RSS |
|---|---|---|---|---|
| main | 30,032 | 7.4 s | 40.1 s | 452 MiB |
| memory-sized, `--sort-memory 16` | 314 | 3.4 s | 34.5 s | 461 MiB |
| memory-sized, `--sort-memory 64` | 148 | 0.0 s | 29.3 s | 502 MiB |

### Sizing sort runs from the input

Measured on 2026-11-23 in a cloud container (4 cores, 15 GiB, Linux),
`--omit-sources all`, Geofabrik extracts of 2026-11-18. "Three regions" is
Berlin, Pomorskie and Małopolskie merged into one 369 MiB PBF with
`osm-pbf-ingest`'s `merge_pbf` example, a fifth of Poland. Each figure is the
mean of two runs, interleaved; peak RSS is `ru_maxrss`, peak anonymous is
`RssAnon` sampled every 0.1 s. Every archive is byte-identical to main's.

| Compile | Build | Time | Peak RSS | Peak anonymous | Most runs at once |
|---|---|---|---|---|---|
| Three regions, 2 threads | 0.1.3 | 141 s | 876 MiB | 575 MiB | 137 |
| | main | 105 s | 1555 MiB | 1246 MiB | 69 |
| | sized from the input | 105 s | 728 MiB | 424 MiB | 90 |
| Three regions, 4 threads | 0.1.3 | 81 s | 991 MiB | 690 MiB | 139 |
| | main | 61 s | 1655 MiB | 1341 MiB | 74 |
| | sized from the input | 58 s | 853 MiB | 551 MiB | 92 |
| Małopolskie, 4 threads | 0.1.3 | 34 s | 627 MiB | 471 MiB | 69 |
| | main | 29 s | 495 MiB | 344 MiB | 68 |
| | sized from the input | 30 s | 495 MiB | 339 MiB | 68 |

- main's buffers grow with the compile, each to about half of what its
  worker has spilled. Sized from the input, the largest buffer was under
  30 MiB here; it grows with the input divided by the 96 runs aimed for.

Poland, the same container, one run each: the openstreetmap.fr extract of
2026-11-23 (2.27 GiB; Geofabrik can't be reached from it), `--bounds 14.07
48.99 24.16 55.04`, since that extract's header has no box. The branch's
archives are byte-identical to main's. About 1.6 GiB of each peak RSS is
the memory-mapped extract and the program.

| Compile | Build | Time | Peak RSS | Peak anonymous | Most runs at once |
|---|---|---|---|---|---|
| Poland, 2 threads | 0.1.3 | 487 s | 2994 MiB | 1413 MiB | 741 |
| | main | 392 s | 4790 MiB | 3209 MiB | 75 |
| | sized from the input | 385 s | 2865 MiB | 1283 MiB | 84 |
| Poland, 4 threads | 0.1.3 | 278 s | 3124 MiB | 1544 MiB | 741 |
| | main | 224 s | 5962 MiB | 4381 MiB | 83 |
| | sized from the input | 223 s | 3278 MiB | 1696 MiB | 86 |

- The extra runs cost nothing measurable: 90 runs still merge in one pass.
- Małopolskie spills fewer runs than the buffers need to grow for, so it is
  unchanged.

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
