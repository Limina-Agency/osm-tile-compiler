//! `include/osm_tile_compiler.h` declares exactly the functions the library
//! exports, with the constants it uses.

use std::collections::BTreeSet;

const HEADER: &str = include_str!("../include/osm_tile_compiler.h");
const LIB: &str = include_str!("../src/lib.rs");

/// The `otc_*` names followed by `(` in `text`, after `prefix`.
fn functions(text: &str, prefix: &str) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| line.trim_start().strip_prefix(prefix).map(|_| line))
        .filter_map(|line| {
            let start = line.find("otc_")?;
            let rest = &line[start..];
            Some(rest[..rest.find('(')?].to_string())
        })
        .collect()
}

#[test]
fn the_header_declares_every_exported_function() {
    let exported: BTreeSet<String> = functions(LIB, "pub extern \"C\" fn")
        .union(&functions(LIB, "pub unsafe extern \"C\" fn"))
        .cloned()
        .collect();
    let declared: BTreeSet<String> = HEADER
        .lines()
        .filter(|line| !line.starts_with(' ') && !line.starts_with('/') && line.ends_with(");"))
        .filter_map(|line| {
            let start = line.find("otc_")?;
            let rest = &line[start..];
            Some(rest[..rest.find('(')?].to_string())
        })
        .collect();
    assert_eq!(exported.len(), 11, "{exported:?}");
    assert_eq!(declared, exported);
}

fn define(name: &str) -> i64 {
    let prefix = format!("#define {name} ");
    let line = HEADER
        .lines()
        .find(|line| line.starts_with(&prefix))
        .unwrap_or_else(|| panic!("{name} is not defined"));
    line[prefix.len()..].trim().parse().unwrap()
}

#[test]
fn the_header_constants_match() {
    use osm_tile_compiler_ffi::*;
    assert_eq!(define("OTC_ABI_VERSION"), i64::from(ABI_VERSION));
    assert_eq!(otc_abi_version(), ABI_VERSION);
    for (name, value) in [
        ("OTC_OK", OTC_OK),
        ("OTC_ERROR_INVALID_ARGUMENT", OTC_ERROR_INVALID_ARGUMENT),
        ("OTC_ERROR_INVALID_REQUEST", OTC_ERROR_INVALID_REQUEST),
        ("OTC_ERROR_NOT_FINISHED", OTC_ERROR_NOT_FINISHED),
        ("OTC_ERROR_INTERNAL", OTC_ERROR_INTERNAL),
        ("OTC_JOB_RUNNING", 0),
        ("OTC_JOB_SUCCEEDED", 1),
        ("OTC_JOB_FAILED", 2),
        ("OTC_JOB_CANCELLED", 3),
    ] {
        assert_eq!(define(name), i64::from(value), "{name}");
    }
}
