//! A polygon a compile is limited to, read from GeoJSON or an Osmosis `.poly`
//! file, and the tiles it covers at each zoom. Like Planetiler's `--polygon`,
//! it decides which tiles are written; features in those tiles keep their
//! full geometry.
//!
//! An area may cross the antimeridian either way GeoJSON and `.poly` files
//! express it: as rings whose longitudes run on past ±180 (from 170 to 190,
//! say), or as polygons split at ±180 (RFC 7946 section 3.1.9). Either way the
//! tiles on both sides are covered, and the bounds wrap.
use crate::compile_bounds::narrowest_covering;
use crate::coordinate_groups::{WorldPoint, lng_lat_to_world};
use crate::{CompileBounds, CompileError};
use geo::{BoundingRect, Contains, InteriorPoint};
use geo_types::{Coord, Geometry, LineString, MultiPolygon, Point, Polygon};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

/// The latitude limit of Web Mercator tiles.
const MAX_LATITUDE: f64 = 85.051_128_779_806_59;

/// One or more polygons, in degrees, that a compile is limited to.
#[derive(Debug, Clone, PartialEq)]
pub struct CompileArea {
    polygons: MultiPolygon<f64>,
}

impl CompileArea {
    /// Reads a GeoJSON file (a Polygon or MultiPolygon geometry, a Feature, or
    /// a FeatureCollection, whose polygons are all used) or an Osmosis `.poly`
    /// file, told apart by their content. A file that cannot be read or
    /// parsed is a [`CompileError::InvalidRequest`].
    pub fn read(path: &Path) -> Result<Self, CompileError> {
        Self::read_file(path).map_err(CompileError::InvalidRequest)
    }

    /// The polygons of a GeoJSON text, as [`read`](Self::read) takes it.
    pub fn from_geojson(text: &str) -> Result<Self, CompileError> {
        Self::parse_geojson(text).map_err(CompileError::InvalidRequest)
    }

    /// The polygons of an Osmosis `.poly` text, as [`read`](Self::read)
    /// takes it.
    pub fn from_poly(text: &str) -> Result<Self, CompileError> {
        Self::parse_poly(text).map_err(CompileError::InvalidRequest)
    }

    /// An area of `polygons`, in degrees. There must be at least one, and
    /// every ring needs three distinct points within latitude ±90 and
    /// longitude ±540, not all on one line: a ring crossing the antimeridian may run on past
    /// ±180, by less than one turn, as long as its polygon spans at most 360
    /// degrees of longitude.
    pub fn from_polygons(polygons: MultiPolygon<f64>) -> Result<Self, CompileError> {
        Self::check_polygons(polygons).map_err(CompileError::InvalidRequest)
    }

