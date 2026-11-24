//! Each reader on a small file of its format holding the same three
//! features: a point with attributes, a line and a polygon with a hole.

use super::*;
use geo_types::{LineString, Point, Polygon, coord};
use std::path::Path;

/// Kraków's market square, and a line and a square with a hole beside it.
fn point() -> Point<f64> {
    Point::new(19.9372, 50.0617)
}
fn line() -> LineString<f64> {
    LineString::from(vec![(19.93, 50.06), (19.94, 50.065), (19.95, 50.06)])
}
fn polygon() -> Polygon<f64> {
    Polygon::new(
        LineString::from(vec![
            (19.90, 50.00),
            (19.92, 50.00),
            (19.92, 50.02),
            (19.90, 50.02),
            (19.90, 50.00),
        ]),
        vec![LineString::from(vec![
            (19.905, 50.005),
            (19.905, 50.015),
            (19.915, 50.015),
            (19.915, 50.005),
            (19.905, 50.005),
        ])],
    )
}

fn read_all(input: &DataInput) -> (Vec<SourceFeature>, DataReadReport) {
    let mut features = Vec::new();
    let report = input
        .read(None, |feature| {
            features.push(feature);
            Ok(())
        })
        .unwrap();
    (features, report)
}

/// Whether two geometries match within `tolerance` degrees, point by
/// point.
fn close(a: &Geometry<f64>, b: &Geometry<f64>, tolerance: f64) -> bool {
    use geo::CoordsIter;
    let (a, b): (Vec<_>, Vec<_>) = (a.coords_iter().collect(), b.coords_iter().collect());
    a.len() == b.len()
        && a.iter()
            .zip(&b)
            .all(|(p, q)| (p.x - q.x).abs() <= tolerance && (p.y - q.y).abs() <= tolerance)
}

/// The three features came back, in order, with the point's attributes.
fn check_three(features: &[SourceFeature], tolerance: f64) {
    assert_eq!(features.len(), 3, "{features:?}");
    assert!(
        close(&features[0].geometry, &Geometry::Point(point()), tolerance),
        "{:?}",
        features[0].geometry
    );
    let props = &features[0].properties;
    assert_eq!(
        props.get("name"),
        Some(&PropertyValue::String("Rynek Główny".into()))
    );
    assert_eq!(props.get("visitors"), Some(&PropertyValue::Int(1200)));
    assert!(
        matches!(props.get("height"), Some(PropertyValue::Float(h)) if (*h - 12.5).abs() < 1e-6)
    );
    let kind = |g: &Geometry<f64>| match g {
        Geometry::LineString(_) | Geometry::MultiLineString(_) => "line",
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) => "polygon",
        _ => "other",
    };
    assert_eq!(kind(&features[1].geometry), "line");
    assert_eq!(kind(&features[2].geometry), "polygon");
    use geo::Area;
    let area = features[2].geometry.unsigned_area();
    let expected = polygon().unsigned_area();
    assert!(
        (area - expected).abs() < expected * 1e-3,
        "{area} vs {expected}"
    );
}

fn web_mercator(geometry: Geometry<f64>) -> Geometry<f64> {
    use geo::MapCoords;
    const R: f64 = 6_378_137.0;
    geometry.map_coords(|c| {
        coord! {
            x: c.x.to_radians() * R,
            y: (std::f64::consts::FRAC_PI_4 + c.y.to_radians() / 2.0).tan().ln() * R,
        }
    })
}

/// ISO WKB of a point, line or polygon, little-endian.
fn to_wkb(geometry: &Geometry<f64>) -> Vec<u8> {
    let mut out = vec![1u8];
    let ring = |out: &mut Vec<u8>, ring: &LineString<f64>| {
        out.extend((ring.0.len() as u32).to_le_bytes());
        for c in &ring.0 {
            out.extend(c.x.to_le_bytes());
            out.extend(c.y.to_le_bytes());
        }
    };
    match geometry {
        Geometry::Point(p) => {
            out.extend(1u32.to_le_bytes());
            out.extend(p.x().to_le_bytes());
            out.extend(p.y().to_le_bytes());
        }
        Geometry::LineString(l) => {
            out.extend(2u32.to_le_bytes());
            ring(&mut out, l);
        }
        Geometry::Polygon(p) => {
            out.extend(3u32.to_le_bytes());
            out.extend((1 + p.interiors().len() as u32).to_le_bytes());
            ring(&mut out, p.exterior());
            for hole in p.interiors() {
                ring(&mut out, hole);
            }
        }
        _ => unreachable!(),
    }
    out
}

