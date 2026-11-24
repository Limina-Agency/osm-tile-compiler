//! End-to-end test of the search index: a small synthetic PBF in the Tatras
//! compiled with the `sar` layer and the index, then queried. A street that
//! crosses several tiles is one record, a hut in `poi` and `sar` is found
//! once, names match without diacritics and by prefix, a word can match the
//! nearest town, and the index's bytes do not depend on the threads.

use osm_tile_compiler::{
    Area, BasemapRequest, CompileBounds, CompileError, SearchIndex, SearchQuery, compile_basemap,
    search_index_path, write_search_index,
};
#[allow(dead_code)]
mod common;
#[path = "common/pbf.rs"]
mod pbf;

use common::synthetic_sources;
use pbf::{Tags, write_pbf};
use std::path::{Path, PathBuf};

const BOX: [f64; 4] = [19.90, 49.20, 20.10, 49.32];

fn write_tatras(path: &Path) {
    let points: [(f64, f64, Tags); 6] = [
        (19.950, 49.297, &[("place", "town"), ("name", "Zakopane")]),
        (
            20.000,
            49.240,
            &[
                ("tourism", "alpine_hut"),
                ("name", "Murowaniec"),
                ("ele", "1500"),
            ],
        ),
        (
            20.010,
            49.235,
            &[("emergency", "access_point"), ("ref", "TT-07")],
        ),
        (
            20.006,
            49.233,
            &[("natural", "spring"), ("name", "Wrzące Źródło")],
        ),
        (
            19.981,
            49.232,
            &[
                ("natural", "peak"),
                ("name", "Kasprowy Wierch"),
                ("ele", "1987"),
            ],
        ),
        (
            19.960,
            49.295,
            &[("amenity", "pharmacy"), ("name", "Apteka Łąkowa")],
        ),
    ];
    let mut nodes: Vec<(i64, f64, f64, Tags)> = points
        .iter()
        .enumerate()
        .map(|(i, &(lon, lat, tags))| (1 + i as i64, lon, lat, tags))
        .collect();
    // Krupówki runs east across several z14 tiles (0.022 degrees each).
    for i in 0..6 {
        nodes.push((100 + i, 19.930 + 0.012 * i as f64, 49.296, &[]));
    }
    write_pbf(
        path,
        BOX,
        &nodes,
        &[(
            1000,
            &[100, 101, 102, 103, 104, 105],
            &[("highway", "residential"), ("name", "Krupówki")],
        )],
        &[],
    );
}

fn compile(dir: &Path) -> PathBuf {
    let sources = dir.join("sources");
    synthetic_sources(&sources);
    let pbf = dir.join("tatras.osm.pbf");
    write_tatras(&pbf);
    let output = dir.join("tatras.pmtiles");
    let mut request = BasemapRequest::new(&pbf, &output, &sources, dir.join("temp"));
    let [west, south, east, north] = BOX;
    request.area = Area::bounds(CompileBounds::new(north, south, east, west));
    request.threads = 2;
    request.sar = true;
    request.search_index = true;
    let result = compile_basemap(&request, |_| {}, || false).unwrap();
    let (path, report) = result.search_index.expect("the index was asked for");
    assert_eq!(path, search_index_path(&output));
    assert_eq!(report.zoom, 14);
    assert_eq!(report.bytes, std::fs::metadata(&path).unwrap().len());
    output
}

fn names(index: &SearchIndex, query: &SearchQuery) -> Vec<String> {
    index
        .search(query)
        .unwrap()
        .into_iter()
        .map(|hit| match hit.context {
            Some(context) => format!("{} ({}), {context}", hit.name, hit.layer),
            None => format!("{} ({})", hit.name, hit.layer),
        })
        .collect()
}

