# Releasing

1. Run `scripts/check.sh` and the Planetiler comparison in
   `benchmarks/compiler-parity/` on a quiet machine against the previous release.
2. Bump the workspace version and write the release's `CHANGELOG.md`
   section. Pull requests do not edit `CHANGELOG.md`; the section is
   drafted from the commits since the previous tag with
   [git-cliff](https://git-cliff.org) and `cliff.toml`:

   ```sh
   git cliff --unreleased --tag v<version> --prepend CHANGELOG.md
   ```

   Then edit the draft into the changelog's usual prose: what changed for
   users, measurements, and what code that uses the library needs to change.
   Commit it with the version bump.
3. Tag `v<version>` and push the tag. The release workflow refuses a tag that does
   not match the crate version, runs the CI checks, then builds the compiler
   binary for Linux (x86_64 and arm64), macOS (arm64 and x86_64) and Windows
   (x86_64) and attaches the archives and their SHA-256 checksums to the
   GitHub release.
4. Publish `osm-tile-compiler` to crates.io.

Each archive carries `THIRD-PARTY-LICENSES`, the licenses of the crates
the binary includes, which the release workflow writes with cargo-about from
`about.toml` and `about.hbs`. A dependency under a license `about.toml` does
not accept fails the release build; check before tagging with
`cargo about generate --locked -m osm-tile-compiler/Cargo.toml about.hbs`.