fn geometries() -> [Geometry<f64>; 3] {
    [
        Geometry::Point(point()),
        Geometry::LineString(line()),
        Geometry::Polygon(polygon()),
    ]
}

#[test]
fn geojson_feature_collections_and_sequences_read() {
    let dir = tempfile::tempdir().unwrap();
    let features = geometries()
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let mut feature = geojson::Feature::from(geojson::Geometry::from(g));
            if i == 0 {
                feature.set_property("name", "Rynek Główny");
                feature.set_property("visitors", 1200);
                feature.set_property("height", 12.5);
                feature.set_property("missing", serde_json::Value::Null);
                feature.set_property("tags", serde_json::json!(["a", "b"]));
                feature.id = Some(geojson::feature::Id::Number(42.into()));
            }
            feature
        })
        .collect::<Vec<_>>();
    let collection = geojson::FeatureCollection {
        bbox: None,
        features: features.clone(),
        foreign_members: None,
    };
    let path = dir.path().join("places.geojson");
    std::fs::write(&path, collection.to_string()).unwrap();
    let (read, report) = read_all(&DataInput::new(&path));
    check_three(&read, 1e-12);
    assert_eq!(read[0].id, 42);
    assert_eq!(read[1].id, 2, "the position is the id without one");
    assert!(!read[0].properties.contains_key("missing"));
    assert_eq!(
        read[0].properties["tags"],
        PropertyValue::String(r#"["a","b"]"#.into())
    );
    assert_eq!(report.crs, "EPSG:4326");
    assert_eq!(report.features, 3);

    let seq = dir.path().join("places.geojsonl");
    let lines = features
        .iter()
        .map(|f| format!("\u{1e}{f}\n"))
        .collect::<String>();
    std::fs::write(&seq, lines).unwrap();
    check_three(&read_all(&DataInput::new(&seq)).0, 1e-12);

    // A lone geometry, and an old-style crs member naming Web Mercator.
    let mercator = dir.path().join("mercator.json");
    let projected = geojson::Geometry::from(&web_mercator(Geometry::Point(point())));
    std::fs::write(
        &mercator,
        format!(
            r#"{{"type":"FeatureCollection","crs":{{"type":"name","properties":{{"name":"urn:ogc:def:crs:EPSG::3857"}}}},"features":[{{"type":"Feature","properties":{{}},"geometry":{projected}}}]}}"#
        ),
    )
    .unwrap();
    let (read, report) = read_all(&DataInput::new(&mercator));
    assert!(close(&read[0].geometry, &Geometry::Point(point()), 1e-9));
    assert_eq!(report.crs, "EPSG:3857");
}

