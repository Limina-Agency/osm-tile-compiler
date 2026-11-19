# osm-tile-compiler-ffi

A C ABI for [osm-tile-compiler](../osm-tile-compiler), for iOS, Android and
other native hosts. Requests and results are JSON; each request runs as a job
the host polls, may cancel, and collects a result from. No function calls
back into the host.

- Header: [`include/osm_tile_compiler.h`](include/osm_tile_compiler.h)
- The JSON and the lifecycle: [`docs/c-abi.md`](../docs/c-abi.md)
- A C host: [`tests/c/smoke.c`](tests/c/smoke.c), built and run by
  `scripts/check-c-abi.sh`

The crate builds a `staticlib` (for an iOS XCFramework), a `cdylib` (for
Android and dynamic links) and an `rlib`. It installs no allocator.

For iOS, build with `IPHONEOS_DEPLOYMENT_TARGET` set to 13.0 or later (CI
uses 13.0): the C code of SQLite, ring and zstd calls `___chkstk_darwin`,
which older targets lack, and Rust's own default is 10.0.

Experimental: not published until the 0.2 API is released.
