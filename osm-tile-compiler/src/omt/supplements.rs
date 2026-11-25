//! Streaming, portable readers for the pinned OpenMapTiles supplemental inputs.
//!
//! Only one SQLite row or shapefile record is decoded at a time. ZIP members are
//! extracted once into `<sources>/.extracted/<archive>/` and reused while the
//! archive's size and mtime are unchanged; a read-only sources directory falls
//! back to the caller's scratch directory. Nothing goes into the repository.
//! Source layers are `lake_centerlines`, `water_polygons`, or the exact NE table.

use super::types::{Source, SourceKind};
use crate::CompileBounds;
use crate::control::Control;
use geo::{BoundingRect, MapCoords};
use geo_types::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon,
    Point, Polygon,
};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};

const MERCATOR_RADIUS: f64 = 6_378_137.0;

/// Which supplemental datasets a compile reads.
#[derive(Debug, Clone, Copy)]
pub struct Read {
    pub water_polygons: bool,
    pub natural_earth: bool,
    pub lake_centerlines: bool,
}

impl Read {
    fn count(self) -> u64 {
        u64::from(self.water_polygons)
            + u64::from(self.natural_earth)
            + u64::from(self.lake_centerlines)
    }
}

pub fn read_sources(
    root: &Path,
    temp: &Path,
    bounds: CompileBounds,
    read: Read,
    control: &Control,
    mut emit: impl FnMut(Source) -> Result<(), String>,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    control.set_total(read.count());
    for (archive, layer, wanted) in [
        (
            "lake_centerline.shp.zip",
            "lake_centerlines",
            read.lake_centerlines,
        ),
        (
            "water-polygons-split-3857.zip",
            "water_polygons",
            read.water_polygons,
        ),
    ] {
        if !wanted {
            continue;
        }
        let files = extract_cached(&root.join(archive), &temp.join(layer), control, warnings)?;
        let shp = files
            .iter()
            .find(|p| p.extension().is_some_and(|e| e == "shp"))
            .ok_or_else(|| format!("No .shp member in {archive}"))?;
        let projection = fs::read_to_string(shp.with_extension("prj"))
            .map_err(|e| format!("Missing shapefile projection: {e}"))?;
        let mercator = if projection.contains("Mercator_Auxiliary_Sphere")
            || projection.contains("Pseudo-Mercator")
        {
            true
        } else if projection.starts_with("GEOGCS[") && projection.contains("WGS") {
            false
        } else {
            return Err(format!(
                "Unsupported shapefile projection in {}",
                shp.display()
            ));
        };
        read_shapes(shp, layer, mercator, bounds, control, &mut emit)?;
        control.add_done(1);
    }
    if !read.natural_earth {
        return Ok(());
    }
    let files = extract_cached(
        &root.join("natural_earth_vector.sqlite.zip"),
        &temp.join("natural_earth"),
        control,
        warnings,
    )?;
    let sqlite = files
        .iter()
        .find(|p| p.extension().is_some_and(|e| e == "sqlite"))
        .ok_or("No SQLite member in Natural Earth archive")?;
    read_natural_earth(sqlite, bounds, control, &mut emit)?;
    control.add_done(1);
    Ok(())
}

const CACHE_MARKER: &str = ".complete";
/// Leads every cache stamp, so a release that changes what a cache holds
/// rebuilds it instead of reading the old one. See `docs/formats.md`.
const CACHE_VERSION: u32 = 1;