#[test]
fn csv_reads_coordinate_and_wkt_columns() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.csv");
    std::fs::write(
        &points,
        "\u{feff}Name,Lat,Lon,visitors,height,postcode\n\
         \"Rynek Główny\",50.0617,19.9372,1200,12.5,31-001\n\
         Wawel,50.0540,19.9355,,,\n\
         nowhere,,,1,2,\n",
    )
    .unwrap();
    let (read, report) = read_all(&DataInput::new(&points));
    assert_eq!(read.len(), 2);
    assert_eq!(report.without_geometry, 1);
    assert!(close(&read[0].geometry, &Geometry::Point(point()), 1e-12));
    assert_eq!(
        read[0].properties["Name"],
        PropertyValue::String("Rynek Główny".into())
    );
    assert_eq!(read[0].properties["visitors"], PropertyValue::Int(1200));
    assert_eq!(
        read[0].properties["postcode"],
        PropertyValue::String("31-001".into())
    );
    assert!(!read[1].properties.contains_key("visitors"));

    let shapes = dir.path().join("shapes.tsv");
    let mut text = String::from("wkt\tname\tvisitors\theight\n");
    for (i, geometry) in geometries().iter().enumerate() {
        let wkt = wkt::ToWkt::wkt_string(geometry);
        if i == 0 {
            text.push_str(&format!("{wkt}\tRynek Główny\t1200\t12.5\n"));
        } else if i == 1 {
            // Hex WKB in the same column.
            text.push_str(&format!("{}\t\t\t\n", hex::encode(to_wkb(geometry))));
        } else {
            text.push_str(&format!("SRID=4326;{wkt}\t\t\t\n"));
        }
    }
    std::fs::write(&shapes, text).unwrap();
    check_three(&read_all(&DataInput::new(&shapes)).0, 1e-12);

    // Columns named explicitly, with a delimiter and a projected CRS.
    let named = dir.path().join("named.txt");
    let Geometry::Point(p) = web_mercator(Geometry::Point(point())) else {
        unreachable!()
    };
    std::fs::write(
        &named,
        format!("e;n;name\n{};{};Rynek Główny\n", p.x(), p.y()),
    )
    .unwrap();
    let mut input = DataInput::new(&named);
    input.format = Some(DataFormat::Csv);
    input.crs = Some("EPSG:3857".into());
    input.csv.longitude = Some("e".into());
    input.csv.latitude = Some("n".into());
    input.csv.delimiter = Some(';');
    let (read, _) = read_all(&input);
    assert!(close(&read[0].geometry, &Geometry::Point(point()), 1e-9));
}

#[test]
fn shapefiles_read_with_their_prj() {
    use shapefile::dbase::{FieldValue, Record, TableWriterBuilder};
    let dir = tempfile::tempdir().unwrap();
    // Three shapefiles, one per shape type, in Web Mercator.
    let prj = r#"PROJCS["WGS 84 / Pseudo-Mercator",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]]],PROJECTION["Mercator_1SP"],UNIT["metre",1],AUTHORITY["EPSG","3857"]]"#;
    let table = || {
        TableWriterBuilder::new()
            .add_character_field("name".try_into().unwrap(), 40)
            .add_numeric_field("visitors".try_into().unwrap(), 10, 0)
            .add_numeric_field("height".try_into().unwrap(), 10, 2)
    };
    let record = |name: &str| {
        let mut record = Record::default();
        record.insert("name".into(), FieldValue::Character(Some(name.into())));
        record.insert("visitors".into(), FieldValue::Numeric(Some(1200.0)));
        record.insert("height".into(), FieldValue::Numeric(Some(12.5)));
        record
    };
    let points = dir.path().join("points.shp");
    {
        let Geometry::Point(p) = web_mercator(Geometry::Point(point())) else {
            unreachable!()
        };
        let mut writer = shapefile::Writer::from_path(&points, table()).unwrap();
        writer
            .write_shape_and_record(
                &shapefile::Point::new(p.x(), p.y()),
                &record("Rynek Główny"),
            )
            .unwrap();
    }
    std::fs::write(points.with_extension("prj"), prj).unwrap();
    let (read, report) = read_all(&DataInput::new(&points));
    assert_eq!(report.crs, "EPSG:3857");
    assert!(close(&read[0].geometry, &Geometry::Point(point()), 1e-9));
    assert_eq!(read[0].properties["visitors"], PropertyValue::Int(1200));
    assert_eq!(read[0].properties["height"], PropertyValue::Float(12.5));
    assert_eq!(read[0].id, 1);

    let polygons = dir.path().join("polygons.shp");
    {
        let Geometry::Polygon(projected) = web_mercator(Geometry::Polygon(polygon())) else {
            unreachable!()
        };
        // Shapefile outer rings run clockwise, holes counter-clockwise.
        let ring = |ring: &LineString<f64>, clockwise: bool| {
            use geo::Winding;
            let mut ring = ring.clone();
            if clockwise {
                ring.make_cw_winding();
            } else {
                ring.make_ccw_winding();
            }
            ring.0
                .iter()
                .map(|c| shapefile::Point::new(c.x, c.y))
                .collect::<Vec<_>>()
        };
        let shape = shapefile::Polygon::with_rings(vec![
            shapefile::PolygonRing::Outer(ring(projected.exterior(), true)),
            shapefile::PolygonRing::Inner(ring(&projected.interiors()[0], false)),
        ]);
        let mut writer = shapefile::Writer::from_path(&polygons, table()).unwrap();
        writer
            .write_shape_and_record(&shape, &record("square"))
            .unwrap();
    }
    std::fs::write(polygons.with_extension("prj"), prj).unwrap();
    let (read, _) = read_all(&DataInput::new(&polygons));
    use geo::Area;
    let area = read[0].geometry.unsigned_area();
    assert!((area - polygon().unsigned_area()).abs() < 1e-9, "{area}");
}

