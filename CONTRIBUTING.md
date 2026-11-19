# Contributing

Issues and pull requests are welcome.

- `scripts/check.sh` passes (it treats clippy warnings as errors).
- A change to tile output updates `fixtures/monaco-features.json` (see
  `fixtures/README.md`), says why in the pull request, and states its effect
  against Planetiler with `benchmarks/compiler-parity/` (see its README). A
  change that is not meant to alter the output leaves that file untouched.
- Pull requests leave `CHANGELOG.md` alone: it is written at release time
  from the commits (see `RELEASING.md`). Start commit subjects with a
  conventional prefix (`feat:`, `fix:`, `perf:`, `docs:`, `chore:`, `ci:`,
  `test:`, `refactor:`; add `!` for a breaking change, as in `feat!:`) and
  put what users should know in the commit body, which the release notes
  are written from.
- Contributions are licensed under the Apache License, Version 2.0, as the
  license's section 5 describes.

## Changing osm-pbf-ingest alongside this repository

`osm-pbf-ingest` comes from crates.io. To build against a local checkout
of [its repository](https://github.com/Limina-Agency/osm-pbf-ingest)
instead, add an override to `.cargo/config.toml` (untracked):

```toml
[patch.crates-io]
osm-pbf-ingest = { path = "../osm-pbf-ingest" }
```

Once the change is released, raise the version in `Cargo.toml` and remove
the override. A change to what the
ingest reads must leave `fixtures/monaco-features.json` untouched, or explain
the difference as any other output change does.
