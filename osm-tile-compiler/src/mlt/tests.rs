use super::decode::json::{geojson, same};
use super::decode::{self, Geometry as G, UNSUPPORTED, Value};
use super::*;
use fast_mvt::{MvtCoord, MvtGeomType, MvtLayerBuilder};
use serde_json::Value as Json;
use std::num::NonZeroU32;
use std::path::PathBuf;

fn coords(points: &[(i32, i32)]) -> Vec<MvtCoord> {
    points.iter().map(|&(x, y)| MvtCoord { x, y }).collect()
}

/// The fixtures directory, which a packaged crate doesn't have.
fn fixtures() -> Option<PathBuf> {
    fixtures_in("mlt-synthetic")
}

fn fixtures_in(name: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(name);
    dir.is_dir().then_some(dir)
}

/// Decodes every `.mlt` fixture in `dir` and compares it with its `.json`.
/// With `skip_unsupported`, a fixture in an encoding the decoder doesn't
/// read is counted rather than failed. Returns (checked, unsupported).
fn check_fixtures(dir: &std::path::Path, skip_unsupported: bool) -> (usize, Vec<String>) {
    let (mut checked, mut unsupported) = (0, Vec::new());
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("mlt"))
        .collect();
    paths.sort();
    for path in paths {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let tile = std::fs::read(&path).unwrap();
        let expected: Json =
            serde_json::from_slice(&std::fs::read(path.with_extension("json")).unwrap()).unwrap();
        let layers = match decode::decode(&tile) {
            Ok(layers) => layers,
            Err(e) if skip_unsupported && e.starts_with(UNSUPPORTED) => {
                unsupported.push(name);
                continue;
            }
            Err(e) => panic!("{}: {e}", path.display()),
        };
        let features = Json::Array(geojson(&layers));
        assert!(
            same(&features, &expected["features"]),
            "{}:\n ours   {features}\n theirs {}",
            path.display(),
            expected["features"]
        );
        checked += 1;
    }
    (checked, unsupported)
}

#[test]
fn the_decoder_reads_the_specification_fixtures() {
    let Some(dir) = fixtures() else {
        return;
    };
    let (checked, _) = check_fixtures(&dir, false);
    assert!(checked >= 300, "only {checked} fixtures");
    // The ones the Rust reference encoder writes in its own ways.
    let (checked, _) = check_fixtures(&fixtures_in("mlt-synthetic-rust").unwrap(), false);
    assert!(checked >= 116, "only {checked} Rust fixtures");
}

/// The fixtures of a checkout of maplibre-tile-spec, as they are now:
/// `MLT_SPEC_FIXTURES` lists v1 fixture directories, such as its
/// `test/synthetic/0x01` and `test/synthetic/0x01-rust`, separated by the
/// platform's path separator. Every fixture in an encoding the decoder reads
/// must decode to its JSON; the nightly workflow runs this against the
/// specification's main branch (`scripts/check-mlt-spec.sh`).
#[test]
#[ignore = "needs MLT_SPEC_FIXTURES"]
fn the_decoder_reads_the_current_specification_fixtures() {
    let dirs = std::env::var_os("MLT_SPEC_FIXTURES").expect("MLT_SPEC_FIXTURES is not set");
    for dir in std::env::split_paths(&dirs) {
        let (checked, unsupported) = check_fixtures(&dir, true);
        eprintln!(
            "{}: {checked} fixtures decoded, {} in encodings the decoder doesn't read",
            dir.display(),
            unsupported.len()
        );
        eprintln!(
            "unsupported in {}: {}",
            dir.display(),
            unsupported.join(" ")
        );
        assert!(checked > 0, "{}: no fixture decoded", dir.display());
    }
}

/// Each layer of `tile` transcoded, then decoded, must hold what the MVT
/// layer does.
fn round_trip(tile: &[u8]) -> Vec<u8> {
    let mlt = from_mvt(tile).unwrap();
    decode::same_as_mvt(&mlt, tile).unwrap();
    // Back to MVT, the tile holds the same features, and encodes to the same
    // MLT bytes again.
    let back = to_mvt(&mlt).unwrap();
    decode::same_as_mvt(&mlt, &back).unwrap();
    assert_eq!(
        from_mvt(&back).unwrap(),
        mlt,
        "MLT to MVT to MLT changed the tile"
    );
    mlt
}

fn layer(name: &str, extent: u32) -> MvtLayerBuilder {
    let mut layer = MvtLayerBuilder::new(name).unwrap();
    layer.extent(NonZeroU32::new(extent).unwrap());
    layer
}