    /// Reads a GeoJSON file (a Polygon or MultiPolygon geometry, a Feature, or
    /// a FeatureCollection, whose polygons are all used) or an Osmosis `.poly`
    /// file, told apart by their content.
    fn read_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        let area = if text.trim_start().starts_with('{') {
            Self::parse_geojson(&text)
        } else {
            Self::parse_poly(&text)
        };
        area.map_err(|e| format!("{}: {e}", path.display()))
    }

    fn parse_geojson(text: &str) -> Result<Self, String> {
        let geojson: geojson::GeoJson =
            text.parse().map_err(|e| format!("invalid GeoJSON: {e}"))?;
        // The conversion below indexes each position's first two numbers.
        if let Some(position) = short_position(&geojson) {
            return Err(format!(
                "invalid GeoJSON: a position needs a longitude and a latitude, found {position:?}"
            ));
        }
        let collection = geo_types::GeometryCollection::<f64>::try_from(&geojson)
            .map_err(|e| format!("invalid GeoJSON geometry: {e}"))?;
        let mut polygons = Vec::new();
        let mut pending = collection.0;
        while let Some(geometry) = pending.pop() {
            match geometry {
                Geometry::Polygon(polygon) => polygons.push(polygon),
                Geometry::MultiPolygon(multi) => polygons.extend(multi.0),
                Geometry::GeometryCollection(nested) => pending.extend(nested.0),
                other => {
                    return Err(format!(
                        "only polygons can limit a compile, found a {}",
                        geometry_name(&other)
                    ));
                }
            }
        }
        polygons.reverse();
        Self::check_polygons(MultiPolygon(polygons))
    }

    /// Parses the Osmosis polygon format: a name line, then sections of
    /// `lon lat` lines, each named on its own line and closed by `END`, the
    /// file closed by a final `END`. A section whose name starts with `!` is
    /// a hole in the polygon that contains it.
    fn parse_poly(text: &str) -> Result<Self, String> {
        let mut lines = text
            .lines()
            .enumerate()
            .map(|(i, line)| (i + 1, line.trim()))
            .filter(|(_, line)| !line.is_empty());
        lines.next().ok_or("empty polygon file")?;
        let mut outers = Vec::new();
        let mut holes = Vec::new();
        loop {
            let (number, name) = lines.next().ok_or("polygon file ends without END")?;
            if name == "END" {
                break;
            }
            let mut ring = Vec::new();
            loop {
                let (number, line) = lines
                    .next()
                    .ok_or_else(|| format!("section on line {number} has no END"))?;
                if line == "END" {
                    break;
                }
                let mut values = line.split_whitespace().map(str::parse::<f64>);
                match (values.next(), values.next(), values.next()) {
                    (Some(Ok(x)), Some(Ok(y)), None) => ring.push(Coord { x, y }),
                    _ => return Err(format!("line {number}: expected 'lon lat', got '{line}'")),
                }
            }
            if name.starts_with('!') {
                holes.push((number, LineString::new(ring)));
            } else {
                outers.push(Polygon::new(LineString::new(ring), vec![]));
            }
        }
        if let Some((number, line)) = lines.next() {
            return Err(format!(
                "line {number}: unexpected '{line}' after the final END"
            ));
        }
        for (number, hole) in holes {
            let first = hole
                .0
                .first()
                .copied()
                .ok_or(format!("hole on line {number} is empty"))?;
            let outer = outers
                .iter_mut()
                .find(|outer| outer.contains(&Point::from(first)))
                .ok_or_else(|| format!("hole on line {number} lies in no polygon"))?;
            outer.interiors_push(hole);
        }
        Self::check_polygons(MultiPolygon(outers))
    }

    /// Checks that there is at least one polygon and that every ring has three
    /// distinct points within latitude ±90 and longitude ±540, not all on one
    /// line: a ring crossing
    /// the antimeridian may run on past ±180, by less than one turn, as long
    /// as its polygon spans at most 360 degrees of longitude.
    fn check_polygons(polygons: MultiPolygon<f64>) -> Result<Self, String> {
        if polygons.0.is_empty() {
            return Err("no polygons".into());
        }
        for polygon in &polygons {
            for ring in std::iter::once(polygon.exterior()).chain(polygon.interiors()) {
                if ring.0.len() < 4 {
                    return Err("a ring needs at least three distinct points".into());
                }
                if let Some(c) = ring.0.iter().find(|c| {
                    !(c.x.is_finite() && c.y.is_finite()) || c.x.abs() > 540.0 || c.y.abs() > 90.0
                }) {
                    return Err(format!(
                        "coordinate {} {} is not a longitude and latitude",
                        c.x, c.y
                    ));
                }
                if encloses_no_area(ring) {
                    return Err(format!(
                        "a ring encloses no area: its points, from {} {}, lie on one line",
                        ring.0[0].x, ring.0[0].y
                    ));
                }
            }
            if let Some(rect) = polygon.bounding_rect()
                && rect.width() > 360.0
            {
                return Err(format!(
                    "a polygon spans {} degrees of longitude, more than the world's 360",
                    rect.width()
                ));
            }
        }
        Ok(Self { polygons })
    }

    /// The bounding box, its latitudes limited to Web Mercator's. Its
    /// longitudes are the narrowest range holding every polygon, which wraps
    /// when the polygons cross the antimeridian or lie close to it on both
    /// sides, and otherwise is the plain bounding box.
    pub fn bounds(&self) -> CompileBounds {
        let rect = self
            .polygons
            .bounding_rect()
            .expect("validated as non-empty");
        let pieces = self
            .polygons
            .iter()
            .filter_map(|polygon| polygon.bounding_rect())
            .flat_map(|rect| {
                let (west, east) = (rect.min().x, rect.max().x);
                if east - west >= 360.0 {
                    return vec![(-180.0, 180.0)];
                }
                // Moved a whole number of turns so the polygon starts within
                // ±180; one crossing the antimeridian ends past 180.
                let shift = 360.0 * ((west + 180.0) / 360.0).floor();
                let (west, east) = (west - shift, east - shift);
                if east <= 180.0 {
                    vec![(west, east)]
                } else {
                    vec![(west, 180.0), (-180.0, east - 360.0)]
                }
            })
            .collect();
        let (west, east) = narrowest_covering(pieces).expect("validated as non-empty");
        CompileBounds::new(
            rect.max().y.min(MAX_LATITUDE),
            rect.min().y.max(-MAX_LATITUDE),
            east,
            west,
        )
    }

    /// Whether the area holds the point, `longitude` within ±180, in any of
    /// the copies a whole turn apart that a polygon running past ±180 has.
    pub fn contains(&self, longitude: f64, latitude: f64) -> bool {
        [0.0, -360.0, 360.0].into_iter().any(|turn| {
            self.polygons
                .contains(&Point::new(longitude + turn, latitude))
        })
    }

    /// A point inside the area, as longitude (within ±180) and latitude.
    pub fn interior_point(&self) -> Option<(f64, f64)> {
        self.polygons.interior_point().map(|p| {
            let longitude = if p.x().abs() <= 180.0 {
                p.x()
            } else {
                (p.x() + 180.0).rem_euclid(360.0) - 180.0
            };
            (longitude, p.y())
        })
    }
}