/// A GeoPackage of the three features in `srs`, 4326 or 3857.
fn write_geopackage(path: &Path, srs: i64) {
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch(
        "CREATE TABLE gpkg_spatial_ref_sys (srs_name TEXT, srs_id INTEGER PRIMARY KEY, \
         organization TEXT, organization_coordsys_id INTEGER, definition TEXT, description TEXT);
         INSERT INTO gpkg_spatial_ref_sys VALUES ('WGS 84', 4326, 'EPSG', 4326, 'GEOGCS[\"WGS 84\"]', NULL);
         INSERT INTO gpkg_spatial_ref_sys VALUES ('Pseudo-Mercator', 3857, 'EPSG', 3857, 'PROJCS[\"WGS 84 / Pseudo-Mercator\"]', NULL);
         CREATE TABLE gpkg_contents (table_name TEXT PRIMARY KEY, data_type TEXT, identifier TEXT);
         CREATE TABLE gpkg_geometry_columns (table_name TEXT, column_name TEXT, \
         geometry_type_name TEXT, srs_id INTEGER, z INTEGER, m INTEGER);
         INSERT INTO gpkg_contents VALUES ('places', 'features', 'places');
         INSERT INTO gpkg_contents VALUES ('other', 'features', 'other');
         INSERT INTO gpkg_contents VALUES ('tiles', 'tiles', 'tiles');
         CREATE TABLE places (fid INTEGER PRIMARY KEY AUTOINCREMENT, geom BLOB, name TEXT, \
         visitors INTEGER, height REAL);
         CREATE TABLE other (fid INTEGER PRIMARY KEY, geom BLOB);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO gpkg_geometry_columns VALUES ('places', 'geom', 'GEOMETRY', ?1, 0, 0)",
        [srs],
    )
    .unwrap();
    for (i, geometry) in geometries().into_iter().enumerate() {
        let geometry = if srs == 3857 {
            web_mercator(geometry)
        } else {
            geometry
        };
        // GP, version 0, little-endian with an XY envelope, the srs id.
        let mut blob = vec![b'G', b'P', 0, 0b0000_0011];
        blob.extend((srs as i32).to_le_bytes());
        use geo::BoundingRect;
        let rect = geometry.bounding_rect().unwrap();
        for v in [rect.min().x, rect.max().x, rect.min().y, rect.max().y] {
            blob.extend(v.to_le_bytes());
        }
        blob.extend(to_wkb(&geometry));
        let (name, visitors, height) = if i == 0 {
            (Some("Rynek Główny"), Some(1200), Some(12.5))
        } else {
            (None, None, None)
        };
        db.execute(
            "INSERT INTO places (fid, geom, name, visitors, height) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![10 + i as i64, blob, name, visitors, height],
        )
        .unwrap();
    }
}

#[test]
fn geopackages_read_a_named_table_in_its_srs() {
    let dir = tempfile::tempdir().unwrap();
    for srs in [4326, 3857] {
        let path = dir.path().join(format!("places-{srs}.gpkg"));
        write_geopackage(&path, srs);
        let mut input = DataInput::new(&path);
        // Two feature tables: one must be named.
        let error = input.read(None, |_| Ok(())).unwrap_err().to_string();
        assert!(error.contains("several feature tables"), "{error}");
        input.table = Some("places".into());
        let (read, report) = read_all(&input);
        check_three(&read, 1e-9);
        assert_eq!(read[0].id, 10);
        assert_eq!(report.crs, format!("EPSG:{srs}"));
        input.table = Some("tiles".into());
        assert!(input.read(None, |_| Ok(())).is_err());
    }
}

