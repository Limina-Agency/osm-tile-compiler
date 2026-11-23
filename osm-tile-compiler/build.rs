//! Fingerprints the crate's sources and the workspace lock file into
//! `OTC_CODE_FINGERPRINT`, so that a merge cache written by one build is never
//! reused by a build whose code or dependencies differ.
use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// FNV-1a, 64 bits, continuing from `hash`.
fn fnv(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn main() {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let mut paths = Vec::new();
    files(&manifest.join("src"), &mut paths);
    paths.push(manifest.join("Cargo.toml"));
    // The workspace's lock file pins the geometry and gzip crates; a
    // published crate has none, and then its version stands in.
    paths.push(manifest.join("../Cargo.lock"));
    paths.sort();
    // Two FNV-1a streams with different offsets: 128 bits in all.
    let (mut a, mut b) = (0xcbf2_9ce4_8422_2325u64, 0x8422_2325_cbf2_9ce4u64);
    for path in &paths {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let name = path.strip_prefix(&manifest).unwrap_or(path);
        let name = name.to_string_lossy();
        for part in [name.as_bytes(), &(bytes.len() as u64).to_le_bytes(), &bytes] {
            a = fnv(part, a);
            b = fnv(part, b);
        }
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=OTC_CODE_FINGERPRINT={version}-{target}-{a:016x}{b:016x}");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=../Cargo.lock");
}