/// Whether a closed ring's points all lie on one line, so that it bounds
/// nothing. Such a ring is no area to limit a compile to, and geo's
/// algorithms assume it away: with a hole beside it, `interior_point` trips
/// a consistency assertion in geo's relate (found by the `polygon_geojson`
/// fuzz target). The shoelace sum is taken relative to the first point,
/// which keeps it exact for points on a line of constant longitude or
/// latitude.
fn encloses_no_area(ring: &LineString<f64>) -> bool {
    let origin = ring.0[0];
    let twice_area: f64 = ring
        .0
        .windows(2)
        .map(|pair| {
            let (a, b) = (pair[0] - origin, pair[1] - origin);
            a.x * b.y - b.x * a.y
        })
        .sum();
    twice_area == 0.0
}

/// The tiles a compile writes: those within its bounds and, when it is
/// limited to an area, covered by it.
#[derive(Debug, Clone)]
pub(crate) struct Region {
    pub(crate) bounds: CompileBounds,
    pub(crate) coverage: Option<Arc<TileCoverage>>,
}

impl From<CompileBounds> for Region {
    fn from(bounds: CompileBounds) -> Self {
        Self {
            bounds,
            coverage: None,
        }
    }
}

impl Region {
    /// Whether tile `z/x/y` is in the area; tiles outside the bounds are
    /// excluded separately.
    pub(crate) fn covers(&self, z: u8, x: u32, y: u32) -> bool {
        self.coverage
            .as_ref()
            .is_none_or(|coverage| coverage.contains(z, x, y))
    }
}