#[cfg(feature = "flatgeobuf")]
#[test]
fn flatgeobuf_reads_with_its_crs() {
    use flatgeobuf::{ColumnType, FgbCrs, FgbWriter, FgbWriterOptions, GeometryType};
    use geozero::{ColumnValue, PropertyProcessor};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("places.fgb");
    let mut writer = FgbWriter::create_with_options(
        "places",
        GeometryType::Unknown,
        FgbWriterOptions {
            crs: FgbCrs {
                code: 3857,
                ..Default::default()
            },
            write_index: false,
            detect_type: false,
            promote_to_multi: false,
            ..Default::default()
        },
    )
    .unwrap();
    writer.add_column("name", ColumnType::String, |_, _| {});
    writer.add_column("visitors", ColumnType::Long, |_, _| {});
    writer.add_column("height", ColumnType::Double, |_, _| {});
    for (i, geometry) in geometries().into_iter().enumerate() {
        writer
            .add_feature_geom(web_mercator(geometry), |feature| {
                if i == 0 {
                    feature
                        .property(0, "name", &ColumnValue::String("Rynek Główny"))
                        .unwrap();
                    feature
                        .property(1, "visitors", &ColumnValue::Long(1200))
                        .unwrap();
                    feature
                        .property(2, "height", &ColumnValue::Double(12.5))
                        .unwrap();
                }
            })
            .unwrap();
    }
    writer.write(std::fs::File::create(&path).unwrap()).unwrap();
    let (read, report) = read_all(&DataInput::new(&path));
    check_three(&read, 1e-9);
    assert_eq!(report.crs, "EPSG:3857");
}

#[cfg(feature = "geoparquet")]
#[test]
fn geoparquet_reads_wkb_and_its_projjson_crs() {
    use parquet::data_type::{ByteArray, ByteArrayType, DoubleType, Int64Type};
    use parquet::file::metadata::KeyValue;
    use parquet::file::properties::WriterProperties;
    use parquet::file::writer::SerializedFileWriter;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    for crs in [
        None,
        Some(serde_json::json!({"id": {"authority": "EPSG", "code": 3857}})),
    ] {
        let path = dir.path().join("places.parquet");
        let _ = std::fs::remove_file(&path);
        let schema = Arc::new(
            parquet::schema::parser::parse_message_type(
                "message places { REQUIRED BYTE_ARRAY geometry; OPTIONAL BYTE_ARRAY name \
                 (UTF8); OPTIONAL INT64 visitors; OPTIONAL DOUBLE height; }",
            )
            .unwrap(),
        );
        let mut column = serde_json::json!({"encoding": "WKB", "geometry_types": []});
        if let Some(crs) = &crs {
            column["crs"] = crs.clone();
        }
        let geo = serde_json::json!({"version": "1.1.0", "primary_column": "geometry",
            "columns": {"geometry": column}});
        let properties = WriterProperties::builder()
            .set_key_value_metadata(Some(vec![KeyValue::new("geo".into(), geo.to_string())]))
            .build();
        let file = std::fs::File::create(&path).unwrap();
        let mut writer = SerializedFileWriter::new(file, schema, Arc::new(properties)).unwrap();
        let mut group = writer.next_row_group().unwrap();
        let wkb = geometries()
            .into_iter()
            .map(|g| {
                let g = if crs.is_some() { web_mercator(g) } else { g };
                ByteArray::from(to_wkb(&g))
            })
            .collect::<Vec<_>>();
        let mut index = 0;
        while let Some(mut column) = group.next_column().unwrap() {
            match index {
                0 => {
                    column
                        .typed::<ByteArrayType>()
                        .write_batch(&wkb, None, None)
                        .unwrap();
                }
                1 => {
                    column
                        .typed::<ByteArrayType>()
                        .write_batch(&[ByteArray::from("Rynek Główny")], Some(&[1, 0, 0]), None)
                        .unwrap();
                }
                2 => {
                    column
                        .typed::<Int64Type>()
                        .write_batch(&[1200], Some(&[1, 0, 0]), None)
                        .unwrap();
                }
                _ => {
                    column
                        .typed::<DoubleType>()
                        .write_batch(&[12.5], Some(&[1, 0, 0]), None)
                        .unwrap();
                }
            }
            column.close().unwrap();
            index += 1;
        }
        group.close().unwrap();
        writer.close().unwrap();
        let (read, report) = read_all(&DataInput::new(&path));
        check_three(&read, 1e-9);
        let expected = if crs.is_some() {
            "EPSG:3857"
        } else {
            "EPSG:4326"
        };
        assert_eq!(report.crs, expected);
    }
}