#[test]
fn simple_layers_are_the_bytes_of_the_specification_fixtures() {
    let Some(dir) = fixtures() else {
        return;
    };
    let fixture = |name: &str| std::fs::read(dir.join(format!("{name}.mlt"))).unwrap();

    let mut point = layer("layer1", 64).feature_of(MvtGeomType::Point);
    point.id(Some(100));
    point.points(coords(&[(13, 42)])).unwrap();
    let tile = point.end().encode();
    assert_eq!(round_trip(&tile), fixture("id"));

    let mut line = layer("layer1", 64).feature_of(MvtGeomType::LineString);
    line.line(coords(&[(11, 52), (71, 72), (61, 22)])).unwrap();
    let tile = line.end().encode();
    assert_eq!(round_trip(&tile), fixture("line"));

    let mut polygon = layer("layer1", 64).feature_of(MvtGeomType::Polygon);
    polygon
        .ring(coords(&[(11, 52), (61, 22), (71, 72)]), true)
        .unwrap();
    polygon
        .ring(coords(&[(65, 66), (55, 36), (35, 56)]), false)
        .unwrap();
    let tile = polygon.end().encode();
    // The reference encoder writes the ring lengths [3, 3] as one run, two
    // bytes longer than plain.
    let ours = round_trip(&tile);
    assert_eq!(ours.len() + 2, fixture("poly_hole").len());
    assert_eq!(
        decode::decode(&ours).unwrap(),
        decode::decode(&fixture("poly_hole")).unwrap()
    );
}

#[test]
fn every_geometry_type_mixes_in_one_layer() {
    let square = |x: i32, y: i32, size: i32| {
        coords(&[(x, y), (x + size, y), (x + size, y + size), (x, y + size)])
    };
    let mut builder = layer("mixed", 4096);
    type Add = Box<dyn Fn(&mut fast_mvt::MvtFeatureBuilder)>;
    let kinds: Vec<(MvtGeomType, Add)> = vec![
        (
            MvtGeomType::Point,
            Box::new(|f| {
                f.points(coords(&[(1, 2)])).unwrap();
            }),
        ),
        (
            MvtGeomType::LineString,
            Box::new(|f| {
                f.line(coords(&[(0, 0), (10, 5), (20, 0)])).unwrap();
            }),
        ),
        (
            MvtGeomType::Polygon,
            Box::new(move |f| {
                f.ring(square(0, 0, 100), true).unwrap();
                f.ring(square(10, 10, 20), false).unwrap();
            }),
        ),
        (
            MvtGeomType::Point,
            Box::new(|f| {
                f.points(coords(&[(5, 5), (-7, 9000), (4096, 4096)]))
                    .unwrap();
            }),
        ),
        (
            MvtGeomType::LineString,
            Box::new(|f| {
                f.line(coords(&[(0, 0), (1, 1)])).unwrap();
                f.line(coords(&[(5, 5), (6, 6), (7, 5)])).unwrap();
            }),
        ),
        (
            MvtGeomType::Polygon,
            Box::new(move |f| {
                f.ring(square(200, 200, 50), true).unwrap();
                f.ring(square(300, 300, 50), true).unwrap();
                f.ring(square(310, 310, 10), false).unwrap();
                f.ring(square(320, 320, 10), false).unwrap();
            }),
        ),
    ];
    // Every subset of the six kinds, in order, as its own layer, so that
    // each combination of topology streams is written.
    for mask in 1u32..64 {
        let mut builder = layer("mixed", 4096);
        for (bit, (kind, draw)) in kinds.iter().enumerate() {
            if mask & (1 << bit) != 0 {
                let mut feature = builder.feature_of(*kind);
                draw(&mut feature);
                builder = feature.end();
            }
        }
        round_trip(&builder.encode());
    }
    for (kind, draw) in &kinds {
        let mut feature = builder.feature_of(*kind);
        draw(&mut feature);
        builder = feature.end();
    }
    let mlt = round_trip(&builder.encode());
    let layers = decode::decode(&mlt).unwrap();
    let types = layers[0]
        .features
        .iter()
        .map(|f| match f.geometry {
            G::Point(_) => 0,
            G::LineString(_) => 1,
            G::Polygon(_) => 2,
            G::MultiPoint(_) => 3,
            G::MultiLineString(_) => 4,
            G::MultiPolygon(_) => 5,
        })
        .collect::<Vec<_>>();
    assert_eq!(types, [0, 1, 2, 3, 4, 5]);
}