/// The first position with fewer than two numbers, if any.
fn short_position(geojson: &geojson::GeoJson) -> Option<Vec<f64>> {
    use geojson::{GeoJson, GeometryValue};
    let mut pending: Vec<&geojson::Geometry> = match geojson {
        GeoJson::Geometry(geometry) => vec![geometry],
        GeoJson::Feature(feature) => feature.geometry.iter().collect(),
        GeoJson::FeatureCollection(collection) => collection
            .features
            .iter()
            .filter_map(|feature| feature.geometry.as_ref())
            .collect(),
    };
    while let Some(geometry) = pending.pop() {
        let positions: Box<dyn Iterator<Item = &geojson::Position>> = match &geometry.value {
            GeometryValue::Point { coordinates } => Box::new(std::iter::once(coordinates)),
            GeometryValue::MultiPoint { coordinates }
            | GeometryValue::LineString { coordinates } => Box::new(coordinates.iter()),
            GeometryValue::MultiLineString { coordinates }
            | GeometryValue::Polygon { coordinates } => Box::new(coordinates.iter().flatten()),
            GeometryValue::MultiPolygon { coordinates } => {
                Box::new(coordinates.iter().flatten().flatten())
            }
            GeometryValue::GeometryCollection { geometries } => {
                pending.extend(geometries);
                Box::new(std::iter::empty())
            }
        };
        if let Some(short) = positions.into_iter().find(|p| p.len() < 2) {
            return Some(short.as_slice().to_vec());
        }
    }
    None
}

fn geometry_name(geometry: &Geometry<f64>) -> &'static str {
    match geometry {
        Geometry::Point(_) => "Point",
        Geometry::MultiPoint(_) => "MultiPoint",
        Geometry::Line(_) | Geometry::LineString(_) => "LineString",
        Geometry::MultiLineString(_) => "MultiLineString",
        Geometry::Rect(_) => "Rect",
        Geometry::Triangle(_) => "Triangle",
        _ => "geometry",
    }
}

/// The tiles an area covers at each zoom of a compile: those whose square
/// intersects one of its polygons, as runs of columns per row.
#[derive(Debug)]
pub(crate) struct TileCoverage {
    min_zoom: u8,
    /// Per zoom from `min_zoom`: row → sorted, disjoint, inclusive column runs.
    zooms: Vec<BTreeMap<u32, Vec<(u32, u32)>>>,
}

impl TileCoverage {
    pub(crate) fn new(area: &CompileArea, min_zoom: u8, max_zoom: u8) -> Self {
        let zooms = (min_zoom..=max_zoom).map(|z| cover(area, z)).collect();
        Self { min_zoom, zooms }
    }

    pub(crate) fn contains(&self, z: u8, x: u32, y: u32) -> bool {
        let Some(rows) = z
            .checked_sub(self.min_zoom)
            .and_then(|i| self.zooms.get(usize::from(i)))
        else {
            return true;
        };
        rows.get(&y).is_some_and(|runs| {
            let at = runs.partition_point(|&(_, end)| end < x);
            runs.get(at).is_some_and(|&(start, _)| start <= x)
        })
    }
}