/// Extracts `archive` next to itself so repeat compiles skip the work. The
/// marker is written last and the directory renamed into place, so an
/// interrupted extraction is never reused.
fn extract_cached(
    archive: &Path,
    scratch: &Path,
    control: &Control,
    warnings: &mut Vec<String>,
) -> Result<Vec<PathBuf>, String> {
    let name = archive.file_name().ok_or("archive has no file name")?;
    let cache = archive.with_file_name(".extracted").join(name);
    let stamp = file_stamp(
        &archive
            .metadata()
            .map_err(|e| format!("{}: {e}", archive.display()))?,
    );
    if fs::read_to_string(cache.join(CACHE_MARKER)).is_ok_and(|marker| marker == stamp) {
        let mut paths = fs::read_dir(&cache)
            .map_err(|e| e.to_string())?
            .map(|entry| entry.map(|entry| entry.path()).map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        paths.retain(|path| path.file_name().is_some_and(|name| name != CACHE_MARKER));
        paths.sort();
        return Ok(paths);
    }
    let staging = cache.with_extension(format!("{}.partial", std::process::id()));
    let stage = || -> Result<Vec<PathBuf>, String> {
        let _ = fs::remove_dir_all(&staging);
        let paths = extract(archive, &staging, control)?;
        fs::write(staging.join(CACHE_MARKER), &stamp).map_err(|e| e.to_string())?;
        let _ = fs::remove_dir_all(&cache);
        fs::rename(&staging, &cache).map_err(|e| e.to_string())?;
        Ok(paths
            .iter()
            .filter_map(|path| path.file_name())
            .map(|file| cache.join(file))
            .collect())
    };
    stage().or_else(|error| {
        if control.is_cancelled() {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
        warnings.push(format!(
            "cannot cache the extracted {} ({error}); extracted it to scratch space instead",
            archive.display()
        ));
        let _ = fs::remove_dir_all(&staging);
        extract(archive, scratch, control)
    })
}

/// The cache version and a file's size and modification time, which
/// identify the caches derived from it.
fn file_stamp(metadata: &fs::Metadata) -> String {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |age| age.as_nanos());
    format!("v{CACHE_VERSION} {} {modified}", metadata.len())
}

fn extract(archive: &Path, destination: &Path, control: &Control) -> Result<Vec<PathBuf>, String> {
    fs::create_dir_all(destination).map_err(|e| e.to_string())?;
    let file = File::open(archive).map_err(|e| format!("{}: {e}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file)).map_err(|e| e.to_string())?;
    let mut paths = Vec::new();
    for i in 0..zip.len() {
        let mut member = zip.by_index(i).map_err(|e| e.to_string())?;
        let enclosed = member.enclosed_name().ok_or("Unsafe ZIP member path")?;
        let Some(extension) = enclosed.extension().and_then(|s| s.to_str()) else {
            continue;
        };
        if !matches!(extension, "sqlite" | "shp" | "shx" | "dbf" | "prj" | "cpg") {
            continue;
        }
        let path = destination.join(enclosed.file_name().ok_or("ZIP member has no filename")?);
        if paths.contains(&path) {
            return Err("ZIP contains colliding flattened member paths".into());
        }
        // Reuse only complete members. Failed extractions retain a .partial file.
        if !path.is_file() || path.metadata().map_err(|e| e.to_string())?.len() != member.size() {
            let partial = path.with_extension(format!("{extension}.partial"));
            let mut output = File::create(&partial).map_err(|e| e.to_string())?;
            let copied = copy_cancellable(&mut member, &mut output, control);
            let bytes = match copied.and_then(|bytes| {
                output.flush().map_err(|e| e.to_string())?;
                Ok(bytes)
            }) {
                Ok(bytes) => bytes,
                Err(error) => {
                    drop(output);
                    let _ = fs::remove_file(&partial);
                    return Err(error);
                }
            };
            if bytes != member.size() {
                return Err("Truncated supplemental archive member".into());
            }
            drop(output);
            if path.exists() {
                fs::remove_file(&path).map_err(|e| e.to_string())?;
            }
            fs::rename(partial, &path).map_err(|e| e.to_string())?;
        }
        paths.push(path);
    }
    Ok(paths)
}

/// `std::io::copy`, checking for a cancel every 1 MiB.
fn copy_cancellable(
    reader: &mut impl std::io::Read,
    writer: &mut impl Write,
    control: &Control,
) -> Result<u64, String> {
    let mut buffer = vec![0; 1 << 20];
    let mut bytes = 0;
    loop {
        control.check()?;
        let count = match reader.read(&mut buffer) {
            Ok(0) => return Ok(bytes),
            Ok(count) => count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        };
        writer
            .write_all(&buffer[..count])
            .map_err(|e| e.to_string())?;
        bytes += count as u64;
    }
}

fn intersects_bounds(geometry: &Geometry<f64>, bounds: CompileBounds) -> bool {
    geometry
        .bounding_rect()
        .is_some_and(|r| bounds.meets(r.min().x, r.min().y, r.max().x, r.max().y))
}

fn read_shapes(
    path: &Path,
    layer: &str,
    mercator: bool,
    bounds: CompileBounds,
    control: &Control,
    emit: &mut impl FnMut(Source) -> Result<(), String>,
) -> Result<(), String> {
    // Pinned ShapefileReader numbers the features its bounding-box filter
    // returns, from 1 in file order; the filter is GeoTools' BBOX, which
    // tests the geometry itself (not only its envelope) against the bounds
    // in the file's projection, and is left out for the whole world.
    let mut filter = ShapeFilter {
        boxes: (!covers_world(bounds)).then(|| file_boxes(bounds, mercator, 0.0)),
        mercator,
        read: 0,
    };
    let Some(candidates) = candidate_records(path, mercator, bounds) else {
        let mut reader =
            shapefile::Reader::from_path(path).map_err(|e| format!("{}: {e}", path.display()))?;
        for record in reader.iter_shapes_and_records() {
            control.check()?;
            let (shape, record) = record.map_err(|e| e.to_string())?;
            emit_shape(&mut filter, shape, record, layer, emit)?;
        }
        return Ok(());
    };
    // Read each candidate by its index: the shape through the .shx offset,
    // the record at its fixed .dbf position.
    let mut shapes =
        shapefile::ShapeReader::from_path(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let dbf = path.with_extension("dbf");
    let mut records =
        shapefile::dbase::Reader::from_path(&dbf).map_err(|e| format!("{}: {e}", dbf.display()))?;
    for index in candidates {
        control.check()?;
        let missing = || format!("{}: record {index} is missing", path.display());
        let shape = shapes
            .read_nth_shape(index)
            .ok_or_else(missing)?
            .map_err(|e| e.to_string())?;
        records.seek(index).map_err(|e| e.to_string())?;
        let record = records
            .iter_records()
            .next()
            .ok_or_else(missing)?
            .map_err(|e| e.to_string())?;
        emit_shape(&mut filter, shape, record, layer, emit)?;
    }
    Ok(())
}

/// Which shapes `ShapefileReader` reads, and how many it has read so far.
struct ShapeFilter {
    /// The bounds in the file's units, or `None` to read every shape.
    boxes: Option<Vec<geo_types::Rect<f64>>>,
    mercator: bool,
    read: u64,
}

/// Whether `bounds` cover Planetiler's whole world (`Bounds.isWorld`), for
/// which `ShapefileReader` applies no filter.
fn covers_world(bounds: CompileBounds) -> bool {
    const WORLD_LATITUDE: f64 = 85.0511287798066;
    bounds.west <= -180.0
        && bounds.east >= 180.0
        && bounds.south <= -WORLD_LATITUDE
        && bounds.north >= WORLD_LATITUDE
}

/// `bounds` in the file's units, one box per longitude range (two when they
/// cross the antimeridian), each widened by `slack`.
fn file_boxes(bounds: CompileBounds, mercator: bool, slack: f64) -> Vec<geo_types::Rect<f64>> {
    bounds
        .longitude_ranges()
        .map(|(west, east)| {
            let x = |lon: f64| {
                if mercator {
                    lon.to_radians() * MERCATOR_RADIUS
                } else {
                    lon
                }
            };
            let y = |lat: f64| {
                if mercator {
                    (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0)
                        .tan()
                        .ln()
                        * MERCATOR_RADIUS
                } else {
                    lat
                }
            };
            geo_types::Rect::new(
                Coord {
                    x: x(west) - slack,
                    y: y(bounds.south) - slack,
                },
                Coord {
                    x: x(east) + slack,
                    y: y(bounds.north) + slack,
                },
            )
        })
        .collect()
}

/// The indices of the records in `shp` whose bounding box, read from the
/// record header through the `.shx` index, may meet `bounds`; the rest are
/// never decoded (the water polygons are 1.3 GB for the whole planet). The
/// box is widened by about a metre, and `emit_shape` still tests the decoded
/// geometry, so this only skips work, provided each header box covers its
/// shape as the format requires (GDAL, which writes these files, ensures it).
/// `None` when there is no usable index.
fn candidate_records(shp: &Path, mercator: bool, bounds: CompileBounds) -> Option<Vec<usize>> {
    use std::io::{Read, Seek, SeekFrom};
    let index = fs::read(shp.with_extension("shx")).ok()?;
    if index.len() < 100 || (index.len() - 100) % 8 != 0 {
        return None;
    }
    // Per longitude range of the bounds (two when they cross the
    // antimeridian), in the file's units.
    let boxes = file_boxes(bounds, mercator, if mercator { 1.0 } else { 1e-5 });
    let mut file = File::open(shp).ok()?;
    let mut candidates = Vec::new();
    for (position, entry) in index[100..].as_chunks::<8>().0.iter().enumerate() {
        let words = |at: usize| i32::from_be_bytes(entry[at..at + 4].try_into().unwrap());
        let (offset, length) = (
            u64::try_from(words(0)).ok()?,
            usize::try_from(words(4)).ok()?,
        );
        // The shape type, then the box (or a point's x and y), after the
        // record's 8-byte header.
        let mut content = [0_u8; 36];
        let read = content.len().min(length * 2);
        file.seek(SeekFrom::Start(offset * 2 + 8)).ok()?;
        file.read_exact(&mut content[..read]).ok()?;
        let value = |at: usize| f64::from_le_bytes(content[at..at + 8].try_into().unwrap());
        let shape_type = i32::from_le_bytes(content[..4].try_into().unwrap());
        let (min_x, min_y, max_x, max_y) = match shape_type {
            0 => continue,
            1 | 11 | 21 if read >= 20 => (value(4), value(12), value(4), value(12)),
            3 | 5 | 8 | 13 | 15 | 18 | 23 | 25 | 28 | 31 if read >= 36 => {
                (value(4), value(12), value(20), value(28))
            }
            _ => {
                candidates.push(position);
                continue;
            }
        };
        // A box that is not finite proves nothing; decode the shape.
        if ![min_x, min_y, max_x, max_y].iter().all(|v| v.is_finite())
            || boxes.iter().any(|b| {
                max_x >= b.min().x && min_x <= b.max().x && max_y >= b.min().y && min_y <= b.max().y
            })
        {
            candidates.push(position);
        }
    }
    Some(candidates)
}

fn emit_shape(
    filter: &mut ShapeFilter,
    shape: shapefile::Shape,
    record: shapefile::dbase::Record,
    layer: &str,
    emit: &mut impl FnMut(Source) -> Result<(), String>,
) -> Result<(), String> {
    use geo::Intersects;
    if matches!(shape, shapefile::Shape::NullShape) {
        return Ok(());
    }
    let mut geometry: Geometry<f64> = shape
        .try_into()
        .map_err(|e| format!("Shapefile geometry: {e}"))?;
    if let Some(boxes) = &filter.boxes
        && !boxes.iter().any(|b| geometry.intersects(b))
    {
        return Ok(());
    }
    filter.read += 1;
    let id = filter.read;
    if filter.mercator {
        // GeoTools' spherical Mercator inverse, as ShapefileReader applies
        // it: pi/2 - 2 atan(exp(-y)). The equivalent 2 atan(exp(y)) - pi/2
        // differs in the last bit for most points, enough to flip an exact
        // Douglas-Peucker tie (Zatoka Gdańska's label line at z13-14). Java's
        // Math.exp is not the platform's exp, so a few points still differ.
        geometry = geometry.map_coords(|c| Coord {
            x: (c.x / MERCATOR_RADIUS).to_degrees(),
            y: (std::f64::consts::FRAC_PI_2 - 2.0 * (-c.y / MERCATOR_RADIUS).exp().atan())
                .to_degrees(),
        });
    }
    let mut tags = super::types::Tags::new();
    for (key, value) in record {
        use shapefile::dbase::FieldValue;
        let value = match value {
            FieldValue::Character(v) => v,
            FieldValue::Numeric(v) => v.map(|x| x.to_string()),
            FieldValue::Float(v) => v.map(|x| x.to_string()),
            FieldValue::Logical(v) => v.map(|x| x.to_string()),
            FieldValue::Integer(v) => Some(v.to_string()),
            FieldValue::Double(v) | FieldValue::Currency(v) => Some(v.to_string()),
            FieldValue::Memo(v) => Some(v),
            _ => None,
        };
        if let Some(value) = value {
            tags.insert(key, value);
        }
    }
    emit(Source {
        id: id * 10,
        osm_id: id as i64,
        kind: SourceKind::Supplement,
        source_layer: layer.into(),
        tags,
        geometry,
        relations: Vec::new(),
    })
}

fn quote_identifier(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn read_natural_earth(
    path: &Path,
    bounds: CompileBounds,
    control: &Control,
    emit: &mut impl FnMut(Source) -> Result<(), String>,
) -> Result<(), String> {
    let mut connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| e.to_string())?;
    // Long queries (counting a table's rows reads most of the file) stop on
    // a cancel too.
    let cancelled = control.flag();
    connection
        .progress_handler(
            10_000,
            Some(move || cancelled.load(std::sync::atomic::Ordering::Relaxed)),
        )
        .map_err(|e| e.to_string())?;
    let mut metadata = connection
        .prepare(
            "SELECT f_table_name, f_geometry_column, srid, geometry_format FROM geometry_columns",
        )
        .map_err(|e| e.to_string())?;
    let metadata = metadata
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    // JDBC getTables(null, null, null, null) orders by TABLE_TYPE, TABLE_NAME.
    // NaturalEarthReader uses that order and increments one source-wide ID for
    // every non-NULL GEOMETRY, including tables the profile does not consume
    // and features outside our regional bounds.
    let mut table_names = connection
        .prepare(
            "SELECT name FROM sqlite_schema WHERE type IN ('table', 'view') ORDER BY type, name",
        )
        .map_err(|e| e.to_string())?;
    let table_names = table_names
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let mut counts = GeometryCounts::load(path);
    let mut source_id = 0_u64;
    for table in table_names {
        control.check()?;
        if !valid_ne_table(&table) {
            continue;
        }
        let columns = connection
            .prepare(&format!(
                "SELECT * FROM {} LIMIT 0",
                quote_identifier(&table)
            ))
            .map_err(|e| e.to_string())?
            .column_names()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if !columns.iter().any(|name| name == "GEOMETRY") {
            continue;
        }
        if !used_ne_table(&table) {
            let count = counts.get(&table, || {
                connection
                    .query_row(
                        &format!(
                            "SELECT COUNT(*) FROM {} WHERE GEOMETRY IS NOT NULL",
                            quote_identifier(&table)
                        ),
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|count| {
                        u64::try_from(count).map_err(|_| format!("negative row count in {table}"))
                    })
            })?;
            source_id = source_id
                .checked_add(count)
                .ok_or("Natural Earth ID overflow")?;
            continue;
        }
        let (_, geometry_column, srid, format) = metadata
            .iter()
            .find(|(name, _, _, _)| name == &table)
            .ok_or_else(|| format!("Missing Natural Earth geometry metadata for {table}"))?;
        if *srid != Some(4326) || format.as_deref() != Some("WKB") {
            return Err(format!(
                "Unsupported Natural Earth encoding {table}: SRID={srid:?}, format={format:?}"
            ));
        }
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {}", quote_identifier(&table)))
            .map_err(|e| e.to_string())?;
        let names = statement
            .column_names()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let geometry_index = names
            .iter()
            .position(|n| n == geometry_column)
            .ok_or("Natural Earth geometry column absent")?;
        let mut rows = statement.query([]).map_err(|e| e.to_string())?;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            control.check()?;
            let blob = match row.get_ref(geometry_index).map_err(|e| e.to_string())? {
                ValueRef::Null => continue,
                ValueRef::Blob(b) => b,
                _ => return Err(format!("Non-BLOB geometry in {table}")),
            };
            source_id = source_id
                .checked_add(1)
                .ok_or("Natural Earth ID overflow")?;
            let geometry = decode_wkb(blob).map_err(|e| format!("{table}: {e}"))?;
            if !intersects_bounds(&geometry, bounds) {
                continue;
            }
            let mut tags = super::types::Tags::new();
            for (i, name) in names.iter().enumerate() {
                if i == geometry_index {
                    continue;
                }
                let value = match row.get_ref(i).map_err(|e| e.to_string())? {
                    ValueRef::Null | ValueRef::Blob(_) => continue,
                    ValueRef::Integer(v) => v.to_string(),
                    ValueRef::Real(v) => v.to_string(),
                    ValueRef::Text(v) => String::from_utf8_lossy(v).into_owned(),
                };
                tags.insert(name.clone(), value);
            }
            let raw_id = i64::try_from(source_id).map_err(|_| "Natural Earth ID exceeds i64")?;
            emit(Source {
                id: source_id
                    .checked_mul(10)
                    .ok_or("Natural Earth feature ID overflow")?,
                osm_id: raw_id,
                kind: SourceKind::Supplement,
                source_layer: table.clone(),
                tags,
                geometry,
                relations: Vec::new(),
            })?;
        }
    }
    counts.save();
    Ok(())
}

/// How many rows with a geometry each Natural Earth table the profile skips
/// has, cached beside the database. Counting reads most of its 850 MB, which
/// takes over a second when the file is not in the page cache. The cache
/// holds only while the database's size and mtime are unchanged.
struct GeometryCounts {
    path: PathBuf,
    stamp: Option<String>,
    counts: BTreeMap<String, u64>,
    changed: bool,
}

impl GeometryCounts {
    fn load(database: &Path) -> Self {
        let mut path = database.as_os_str().to_owned();
        path.push(".geometry-counts");
        let stamp = database.metadata().ok().map(|m| file_stamp(&m));
        let counts = fs::read_to_string(&path)
            .ok()
            .filter(|text| stamp.is_some() && text.lines().next() == stamp.as_deref())
            .map(|text| {
                text.lines()
                    .skip(1)
                    .filter_map(|line| {
                        let (table, count) = line.split_once('\t')?;
                        Some((table.to_owned(), count.parse().ok()?))
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            path: path.into(),
            stamp,
            counts,
            changed: false,
        }
    }

    fn get(
        &mut self,
        table: &str,
        count: impl FnOnce() -> Result<u64, String>,
    ) -> Result<u64, String> {
        if let Some(&cached) = self.counts.get(table) {
            return Ok(cached);
        }
        let counted = count()?;
        self.counts.insert(table.to_owned(), counted);
        self.changed = true;
        Ok(counted)
    }

    /// Best effort: a read-only sources directory just counts every time.
    fn save(&self) {
        let Some(stamp) = self.stamp.as_ref().filter(|_| self.changed) else {
            return;
        };
        let mut text = format!("{stamp}\n");
        for (table, count) in &self.counts {
            text.push_str(&format!("{table}\t{count}\n"));
        }
        let partial = self
            .path
            .with_extension(format!("geometry-counts.{}.partial", std::process::id()));
        if fs::write(&partial, text).is_err() || fs::rename(&partial, &self.path).is_err() {
            let _ = fs::remove_file(partial);
        }
    }
}

fn valid_ne_table(table: &str) -> bool {
    table
        .as_bytes()
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(b"ne_"))
        && table.as_bytes()[3..]
            .iter()
            .copied()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn used_ne_table(table: &str) -> bool {
    // Exact source-table hooks in the pinned profile. Nonspatial helper tables
    // such as ne_10m_admin_0_names may have NULL CRS metadata and are not inputs.
    matches!(
        table,
        "ne_10m_admin_0_boundary_lines_land"
            | "ne_10m_admin_0_countries"
            | "ne_10m_admin_1_states_provinces"
            | "ne_10m_admin_1_states_provinces_lines"
            | "ne_10m_antarctic_ice_shelves_polys"
            | "ne_10m_geography_marine_polys"
            | "ne_10m_glaciated_areas"
            | "ne_10m_lakes"
            | "ne_10m_ocean"
            | "ne_10m_populated_places"
            | "ne_110m_admin_0_boundary_lines_land"
            | "ne_110m_glaciated_areas"
            | "ne_110m_lakes"
            | "ne_110m_ocean"
            | "ne_110m_rivers_lake_centerlines"
            | "ne_50m_admin_0_boundary_lines_land"
            | "ne_50m_antarctic_ice_shelves_polys"
            | "ne_50m_glaciated_areas"
            | "ne_50m_lakes"
            | "ne_50m_ocean"
            | "ne_50m_rivers_lake_centerlines"
            | "ne_50m_urban_areas"
    )
}

/// Bounded WKB/EWKB reader. The pinned NE SQLite declares WKB, not SpatiaLite.
fn decode_wkb(bytes: &[u8]) -> Result<Geometry<f64>, String> {
    struct Reader<'a> {
        bytes: &'a [u8],
        offset: usize,
    }
    impl Reader<'_> {
        fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
            let end = self.offset.checked_add(N).ok_or("WKB offset overflow")?;
            let bytes = self.bytes.get(self.offset..end).ok_or("Truncated WKB")?;
            self.offset = end;
            bytes.try_into().map_err(|_| "Invalid WKB field".into())
        }
        fn u32(&mut self, le: bool) -> Result<u32, String> {
            let b = self.take()?;
            Ok(if le {
                u32::from_le_bytes(b)
            } else {
                u32::from_be_bytes(b)
            })
        }
        fn f64(&mut self, le: bool) -> Result<f64, String> {
            let b = self.take()?;
            Ok(if le {
                f64::from_le_bytes(b)
            } else {
                f64::from_be_bytes(b)
            })
        }
        fn count(&mut self, le: bool) -> Result<usize, String> {
            let count = self.u32(le)? as usize;
            if count > self.bytes.len().saturating_sub(self.offset) {
                return Err("WKB count exceeds remaining bytes".into());
            }
            Ok(count)
        }
        fn coord(&mut self, le: bool, dimensions: u32) -> Result<Coord<f64>, String> {
            let c = Coord {
                x: self.f64(le)?,
                y: self.f64(le)?,
            };
            for _ in 2..dimensions {
                self.f64(le)?;
            }
            if !c.x.is_finite() || !c.y.is_finite() {
                return Err("Non-finite WKB coordinate".into());
            }
            Ok(c)
        }
        fn line(&mut self, le: bool, dimensions: u32) -> Result<LineString<f64>, String> {
            let n = self.count(le)?;
            let mut points = Vec::with_capacity(n.min(4096));
            for _ in 0..n {
                points.push(self.coord(le, dimensions)?);
            }
            Ok(LineString(points))
        }
        fn geometry(&mut self, depth: usize) -> Result<Geometry<f64>, String> {
            if depth > 32 {
                return Err("WKB nesting exceeds limit".into());
            }
            let le = match self.take::<1>()?[0] {
                0 => false,
                1 => true,
                _ => return Err("Invalid WKB byte order".into()),
            };
            let encoded = self.u32(le)?;
            let raw = encoded & 0x0fff_ffff;
            let dims = 2
                + u32::from(encoded & 0x8000_0000 != 0 || raw / 1000 == 1 || raw / 1000 == 3)
                + u32::from(encoded & 0x4000_0000 != 0 || raw / 1000 == 2 || raw / 1000 == 3);
            if encoded & 0x2000_0000 != 0 {
                let srid = self.u32(le)?;
                if srid != 4326 && srid != 0 {
                    return Err(format!("Unsupported EWKB SRID {srid}"));
                }
            }
            match raw % 1000 {
                1 => Ok(Point(self.coord(le, dims)?).into()),
                2 => Ok(self.line(le, dims)?.into()),
                3 => {
                    let count = self.count(le)?;
                    let mut rings = Vec::with_capacity(count.min(4096));
                    for _ in 0..count {
                        rings.push(self.line(le, dims)?);
                    }
                    if rings.is_empty() {
                        return Ok(Polygon::empty().into());
                    }
                    let outer = rings.remove(0);
                    Ok(Polygon::new(outer, rings).into())
                }
                kind @ 4..=7 => {
                    let n = self.count(le)?;
                    let mut geometries = Vec::with_capacity(n.min(4096));
                    for _ in 0..n {
                        geometries.push(self.geometry(depth + 1)?);
                    }
                    match kind {
                        4 => geometries
                            .into_iter()
                            .map(|g| match g {
                                Geometry::Point(p) => Ok(p),
                                _ => Err("Non-point in WKB MultiPoint".to_string()),
                            })
                            .collect::<Result<Vec<_>, _>>()
                            .map(|v| MultiPoint(v).into()),
                        5 => geometries
                            .into_iter()
                            .map(|g| match g {
                                Geometry::LineString(p) => Ok(p),
                                _ => Err("Non-line in WKB MultiLineString".to_string()),
                            })
                            .collect::<Result<Vec<_>, _>>()
                            .map(|v| MultiLineString(v).into()),
                        6 => geometries
                            .into_iter()
                            .map(|g| match g {
                                Geometry::Polygon(p) => Ok(p),
                                _ => Err("Non-polygon in WKB MultiPolygon".to_string()),
                            })
                            .collect::<Result<Vec<_>, _>>()
                            .map(|v| MultiPolygon(v).into()),
                        _ => Ok(Geometry::GeometryCollection(GeometryCollection(geometries))),
                    }
                }
                kind => Err(format!("Unsupported WKB geometry type {kind}")),
            }
        }
    }
    let mut reader = Reader { bytes, offset: 0 };
    let geometry = reader.geometry(0)?;
    if reader.offset != bytes.len() {
        return Err("Trailing bytes after WKB geometry".into());
    }
    Ok(geometry)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_stamps_lead_with_version_1() {
        assert_eq!(CACHE_VERSION, 1);
        let file = tempfile::NamedTempFile::new().unwrap();
        let stamp = file_stamp(&file.as_file().metadata().unwrap());
        assert!(stamp.starts_with("v1 0 "), "{stamp}");
    }
    #[test]
    fn pinned_natural_earth_ids_use_global_rows_not_ogc_fid() {
        let evidence: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/omt-natural-earth-global-ids.json"
        )))
        .unwrap();
        for case in evidence["cases"].as_array().unwrap() {
            let first = case["table_first_id"].as_u64().unwrap();
            let row = case["row"].as_u64().unwrap();
            let ogc_fid = case["ogc_fid"].as_u64().unwrap();
            let reference = case["reference_feature_id"].as_u64().unwrap();
            assert_eq!((first + row - 1) * 10, reference, "{case}");
            assert_ne!(ogc_fid * 10, reference, "{case}");
        }
    }
    #[test]
    fn natural_earth_ids_follow_all_spatial_rows_in_jdbc_table_order() {
        // NaturalEarthReader.java uses JDBC getTables order and one global ID.
        // A profile-ignored table and an out-of-region row still consume IDs.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ne.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE geometry_columns(f_table_name TEXT, f_geometry_column TEXT, srid INTEGER, geometry_format TEXT);
             CREATE TABLE ne_50m_admin_0_boundary_lines_land(ogc_fid INTEGER, GEOMETRY BLOB);
             CREATE TABLE ne_10m_unused_spatial(GEOMETRY BLOB);
             CREATE TABLE ne_10m_admin_0_boundary_lines_land(ogc_fid INTEGER, GEOMETRY BLOB);
             INSERT INTO geometry_columns VALUES('ne_10m_admin_0_boundary_lines_land','GEOMETRY',4326,'WKB');
             INSERT INTO geometry_columns VALUES('ne_50m_admin_0_boundary_lines_land','GEOMETRY',4326,'WKB');",
        )
        .unwrap();
        let point = |lon: f64, lat: f64| {
            let mut bytes = vec![1, 1, 0, 0, 0];
            bytes.extend(lon.to_le_bytes());
            bytes.extend(lat.to_le_bytes());
            bytes
        };
        db.execute(
            "INSERT INTO ne_10m_admin_0_boundary_lines_land VALUES(900, ?1)",
            [point(0., 0.)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO ne_10m_admin_0_boundary_lines_land VALUES(901, ?1)",
            [point(19., 50.)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO ne_10m_unused_spatial VALUES(?1)",
            [point(19., 50.)],
        )
        .unwrap();
        db.execute(
            "INSERT INTO ne_50m_admin_0_boundary_lines_land VALUES(700, ?1)",
            [point(19., 50.)],
        )
        .unwrap();
        drop(db);
        let mut emitted = Vec::new();
        read_natural_earth(
            &path,
            CompileBounds::new(51., 49., 21., 18.),
            &Control::detached(),
            &mut |source| {
                emitted.push((source.source_layer, source.id, source.osm_id));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            emitted,
            [
                ("ne_10m_admin_0_boundary_lines_land".into(), 20, 2),
                ("ne_50m_admin_0_boundary_lines_land".into(), 40, 4)
            ]
        );
    }
    #[test]
    fn unused_ne_table_with_null_crs_is_ignored() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ne.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE geometry_columns(f_table_name TEXT, f_geometry_column TEXT, srid INTEGER, geometry_format TEXT); INSERT INTO geometry_columns VALUES('ne_10m_admin_0_names','GEOMETRY',NULL,NULL);").unwrap();
        drop(db);
        read_natural_earth(
            &path,
            CompileBounds::new(51., 49., 21., 18.),
            &Control::detached(),
            &mut |_| panic!("unused helper emitted"),
        )
        .unwrap();
    }
    #[test]
    fn wkb_endianness_and_truncation() {
        for le in [true, false] {
            let mut b = vec![u8::from(le)];
            b.extend(if le {
                1u32.to_le_bytes()
            } else {
                1u32.to_be_bytes()
            });
            for v in [19.5f64, 50.1] {
                b.extend(if le { v.to_le_bytes() } else { v.to_be_bytes() });
            }
            assert_eq!(
                decode_wkb(&b).unwrap(),
                Geometry::Point(Point::new(19.5, 50.1))
            );
            assert!(decode_wkb(&b[..b.len() - 1]).is_err());
        }
    }
    #[test]
    fn hostile_wkb_count_is_rejected_before_allocation() {
        let b = [1, 2, 0, 0, 0, 255, 255, 255, 255];
        assert!(decode_wkb(&b).unwrap_err().contains("count"));
    }
    #[test]
    fn region_filter_includes_crossing_geometry_not_only_vertices() {
        let line: Geometry = LineString::from(vec![(0., 50.), (40., 50.)]).into();
        assert!(intersects_bounds(
            &line,
            CompileBounds::new(51., 49., 21., 18.)
        ));
        // Bounds crossing the antimeridian, from 170 to -170.
        let wrapped = CompileBounds::new(51., 49., -170., 170.);
        let line = |a: f64, b: f64| Geometry::from(LineString::from(vec![(a, 50.), (b, 50.)]));
        assert!(intersects_bounds(&line(175., 176.), wrapped));
        assert!(intersects_bounds(&line(-176., -175.), wrapped));
        assert!(!intersects_bounds(&line(-160., 160.), wrapped));
    }
    #[test]
    fn header_box_prefilter_emits_what_a_full_scan_emits() {
        use shapefile::{Point as ShpPoint, Polyline, dbase};
        let temp = tempfile::tempdir().unwrap();
        let shp = temp.path().join("water.shp");
        let mercator = |lon: f64, lat: f64| {
            ShpPoint::new(
                lon.to_radians() * MERCATOR_RADIUS,
                (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0)
                    .tan()
                    .ln()
                    * MERCATOR_RADIUS,
            )
        };
        let lines = [
            [(19.5, 50.0), (19.6, 50.1)], // inside
            [(0.0, 0.0), (1.0, 1.0)],     // far away
            [(10.0, 50.0), (30.0, 50.0)], // crosses without a vertex inside
            [(18.0, 51.0), (21.0, 51.0)], // just north of the bounds
            [(17.0, 49.0), (18.0, 49.5)], // ends exactly on the west edge
        ];
        {
            let table = dbase::TableWriterBuilder::new()
                .add_character_field("name".try_into().unwrap(), 10);
            let mut writer = shapefile::Writer::from_path(&shp, table).unwrap();
            for (index, [a, b]) in lines.iter().enumerate() {
                let mut record = dbase::Record::default();
                record.insert("name".into(), index.to_string().into());
                let line = Polyline::new(vec![mercator(a.0, a.1), mercator(b.0, b.1)]);
                writer.write_shape_and_record(&line, &record).unwrap();
            }
        }
        let bounds = CompileBounds::new(50.5, 49.0, 21.0, 18.0);
        let read = || {
            let mut ids = Vec::new();
            read_shapes(
                &shp,
                "water_polygons",
                true,
                bounds,
                &Control::detached(),
                &mut |source| {
                    ids.push((source.id, source.tags["name"].clone()));
                    Ok(())
                },
            )
            .unwrap();
            ids
        };
        assert_eq!(candidate_records(&shp, true, bounds), Some(vec![0, 2, 4]));
        let filtered = read();
        fs::remove_file(shp.with_extension("shx")).unwrap();
        assert_eq!(candidate_records(&shp, true, bounds), None);
        assert_eq!(filtered, read());
        assert_eq!(
            filtered,
            // Numbered by the shapes read, as Pomorskie's ocean polygons
            // are 10, 20 and 30 in Planetiler, not by record.
            [(10, "0".into()), (20, "2".into()), (30, "4".into())]
        );
        // Bounds from 19.65 east round the world to 17.5 leave out the line
        // between those longitudes, and keep the one crossing them and the
        // one ending west of 17.5. The index removed above is written again.
        fs::remove_file(&shp).unwrap();
        let table =
            dbase::TableWriterBuilder::new().add_character_field("name".try_into().unwrap(), 10);
        let mut writer = shapefile::Writer::from_path(&shp, table).unwrap();
        for (index, [a, b]) in lines.iter().enumerate() {
            let mut record = dbase::Record::default();
            record.insert("name".into(), index.to_string().into());
            let line = Polyline::new(vec![mercator(a.0, a.1), mercator(b.0, b.1)]);
            writer.write_shape_and_record(&line, &record).unwrap();
        }
        drop(writer);
        let wrapped = CompileBounds::new(50.5, 49.0, 17.5, 19.65);
        assert_eq!(candidate_records(&shp, true, wrapped), Some(vec![2, 4]));
    }
    #[test]
    fn shapes_are_read_when_their_geometry_meets_the_bounds() {
        // GeoTools' BBOX filter tests the geometry: an L whose envelope
        // covers the bounds but whose arms miss them is neither read nor
        // numbered, as a land box inside a sea polygon's envelope reads
        // nothing from Planetiler's ShapefileReader.
        use shapefile::{Point as ShpPoint, Polygon as ShpPolygon, PolygonRing, dbase};
        let temp = tempfile::tempdir().unwrap();
        let shp = temp.path().join("water_polygons.shp");
        let ring = |points: &[(f64, f64)]| {
            PolygonRing::Outer(points.iter().map(|&(x, y)| ShpPoint::new(x, y)).collect())
        };
        let shapes = [
            // An L around the bounds' south-west, missing them.
            ring(&[
                (0., 0.),
                (0., 10.),
                (1., 10.),
                (1., 1.),
                (10., 1.),
                (10., 0.),
                (0., 0.),
            ]),
            // A square holding them.
            ring(&[(4., 4.), (4., 6.), (6., 6.), (6., 4.), (4., 4.)]),
        ];
        {
            let table = dbase::TableWriterBuilder::new()
                .add_character_field("name".try_into().unwrap(), 10);
            let mut writer = shapefile::Writer::from_path(&shp, table).unwrap();
            for (index, shape) in shapes.into_iter().enumerate() {
                let mut record = dbase::Record::default();
                record.insert("name".into(), index.to_string().into());
                writer
                    .write_shape_and_record(&ShpPolygon::with_rings(vec![shape]), &record)
                    .unwrap();
            }
        }
        let bounds = CompileBounds::new(5.5, 4.5, 5.5, 4.5);
        let mut read = Vec::new();
        read_shapes(
            &shp,
            "water_polygons",
            false,
            bounds,
            &Control::detached(),
            &mut |source| {
                read.push((source.id, source.tags["name"].clone()));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(read, [(10, "1".to_string())]);
        // The whole world reads every shape.
        let world = CompileBounds::new(90., -90., 180., -180.);
        let mut read = Vec::new();
        read_shapes(
            &shp,
            "water_polygons",
            false,
            world,
            &Control::detached(),
            &mut |source| {
                read.push(source.id);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(read, [10, 20]);
    }
    #[test]
    fn mercator_shapes_unproject_as_geotools_does() {
        // A vertex of relation 8259509's lake centerline (Zatoka Gdańska),
        // and the coordinates Planetiler's ShapefileReader gives it; the
        // old 2 atan(exp(y)) - pi/2 gave latitude ...837.
        use geo::CoordsIter;
        use shapefile::{Point as ShpPoint, Polyline, dbase};
        let temp = tempfile::tempdir().unwrap();
        let shp = temp.path().join("lake_centerline.shp");
        {
            let table = dbase::TableWriterBuilder::new()
                .add_character_field("name".try_into().unwrap(), 10);
            let mut writer = shapefile::Writer::from_path(&shp, table).unwrap();
            let line = Polyline::new(vec![
                ShpPoint::new(2130220.516259894, 7303417.359127985),
                ShpPoint::new(2130320.0, 7303417.0),
            ]);
            let mut record = dbase::Record::default();
            record.insert("name".into(), "bay".to_string().into());
            writer.write_shape_and_record(&line, &record).unwrap();
        }
        let bounds = CompileBounds::new(55.0, 54.0, 20.0, 19.0);
        let mut first = None;
        read_shapes(
            &shp,
            "lake_centerlines",
            true,
            bounds,
            &Control::detached(),
            &mut |source| {
                first = source.geometry.coords_iter().next();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            first,
            Some(Coord {
                x: 19.136096483012402,
                y: 54.697709566658354
            })
        );
    }
    #[test]
    fn natural_earth_counts_are_cached_beside_the_database() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("ne.sqlite");
        let db = Connection::open(&path).unwrap();
        db.execute_batch(
            "CREATE TABLE geometry_columns(f_table_name TEXT, f_geometry_column TEXT, srid INTEGER, geometry_format TEXT);
             CREATE TABLE ne_10m_unused_spatial(GEOMETRY BLOB);
             INSERT INTO ne_10m_unused_spatial VALUES(x'00'), (x'00'), (NULL);",
        )
        .unwrap();
        drop(db);
        let mut counts = GeometryCounts::load(&path);
        assert_eq!(counts.get("ne_10m_unused_spatial", || Ok(2)), Ok(2));
        counts.save();
        let mut cached = GeometryCounts::load(&path);
        assert_eq!(
            cached.get("ne_10m_unused_spatial", || panic!("not cached")),
            Ok(2)
        );
        // A changed database invalidates the cache. Its size may not change,
        // and its mtime may have one-second resolution, so move the mtime.
        Connection::open(&path)
            .unwrap()
            .execute("INSERT INTO ne_10m_unused_spatial VALUES(x'00')", [])
            .unwrap();
        let file = File::options().write(true).open(&path).unwrap();
        let modified = file.metadata().unwrap().modified().unwrap();
        file.set_modified(modified + std::time::Duration::from_secs(10))
            .unwrap();
        let mut stale = GeometryCounts::load(&path);
        assert_eq!(stale.get("ne_10m_unused_spatial", || Ok(3)), Ok(3));
    }
}