#[test]
fn ids_and_attributes_keep_their_values_and_nulls() {
    use fast_mvt::MvtValueRef as V;
    type Row<'a> = (Option<u64>, Vec<(&'a str, V<'a>)>);
    let rows: Vec<Row> = vec![
        (
            Some(7),
            vec![
                ("class", V::String("primary")),
                ("rank", V::UInt(3)),
                ("oneway", V::Bool(true)),
            ],
        ),
        (
            None,
            vec![
                ("class", V::String("primary")),
                ("ele", V::Double(1.5)),
                ("name", V::String("Ülm")),
            ],
        ),
        (
            Some(5_000_000_000),
            vec![("rank", V::SInt(-4)), ("big", V::UInt(u64::MAX))],
        ),
        (
            Some(9),
            vec![
                ("class", V::String("")),
                ("f", V::Float(0.25)),
                ("mixed", V::Int(5)),
            ],
        ),
        (
            Some(10),
            vec![
                ("mixed", V::String("five")),
                ("oneway", V::Bool(false)),
                ("ele", V::Int(3)),
            ],
        ),
        (
            Some(11),
            vec![
                ("wide", V::Int(i64::MIN)),
                ("unsigned", V::UInt(3_000_000_000)),
            ],
        ),
    ];
    let mut builder = layer("attributes", 4096);
    for (i, (id, tags)) in rows.iter().enumerate() {
        let mut feature = builder.feature_of(MvtGeomType::Point);
        feature.id(*id);
        for (key, value) in tags {
            feature.tag_ref(key, *value).unwrap();
        }
        feature.points(coords(&[(i as i32, 0)])).unwrap();
        builder = feature.end();
    }
    let mlt = round_trip(&builder.encode());
    let layers = decode::decode(&mlt).unwrap();
    let features = &layers[0].features;
    assert_eq!(
        features.iter().map(|f| f.id).collect::<Vec<_>>(),
        [
            Some(7),
            None,
            Some(5_000_000_000),
            Some(9),
            Some(10),
            Some(11)
        ]
    );
    let value = |row: usize, key: &str| {
        features[row]
            .properties
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
    };
    // Narrow integer columns stay integers; one with a value past i64 is
    // u64; numbers of mixed kinds are doubles; anything mixed with a string
    // is a string.
    assert_eq!(value(0, "rank"), Some(Value::Int(3)));
    assert_eq!(value(2, "rank"), Some(Value::Int(-4)));
    assert_eq!(value(2, "big"), Some(Value::UInt(u64::MAX)));
    assert_eq!(value(1, "ele"), Some(Value::Double(1.5)));
    assert_eq!(value(4, "ele"), Some(Value::Double(3.0)));
    assert_eq!(value(3, "f"), Some(Value::Float(0.25)));
    assert_eq!(value(3, "mixed"), Some(Value::String("5".into())));
    assert_eq!(value(4, "oneway"), Some(Value::Bool(false)));
    assert_eq!(value(5, "wide"), Some(Value::Int(i64::MIN)));
    assert_eq!(value(5, "unsigned"), Some(Value::UInt(3_000_000_000)));
    assert_eq!(value(3, "class"), Some(Value::String(String::new())));
    assert_eq!(value(1, "rank"), None);
}

#[test]
fn layers_without_features_and_empty_geometries_are_left_out() {
    let mut builder = layer("empty", 4096);
    let feature = builder.feature_of(MvtGeomType::Point);
    builder = feature.end();
    let mut tile = builder.encode();
    let mut point = layer("kept", 4096).feature_of(MvtGeomType::Point);
    point.points(coords(&[(1, 1)])).unwrap();
    tile.extend(point.end().encode());
    let mlt = round_trip(&tile);
    let layers = decode::decode(&mlt).unwrap();
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].name, "kept");
    // Layers transcoded one at a time concatenate to the whole tile's.
    assert_eq!(
        from_mvt(&point_tile("a")).unwrap().len() + from_mvt(&point_tile("b")).unwrap().len(),
        {
            let mut both = point_tile("a");
            both.extend(point_tile("b"));
            from_mvt(&both).unwrap().len()
        }
    );
}

fn point_tile(name: &str) -> Vec<u8> {
    let mut point = layer(name, 4096).feature_of(MvtGeomType::Point);
    point.points(coords(&[(1, 1)])).unwrap();
    point.end().encode()
}