/// Covers the tiles at zoom `z` that the boundary passes through, then fills
/// each row between them: a run of tiles no boundary passes through lies
/// wholly inside or wholly outside, which its first tile's center decides.
///
/// Columns are counted on from the world's edges for rings running past
/// ±180, and folded back into the world at the end. A ring ending exactly on
/// the world's (or that extension's) last edge covers the column inside it,
/// not the one beyond.
fn cover(area: &CompileArea, z: u8) -> BTreeMap<u32, Vec<(u32, u32)>> {
    let last = (1_u32 << z) - 1;
    let width = i64::from(last) + 1;
    let project = |c: &Coord<f64>| lng_lat_to_world(c.x, c.y.clamp(-MAX_LATITUDE, MAX_LATITUDE), z);
    let polygons = area
        .polygons
        .iter()
        .map(|polygon| {
            std::iter::once(polygon.exterior())
                .chain(polygon.interiors())
                .map(|ring| ring.0.iter().map(project).collect::<Vec<_>>())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let mut boundary: BTreeMap<u32, Vec<i64>> = BTreeMap::new();
    let cell = |v: f64| (v.floor().max(0.0) as u32).min(last);
    let (min_x, max_x) = polygons
        .iter()
        .flatten()
        .flatten()
        .fold((0.0_f64, f64::from(last) + 1.0), |(lo, hi), p| {
            (lo.min(p.x), hi.max(p.x))
        });
    let (first_column, last_column) = (min_x.floor() as i64, max_x.ceil() as i64 - 1);
    let column_of = |v: f64| (v.floor() as i64).clamp(first_column, last_column);
    for ring in polygons.iter().flatten() {
        for edge in ring.windows(2) {
            let (a, b) = (edge[0], edge[1]);
            let (x0, x1) = (a.x.min(b.x), a.x.max(b.x));
            for column in column_of(x0)..=column_of(x1) {
                // The part of the edge within the column's x range.
                let (left, right) = (x0.max(column as f64), x1.min((column + 1) as f64));
                let (y0, y1) = if a.x == b.x {
                    (a.y.min(b.y), a.y.max(b.y))
                } else {
                    let y = |x: f64| a.y + (x - a.x) * (b.y - a.y) / (b.x - a.x);
                    (y(left).min(y(right)), y(left).max(y(right)))
                };
                for row in cell(y0)..=cell(y1) {
                    boundary.entry(row).or_default().push(column);
                }
            }
        }
    }
    let inside = |p: WorldPoint| {
        polygons
            .iter()
            .any(|rings| rings.iter().filter(|ring| crosses_left(ring, p)).count() % 2 == 1)
    };
    boundary
        .into_iter()
        .map(|(row, mut columns)| {
            columns.sort_unstable();
            columns.dedup();
            let mut runs: Vec<(i64, i64)> = Vec::new();
            for (i, &column) in columns.iter().enumerate() {
                match runs.last_mut() {
                    Some(run) if run.1 + 1 == column => run.1 = column,
                    _ => runs.push((column, column)),
                }
                if let Some(&next) = columns.get(i + 1)
                    && next > column + 1
                    && inside(WorldPoint::new(column as f64 + 1.5, f64::from(row) + 0.5))
                {
                    runs.last_mut().unwrap().1 = next - 1;
                }
            }
            (row, fold_runs(runs, width))
        })
        .collect()
}

/// Folds runs of columns counted past the world's edges back into the world
/// `width` columns wide, as sorted, disjoint inclusive runs. Runs within the
/// world are returned as they are.
fn fold_runs(runs: Vec<(i64, i64)>, width: i64) -> Vec<(u32, u32)> {
    let last = width - 1;
    if runs.iter().all(|&(start, end)| start >= 0 && end <= last) {
        return runs
            .into_iter()
            .map(|(start, end)| (start as u32, end as u32))
            .collect();
    }
    let mut folded = Vec::new();
    for (start, end) in runs {
        if end - start >= last {
            folded.push((0, last));
            continue;
        }
        let start_in = start.rem_euclid(width);
        let end_in = start_in + (end - start);
        if end_in <= last {
            folded.push((start_in, end_in));
        } else {
            folded.push((start_in, last));
            folded.push((0, end_in - width));
        }
    }
    folded.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (start, end) in folded {
        let (start, end) = (start as u32, end as u32);
        match merged.last_mut() {
            Some(run) if start <= run.1 + 1 => run.1 = run.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Whether a ray from `p` towards negative x crosses `ring` an odd number of
/// times (even-odd rule), counted per edge.
fn crosses_left(ring: &[WorldPoint], p: WorldPoint) -> bool {
    let mut odd = false;
    for edge in ring.windows(2) {
        let (a, b) = (edge[0], edge[1]);
        if (a.y > p.y) != (b.y > p.y) && p.x > a.x + (p.y - a.y) * (b.x - a.x) / (b.y - a.y) {
            odd = !odd;
        }
    }
    odd
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Intersects;
    use geo_types::{Rect, polygon};

    const POLY: &str = "\
monaco
area
  7.40 43.72
  7.45 43.72
  7.45 43.76
  7.40 43.76
END
!lake
  7.42 43.735
  7.43 43.735
  7.43 43.745
  7.42 43.745
END
second
  7.50 43.72
  7.51 43.72
  7.51 43.73
END
END
";

    #[test]
    fn reads_poly_files_with_holes() {
        let area = CompileArea::parse_poly(POLY).unwrap();
        assert_eq!(area.polygons.0.len(), 2);
        assert_eq!(area.polygons.0[0].interiors().len(), 1);
        assert!(area.contains(7.41, 43.73));
        assert!(!area.contains(7.425, 43.74), "inside the hole");
        assert!(area.contains(7.505, 43.722));
        assert_eq!(area.bounds(), CompileBounds::new(43.76, 43.72, 7.51, 7.40));
    }

    #[test]
    fn rejects_malformed_poly_files() {
        for (text, message) in [
            ("", "empty polygon file"),
            ("name\narea\n1 2\n", "has no END"),
            ("name\narea\n1 2 3\nEND\nEND\n", "expected 'lon lat'"),
            (
                "name\narea\n0 0\n1 0\n1 1\nEND\nEND\nmore\n",
                "after the final END",
            ),
            (
                "name\n!hole\n5 5\n6 5\n6 6\nEND\nEND\n",
                "lies in no polygon",
            ),
            ("name\narea\n0 0\n1 0\nEND\nEND\n", "three distinct points"),
            ("name\narea\n0 0\n1 0\n2 0\nEND\nEND\n", "encloses no area"),
            ("name\narea\n0 0\n600 0\n1 1\nEND\nEND\n", "not a longitude"),
            (
                "name\narea\n-100 0\n300 0\n1 1\nEND\nEND\n",
                "more than the world's",
            ),
        ] {
            let error = CompileArea::parse_poly(text).unwrap_err();
            assert!(error.contains(message), "{text:?}: {error}");
        }
    }

    #[test]
    fn reads_polygons_from_any_geojson_container() {
        let square = r#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}"#;
        let multi = r#"{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,0]]],[[[2,2],[3,2],[3,3],[2,2]]]]}"#;
        let collection = format!(
            r#"{{"type":"FeatureCollection","features":[
                {{"type":"Feature","properties":{{}},"geometry":{square}}},
                {{"type":"Feature","properties":{{}},"geometry":{multi}}}]}}"#
        );
        assert_eq!(
            CompileArea::parse_geojson(square).unwrap().polygons.0.len(),
            1
        );
        assert_eq!(
            CompileArea::parse_geojson(multi).unwrap().polygons.0.len(),
            2
        );
        let area = CompileArea::parse_geojson(&collection).unwrap();
        assert_eq!(area.polygons.0.len(), 3);
        assert_eq!(area.bounds(), CompileBounds::new(3., 0., 3., 0.));
        let line = r#"{"type":"LineString","coordinates":[[0,0],[1,1]]}"#;
        assert!(
            CompileArea::parse_geojson(line)
                .unwrap_err()
                .contains("found a LineString")
        );
        let empty = r#"{"type":"FeatureCollection","features":[]}"#;
        assert_eq!(
            CompileArea::parse_geojson(empty).unwrap_err(),
            "no polygons"
        );
    }

    /// Found by the polygon_geojson fuzz target: the geojson crate's
    /// conversion panicked on a position with fewer than two numbers.
    #[test]
    fn geojson_positions_without_a_latitude_are_errors() {
        for text in [
            r#"{"type":"MultiPolygon","coordinates":[[[[1719]]],[[[-180,-19],[],[-177,-16],[-180,-16],[-180,-19]]]]}"#,
            r#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1],[0,0]]]}}"#,
            r#"{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[]}]}"#,
        ] {
            let error = CompileArea::from_geojson(text).unwrap_err();
            assert!(error.message().starts_with("invalid GeoJSON"), "{error}");
        }
    }

    /// Found by the polygon_geojson fuzz target: an outer ring whose points
    /// all lie on the meridian 7.40, with a hole beside it, made geo's
    /// `interior_point` panic on a consistency assertion. A ring that
    /// encloses nothing, outer or hole, is now an error.
    #[test]
    fn rings_that_enclose_no_area_are_errors() {
        for text in [
            r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[[[7.40,43.7276],[7.40,43.76],[7.40,43.72]],[[7.40,43.73],[7.42,49.73],[7.23,43.74],[6.41,43.33]]]}}]}"#,
            r#"{"type":"Polygon","coordinates":[[[0,0],[1,1],[2,2],[0,0]]]}"#,
            r#"{"type":"Polygon","coordinates":[[[0,0],[4,0],[4,4],[0,4],[0,0]],[[1,1],[2,1],[3,1],[1,1]]]}"#,
        ] {
            let error = CompileArea::from_geojson(text).unwrap_err();
            assert!(
                error.message().contains("a ring encloses no area"),
                "{text}: {error}"
            );
        }
        // Collinear runs within a ring that does enclose an area are fine.
        let square = r#"{"type":"Polygon","coordinates":[[[0,0],[1,0],[2,0],[2,2],[0,2],[0,0]]]}"#;
        CompileArea::from_geojson(square).unwrap();
    }

    /// Every tile whose square, or one of its copies a world's width east or
    /// west, intersects the polygon, by brute force.
    fn brute_force(area: &CompileArea, z: u8) -> Vec<(u32, u32)> {
        let n = 1_u32 << z;
        let world = area
            .polygons
            .iter()
            .map(|polygon| {
                let ring = |ring: &LineString<f64>| {
                    LineString::from(
                        ring.0
                            .iter()
                            .map(|c| {
                                let p = lng_lat_to_world(c.x, c.y, z);
                                (p.x, p.y)
                            })
                            .collect::<Vec<_>>(),
                    )
                };
                Polygon::new(
                    ring(polygon.exterior()),
                    polygon.interiors().iter().map(ring).collect(),
                )
            })
            .collect::<MultiPolygon<f64>>();
        let mut tiles = Vec::new();
        for y in 0..n {
            for x in 0..n {
                let copies = [-1.0, 0.0, 1.0].map(|turn| {
                    let x = f64::from(x) + turn * f64::from(n);
                    Rect::new(
                        Coord { x, y: f64::from(y) },
                        Coord {
                            x: x + 1.0,
                            y: f64::from(y + 1),
                        },
                    )
                });
                if copies.iter().any(|tile| world.intersects(tile)) {
                    tiles.push((x, y));
                }
            }
        }
        tiles
    }

    /// Checks `area`'s coverage against the brute force at zooms 0 to 7.
    fn assert_covers_brute_force(area: &CompileArea) {
        let coverage = TileCoverage::new(area, 0, 7);
        for z in 0..=7 {
            let n = 1_u32 << z;
            let covered = (0..n)
                .flat_map(|y| (0..n).map(move |x| (x, y)))
                .filter(|&(x, y)| coverage.contains(z, x, y))
                .collect::<Vec<_>>();
            let mut expected = brute_force(area, z);
            expected.sort_by_key(|&(x, y)| (y, x));
            assert_eq!(covered, expected, "z{z}");
        }
    }

    #[test]
    fn covers_polygons_running_past_the_antimeridian() {
        // A ring from 160.3 E to 190.7 (169.3 W) around a hole that itself
        // crosses 180, and one written the other way, from -200.3 (159.7 E)
        // to -170.1, further south.
        let area = CompileArea::check_polygons(MultiPolygon(vec![
            polygon!(
                exterior: [
                    (x: 160.3, y: 50.1), (x: 190.7, y: 50.1), (x: 190.7, y: 71.3),
                    (x: 175.2, y: 66.7), (x: 160.3, y: 71.3),
                ],
                interiors: [[(x: 170.9, y: 55.3), (x: 185.1, y: 55.3), (x: 185.1, y: 60.9), (x: 170.9, y: 60.9)]],
            ),
            polygon!(
                (x: -200.3, y: -30.1), (x: -170.1, y: -12.3), (x: -185.9, y: -5.7),
            ),
        ]))
        .unwrap();
        assert_eq!(
            area.bounds(),
            CompileBounds::new(71.3, -30.1, -169.3, 159.7)
        );
        assert!(area.contains(-175.0, 52.0) && area.contains(165.0, 52.0));
        assert!(!area.contains(179.0, 58.0), "inside the hole");
        assert!(!area.contains(0.0, 52.0));
        let (x, _) = area.interior_point().unwrap();
        assert!(x.abs() <= 180.0);
        assert_covers_brute_force(&area);
        let coverage = TileCoverage::new(&area, 0, 7);
        // Both sides of the antimeridian at z3: columns 7 and 0.
        assert!(coverage.contains(3, 7, 2) && coverage.contains(3, 0, 2));
    }

    #[test]
    fn covers_polygons_split_at_the_antimeridian() {
        // RFC 7946: one polygon split into its parts either side of 180.
        let multi = r#"{"type":"MultiPolygon","coordinates":[
            [[[170.3,-20.1],[180,-20.1],[180,-10.7],[170.3,-15.2],[170.3,-20.1]]],
            [[[-180,-20.1],[-170.7,-20.1],[-175.2,-10.7],[-180,-10.7],[-180,-20.1]]]]}"#;
        let area = CompileArea::parse_geojson(multi).unwrap();
        assert_eq!(
            area.bounds(),
            CompileBounds::new(-10.7, -20.1, -170.7, 170.3)
        );
        assert!(area.contains(179.0, -15.0) && area.contains(-179.0, -15.0));
        assert_covers_brute_force(&area);
        // The same polygon as one ring running past 180, from a .poly file.
        let poly =
            "fiji\n1\n170.3 -20.1\n189.3 -20.1\n184.8 -10.7\n180 -10.7\n170.3 -15.2\nEND\nEND\n";
        let continuous = CompileArea::parse_poly(poly).unwrap();
        assert_eq!(continuous.bounds(), area.bounds());
        assert_covers_brute_force(&continuous);
        for z in 0..=7 {
            let n = 1 << z;
            let (split, whole) = (
                TileCoverage::new(&area, z, z),
                TileCoverage::new(&continuous, z, z),
            );
            for (x, y) in (0..n).flat_map(|y| (0..n).map(move |x| (x, y))) {
                assert_eq!(
                    split.contains(z, x, y),
                    whole.contains(z, x, y),
                    "{z}/{x}/{y}"
                );
            }
        }
    }

    #[test]
    fn folds_runs_into_the_world() {
        assert_eq!(fold_runs(vec![(1, 2), (5, 6)], 8), [(1, 2), (5, 6)]);
        assert_eq!(fold_runs(vec![(6, 9)], 8), [(0, 1), (6, 7)]);
        assert_eq!(fold_runs(vec![(-2, 1)], 8), [(0, 1), (6, 7)]);
        assert_eq!(fold_runs(vec![(-2, 1), (2, 3)], 8), [(0, 3), (6, 7)]);
        assert_eq!(fold_runs(vec![(3, 12)], 8), [(0, 7)]);
    }

    #[test]
    fn covers_exactly_the_tiles_a_polygon_intersects() {
        // An L-shape around a hole, plus a separate triangle, at coordinates
        // that do not fall on tile edges.
        let area = CompileArea::check_polygons(MultiPolygon(vec![
            polygon!(
                exterior: [
                    (x: -71.3, y: -33.1), (x: 52.7, y: -33.1), (x: 52.7, y: 8.9),
                    (x: -11.1, y: 8.9), (x: -11.1, y: 61.3), (x: -71.3, y: 61.3),
                ],
                interiors: [[(x: -50.3, y: -20.7), (x: 20.3, y: -20.7), (x: 20.3, y: 0.3), (x: -50.3, y: 0.3)]],
            ),
            polygon!((x: 100.3, y: 10.1), (x: 160.7, y: 20.3), (x: 120.9, y: 70.2)),
        ]))
        .unwrap();
        assert_covers_brute_force(&area);
        let coverage = TileCoverage::new(&area, 0, 7);
        assert!(!coverage.contains(7, 0, 0));
        assert!(
            coverage.contains(8, 0, 0),
            "zooms outside the range are not limited"
        );
    }
}