#[test]
fn bounds_and_ids_and_collections() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.geojson");
    std::fs::write(
        &path,
        r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"ref":"7"},"geometry":{"type":"GeometryCollection","geometries":[
                {"type":"Point","coordinates":[19.93,50.06]},
                {"type":"LineString","coordinates":[[19.93,50.06],[19.94,50.07]]}]}},
            {"type":"Feature","properties":{"ref":"8"},"geometry":{"type":"Point","coordinates":[2.35,48.85]}},
            {"type":"Feature","properties":{"ref":"9"},"geometry":null},
            {"type":"Feature","properties":{"ref":"x"},"geometry":{"type":"Point","coordinates":[19.95,50.05]}}
        ]}"#,
    )
    .unwrap();
    let mut input = DataInput::new(&path);
    input.id_attribute = Some("ref".into());
    let mut read = Vec::new();
    let report = input
        .read(Some(CompileBounds::new(50.2, 49.9, 20.2, 19.7)), |f| {
            read.push(f);
            Ok(())
        })
        .unwrap();
    // The collection's two parts share its id; the last feature's ref is
    // not a number, so its position is its id.
    assert_eq!(read.iter().map(|f| f.id).collect::<Vec<_>>(), [7, 7, 4]);
    assert_eq!(report.features, 2);
    assert_eq!(report.outside_bounds, 1);
    assert_eq!(report.without_geometry, 1);
}

#[test]
fn data_layers_validate_and_select_attributes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.csv");
    std::fs::write(&path, "lon,lat,name,kind,secret\n1,2,n,k,s\n").unwrap();
    let mut layer = DataLayer::new("things", DataInput::new(&path));
    let zoom = crate::ZoomRange::new(4, 14).unwrap();
    layer.validate(zoom).unwrap();
    assert_eq!(layer.zoom_range(zoom), (4, 14));
    layer.min_zoom = Some(10);
    layer.max_zoom = Some(16);
    assert_eq!(layer.zoom_range(zoom), (10, 14));
    layer.attributes = Some(vec!["name".into(), "kind".into(), "secret".into()]);
    layer.exclude_attributes = vec!["secret".into()];
    layer
        .rename_attributes
        .insert("kind".into(), "class".into());
    let properties = BTreeMap::from([
        ("name".to_string(), PropertyValue::String("n".into())),
        ("kind".to_string(), PropertyValue::String("k".into())),
        ("secret".to_string(), PropertyValue::String("s".into())),
        ("other".to_string(), PropertyValue::Int(1)),
    ]);
    let attrs = layer.tile_attributes(properties);
    assert_eq!(attrs.keys().collect::<Vec<_>>(), ["class", "name"]);

    let mut bad = layer.clone();
    bad.min_zoom = Some(15);
    assert!(bad.validate(zoom).is_err());
    let mut bad = layer.clone();
    bad.layer = LayerName::from("");
    assert!(bad.validate(zoom).is_err());
    let missing = DataLayer::new("x", DataInput::new(dir.path().join("missing.csv")));
    assert!(missing.validate(zoom).is_err());
    let unknown = DataLayer::new("x", DataInput::new(dir.path().join("a.xyz")));
    let error = unknown.validate(zoom).unwrap_err().to_string();
    assert!(error.contains("no data format"), "{error}");

    // The JSON form, with snake_case or camelCase keys.
    let json = serde_json::json!({
        "layer": "things",
        "source": {"path": path, "csv": {"longitude": "lon", "latitude": "lat"}},
        "minZoom": 10,
        "max_zoom": 14,
        "exclude_attributes": ["secret"]
    });
    let parsed: DataLayer = serde_json::from_value(json).unwrap();
    assert_eq!((parsed.min_zoom, parsed.max_zoom), (Some(10), Some(14)));
    assert_eq!(parsed.source.csv.longitude.as_deref(), Some("lon"));
    let typo = serde_json::json!({"layer": "x", "source": {"path": "a.csv"}, "minzoom": 3});
    assert!(serde_json::from_value::<DataLayer>(typo).is_err());
}