#[test]
fn long_runs_and_many_features_round_trip() {
    let mut builder = layer("many", 4096);
    for i in 0..1000i32 {
        let mut feature = builder.feature_of(MvtGeomType::LineString);
        feature.id(Some(1000 + i as u64));
        // Long runs of one class, and a key most features lack.
        feature
            .tag_ref(
                "class",
                fast_mvt::MvtValueRef::String(if i < 700 { "path" } else { "track" }),
            )
            .unwrap();
        if i % 97 == 0 {
            feature
                .tag_ref("bridge", fast_mvt::MvtValueRef::Bool(true))
                .unwrap();
        }
        feature
            .line(coords(&[(i, i), (i + 3, i * 2 % 4096), (-i, 9)]))
            .unwrap();
        builder = feature.end();
    }
    let mlt = round_trip(&builder.encode());
    assert!(mlt.len() < builder_size_hint(), "{} bytes", mlt.len());
}

fn builder_size_hint() -> usize {
    // 1000 three-vertex lines with an id and a class: under 12 bytes each.
    12_000
}

#[test]
fn byte_runs_split_at_their_limits() {
    for bytes in [
        vec![],
        vec![1],
        vec![1, 1],
        vec![1, 1, 1],
        vec![7; 130],
        vec![7; 131],
        vec![7; 400],
        (0..=255).collect::<Vec<u8>>(),
        (0..300).map(|i| (i % 2) as u8).collect(),
        [vec![1, 2], vec![3; 5], vec![4]].concat(),
    ] {
        let encoded = byte_rle(&bytes);
        let mut decoded = Vec::new();
        let mut at = 0;
        while at < encoded.len() {
            let control = encoded[at];
            at += 1;
            if control < 128 {
                decoded.extend(std::iter::repeat_n(encoded[at], usize::from(control) + 3));
                at += 1;
            } else {
                let n = 256 - usize::from(control);
                decoded.extend_from_slice(&encoded[at..at + n]);
                at += n;
            }
        }
        assert_eq!(decoded, bytes);
    }
}

#[test]
fn integer_streams_take_the_smallest_encoding() {
    let mut data = Vec::new();
    int_stream(&mut data, stream::DATA, &[5; 100], 32, false);
    assert_eq!(data[1], encoding::RLE_VARINT);
    let mut data = Vec::new();
    let ascending = (1000..1100).collect::<Vec<u64>>();
    int_stream(&mut data, stream::DATA, &ascending, 32, true);
    assert_eq!(data[1], encoding::DELTA_RLE_VARINT);
    let mut data = Vec::new();
    int_stream(&mut data, stream::DATA, &[3, 1, 4, 1, 5], 32, true);
    assert_eq!(data[1], encoding::VARINT);
}

/// MapLibre Native 6.4's C++ decoder needs a present stream on every string
/// column, and no run-length stream longer than the values it holds.
#[test]
fn streams_suit_the_maplibre_native_decoder() {
    use fast_mvt::MvtValueRef as V;
    let (mut meta, mut data) = (Vec::new(), Vec::new());
    let values = [Some(V::String("a")), Some(V::String("b"))];
    encode_property("name", &values, &mut meta, &mut data);
    assert_eq!(meta[0], column::STRING + 1);
    // Three streams, the first the present one.
    assert_eq!(&data[..2], &[3, stream::PRESENT]);

    // Deltas 1000, 1, 1 make two runs: four words for three values.
    let mut data = Vec::new();
    int_stream(&mut data, stream::DATA, &[1000, 1001, 1002], 32, true);
    assert_eq!(data[1], encoding::DELTA_VARINT);
    let mut data = Vec::new();
    int_stream(&mut data, stream::DATA, &[9, 9, 9, 9, 9, 9, 7], 32, false);
    assert_eq!(data[1], encoding::RLE_VARINT);
    let mut data = Vec::new();
    int_stream(&mut data, stream::DATA, &[9, 9, 7], 32, false);
    assert_eq!(data[1], encoding::VARINT);
}

#[test]
fn unsupported_encodings_are_named() {
    let Some(dir) = fixtures() else {
        return;
    };
    let _ = dir;
    // A shared dictionary column: type 30.
    let mut body = Vec::new();
    put_string(&mut body, "layer1");
    put_varint(&mut body, 64);
    put_varint(&mut body, 2);
    body.push(column::GEOMETRY);
    body.push(30);
    let mut tile = Vec::new();
    write_frame(&mut tile, &body);
    let error = decode::decode(&tile).unwrap_err();
    assert!(error.starts_with(UNSUPPORTED), "{error}");
}
