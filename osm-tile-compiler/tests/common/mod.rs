//! Shared by the end-to-end tests: the Monaco fixture and small synthetic
//! supplemental inputs (one sea polygon, one out-of-region lake line, an empty
//! Natural Earth database).

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

const WGS84_PRJ: &str = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;

pub fn monaco_pbf() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/monaco.osm.pbf")
}

/// Zips a shapefile written by `write` together with a WGS84 projection.
fn shapefile_zip(
    dir: &Path,
    archive: &str,
    write: impl FnOnce(&mut shapefile::Writer<std::io::BufWriter<File>>),
) {
    let stem = archive.trim_end_matches(".zip").trim_end_matches(".shp");
    let shp = dir.join(format!("{stem}.shp"));
    {
        let table = shapefile::dbase::TableWriterBuilder::new()
            .add_character_field("name".try_into().unwrap(), 20);
        let mut writer = shapefile::Writer::from_path(&shp, table).unwrap();
        write(&mut writer);
    }
    fs::write(shp.with_extension("prj"), WGS84_PRJ).unwrap();
    let mut zip = zip::ZipWriter::new(File::create(dir.join(archive)).unwrap());
    for extension in ["shp", "shx", "dbf", "prj"] {
        let member = shp.with_extension(extension);
        zip.start_file(
            member.file_name().unwrap().to_str().unwrap(),
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        zip.write_all(&fs::read(&member).unwrap()).unwrap();
        fs::remove_file(member).unwrap();
    }
    zip.finish().unwrap();
}

pub fn synthetic_sources(dir: &Path) {
    use shapefile::{Point, Polygon, PolygonRing, Polyline, dbase::Record};
    let named = |name: &str| {
        let mut record = Record::default();
        record.insert("name".into(), name.to_string().into());
        record
    };
    fs::create_dir_all(dir).unwrap();
    shapefile_zip(dir, "water-polygons-split-3857.zip", |writer| {
        // Clockwise outer ring over the sea south of Monaco.
        let sea = Polygon::new(PolygonRing::Outer(vec![
            Point::new(7.38, 43.69),
            Point::new(7.38, 43.725),
            Point::new(7.47, 43.725),
            Point::new(7.47, 43.69),
            Point::new(7.38, 43.69),
        ]));
        writer.write_shape_and_record(&sea, &named("sea")).unwrap();
    });
    shapefile_zip(dir, "lake_centerline.shp.zip", |writer| {
        let far_away = Polyline::new(vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)]);
        writer
            .write_shape_and_record(&far_away, &named("far away"))
            .unwrap();
    });
    let sqlite = dir.join("natural_earth_vector.sqlite");
    rusqlite::Connection::open(&sqlite)
        .unwrap()
        .execute_batch(
            "CREATE TABLE geometry_columns(f_table_name TEXT, f_geometry_column TEXT, srid INTEGER, geometry_format TEXT);",
        )
        .unwrap();
    let mut zip =
        zip::ZipWriter::new(File::create(dir.join("natural_earth_vector.sqlite.zip")).unwrap());
    zip.start_file(
        "natural_earth_vector.sqlite",
        zip::write::SimpleFileOptions::default(),
    )
    .unwrap();
    zip.write_all(&fs::read(&sqlite).unwrap()).unwrap();
    zip.finish().unwrap();
    fs::remove_file(sqlite).unwrap();
}