#[test]
fn finds_names_in_a_compiled_archive() {
    let dir = tempfile::tempdir().unwrap();
    let archive = compile(dir.path());
    let index = SearchIndex::open(&search_index_path(&archive)).unwrap();
    assert_eq!(index.metadata()["format"], "search-index");
    assert_eq!(index.metadata()["source"], "tatras.pmtiles");

    // A prefix, any case, no diacritics.
    let hits = index.search(&SearchQuery::new("ZAKOP")).unwrap();
    assert_eq!(hits[0].name, "Zakopane");
    assert_eq!(
        (hits[0].layer.as_str(), hits[0].class.as_str()),
        ("place", "town")
    );

    // One street across tiles, in its town.
    let hits = index.search(&SearchQuery::new("krupowki")).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].layer, "transportation_name");
    assert_eq!(hits[0].context.as_deref(), Some("Zakopane"));
    assert!((19.93..19.99).contains(&hits[0].longitude), "{hits:?}");
    // The town's name narrows it; a word that matches neither drops it.
    assert_eq!(
        names(&index, &SearchQuery::new("ul. krupówki zakopane")),
        ["Krupówki (transportation_name), Zakopane"]
    );
    assert!(names(&index, &SearchQuery::new("krupowki krakow")).is_empty());

    // The hut is in poi and sar; it is found once, as sar.
    let hits = index.search(&SearchQuery::new("murowaniec")).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(
        (
            hits[0].layer.as_str(),
            hits[0].class.as_str(),
            hits[0].subclass.as_str()
        ),
        ("sar", "shelter", "alpine_hut")
    );
    assert_eq!(hits[0].ele, Some(1500));
    assert!(hits[0].feature_id.is_some());

    // An access point with only a number.
    let hits = index.search(&SearchQuery::new("tt 07")).unwrap();
    assert_eq!(hits.len(), 1, "{hits:?}");
    assert_eq!(hits[0].class, "emergency");
    assert_eq!(hits[0].reference.as_deref(), Some("TT-07"));

    assert_eq!(
        names(&index, &SearchQuery::new("wrzace zrodlo")),
        ["Wrzące Źródło (sar)"]
    );
    assert_eq!(
        names(&index, &SearchQuery::new("lakowa")),
        ["Apteka Łąkowa (poi), Zakopane"]
    );
    let hits = index.search(&SearchQuery::new("kasprowy")).unwrap();
    assert_eq!(hits[0].layer, "mountain_peak");
    assert_eq!(hits[0].ele, Some(1987));

    // Filters.
    let mut query = SearchQuery::new("murowaniec");
    query.layers = vec!["poi".into()];
    assert!(index.search(&query).unwrap().is_empty());
    let mut query = SearchQuery::new("zakopane");
    query.within = Some(CompileBounds::new(49.25, 49.20, 20.10, 19.90));
    assert!(index.search(&query).unwrap().is_empty());
    let mut query = SearchQuery::new("zakopane");
    query.near = Some([19.950, 49.297]);
    let hits = index.search(&query).unwrap();
    assert!(hits[0].distance_m.unwrap() < 50.0, "{hits:?}");
    let mut query = SearchQuery::new("a");
    query.limit = 2;
    assert!(index.search(&query).unwrap().len() <= 2);
    assert!(index.search(&SearchQuery::new(" -- ")).unwrap().is_empty());
    assert!(index.search(&SearchQuery::new("qqqqq")).unwrap().is_empty());

    // The same index held in memory answers the same.
    let bytes = std::fs::read(search_index_path(&archive)).unwrap();
    let memory = SearchIndex::from_bytes(bytes.clone()).unwrap();
    assert_eq!(
        memory.search(&SearchQuery::new("krup")).unwrap(),
        index.search(&SearchQuery::new("krup")).unwrap()
    );

    // The same bytes on one thread, and no replacing.
    let again = dir.path().join("again.idx");
    write_search_index(&archive, &again, 1).unwrap();
    assert_eq!(std::fs::read(&again).unwrap(), bytes);
    assert!(matches!(
        write_search_index(&archive, &again, 1),
        Err(CompileError::InvalidRequest(_))
    ));

    // A cut or damaged index fails to open or to answer, and never panics.
    for length in (0..bytes.len()).step_by(97) {
        if let Ok(index) = SearchIndex::from_bytes(bytes[..length].to_vec()) {
            let _ = index.search(&SearchQuery::new("krupowki zakopane"));
        }
    }
    for at in (0..bytes.len()).step_by(89) {
        let mut damaged = bytes.clone();
        damaged[at] ^= 0x5a;
        if let Ok(index) = SearchIndex::from_bytes(damaged) {
            for text in ["z", "murowaniec", "krupowki zakopane", "tt 07"] {
                let _ = index.search(&SearchQuery::new(text));
            }
        }
    }
}
