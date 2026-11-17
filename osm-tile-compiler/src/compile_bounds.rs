use serde::{Deserialize, Serialize};

/// Geographic bounds used to constrain a vector compilation, in degrees.
///
/// This deliberately belongs to the compiler rather than the application-wide
/// geo model: compilation only needs an axis-aligned WGS84 extent and its
/// center for PMTiles metadata.
///
/// A box whose `west` is greater than its `east` crosses the antimeridian: it
/// runs from `west` east to 180, then from -180 to `east`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompileBounds {
    pub north: f64,
    pub south: f64,
    pub east: f64,
    pub west: f64,
}

impl CompileBounds {
    pub const fn new(north: f64, south: f64, east: f64, west: f64) -> Self {
        Self {
            north,
            south,
            east,
            west,
        }
    }

    /// Whether the box crosses the antimeridian (`west > east`).
    pub fn wraps(self) -> bool {
        self.west > self.east
    }

    /// The same box with its longitudes within ±180: a longitude beyond that
    /// (as some PBF headers give for an extract crossing the antimeridian,
    /// such as 170 to 190) is moved a whole turn, which makes the box wrap,
    /// and a box at least 360 degrees wide becomes -180 to 180. A wrapped box
    /// that only touches one side of the antimeridian is unwrapped.
    pub fn normalized(self) -> Self {
        let mut bounds = self;
        if bounds.east - bounds.west >= 360.0 {
            (bounds.west, bounds.east) = (-180.0, 180.0);
        }
        let turn = |longitude: f64| {
            if (-180.0..=180.0).contains(&longitude) {
                longitude
            } else {
                (longitude + 180.0).rem_euclid(360.0) - 180.0
            }
        };
        (bounds.west, bounds.east) = (turn(bounds.west), turn(bounds.east));
        if bounds.wraps() && bounds.west == 180.0 {
            bounds.west = -180.0;
        }
        if bounds.wraps() && bounds.east == -180.0 {
            bounds.east = 180.0;
        }
        bounds
    }

    /// The box's longitude ranges, each west to east: one, or for a wrapped
    /// box two, from `west` to 180 and from -180 to `east`.
    pub fn longitude_ranges(self) -> impl Iterator<Item = (f64, f64)> {
        let (first, second) = if self.wraps() {
            ((self.west, 180.0), Some((-180.0, self.east)))
        } else {
            ((self.west, self.east), None)
        };
        std::iter::once(first).chain(second)
    }

    /// The width in degrees of longitude.
    pub fn width(self) -> f64 {
        if self.wraps() {
            self.east - self.west + 360.0
        } else {
            self.east - self.west
        }
    }

    pub fn center_latitude(self) -> f64 {
        (self.north + self.south) / 2.0
    }

    /// The longitude halfway from `west` east to `east`, within ±180.
    pub fn center_longitude(self) -> f64 {
        if !self.wraps() {
            return (self.east + self.west) / 2.0;
        }
        let center = self.west + self.width() / 2.0;
        if center > 180.0 {
            center - 360.0
        } else {
            center
        }
    }

    /// Whether the point lies in the box or on its edge.
    pub fn contains(self, longitude: f64, latitude: f64) -> bool {
        (self.south..=self.north).contains(&latitude)
            && self
                .longitude_ranges()
                .any(|(west, east)| (west..=east).contains(&longitude))
    }

    /// Whether the box meets the longitudes from `west` to `east` (within
    /// ±180, `west <= east`) and the latitudes from `south` to `north`, edges
    /// included.
    pub fn meets(self, west: f64, south: f64, east: f64, north: f64) -> bool {
        north >= self.south
            && south <= self.north
            && self.longitude_ranges().any(|(w, e)| east >= w && west <= e)
    }

    /// The overlap of two bounds, if they share any area. Two boxes, one or
    /// both wrapped, can overlap in two separate pieces (a box from 170 to
    /// -170 and one from -175 to 175 share 170 to 175 and -175 to -170); the
    /// result is then the narrowest box holding both.
    pub fn intersection(self, other: Self) -> Option<Self> {
        if !self.wraps() && !other.wraps() {
            let bounds = Self::new(
                self.north.min(other.north),
                self.south.max(other.south),
                self.east.min(other.east),
                self.west.max(other.west),
            );
            return (bounds.west < bounds.east && bounds.south < bounds.north).then_some(bounds);
        }
        let (north, south) = (self.north.min(other.north), self.south.max(other.south));
        if south >= north {
            return None;
        }
        let pieces = self
            .longitude_ranges()
            .flat_map(|(w, e)| {
                other
                    .longitude_ranges()
                    .map(move |(ow, oe)| (w.max(ow), e.min(oe)))
            })
            .filter(|(w, e)| w < e)
            .collect();
        let (west, east) = narrowest_covering(pieces)?;
        Some(Self::new(north, south, east, west))
    }

    /// The highest zoom at which the bounds fit in one tile, so a viewer
    /// opened there shows all of them.
    pub fn fit_zoom(self) -> u8 {
        let width = self.width() / 360.0;
        let y = |lat: f64| crate::coordinate_groups::lng_lat_to_world(0.0, lat, 0).y;
        let height = (y(self.south) - y(self.north)).abs();
        let extent = width.max(height);
        if extent <= 0.0 || !extent.is_finite() {
            return u8::MAX;
        }
        (-extent.log2()).floor().clamp(0.0, f64::from(u8::MAX)) as u8
    }

    /// The bounds an archive's header declares: these, or for a wrapped box
    /// every longitude. The PMTiles specification does not say how to
    /// declare a box crossing the antimeridian, and its readers expect west
    /// below east: pmtiles.js hands the header's box to MapLibre as the
    /// TileJSON bounds, and MapLibre requests no tile at all for a box whose
    /// west exceeds its east.
    pub fn archive_bounds(self) -> Self {
        if self.wraps() {
            Self::new(self.north, self.south, 180.0, -180.0)
        } else {
            self
        }
    }

    /// The columns of tiles at zoom `z` that the box meets, as inclusive
    /// ranges: one, or two for a wrapped box unless they meet.
    pub(crate) fn tile_columns(self, z: u8) -> impl Iterator<Item = (i32, i32)> {
        let last = (1_i32 << z) - 1;
        let column = |longitude: f64| {
            crate::coordinate_groups::lng_lat_to_world(longitude, 0.0, z)
                .x
                .floor() as i32
        };
        let (west, east) = (column(self.west), column(self.east));
        let (first, second) = if !self.wraps() {
            ((west, east), None)
        } else if west <= east + 1 {
            ((0, last), None)
        } else {
            ((0, east), Some((west, last)))
        };
        std::iter::once(first).chain(second)
    }
}

/// The narrowest longitude range, possibly crossing the antimeridian, that
/// holds every one of `pieces` (each west to east within ±180), as west and
/// east: the circle less the widest gap between pieces. When the gap across
/// the antimeridian is (one of) the widest, the range does not cross it.
pub(crate) fn narrowest_covering(mut pieces: Vec<(f64, f64)>) -> Option<(f64, f64)> {
    pieces.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::new();
    for (west, east) in pieces {
        match merged.last_mut() {
            Some(last) if west <= last.1 => last.1 = last.1.max(east),
            _ => merged.push((west, east)),
        }
    }
    let (first, last) = (*merged.first()?, *merged.last()?);
    let across = first.0 + 360.0 - last.1;
    let widest = merged
        .windows(2)
        .enumerate()
        .map(|(i, pair)| (pair[1].0 - pair[0].1, i))
        .filter(|&(gap, _)| gap > across)
        .max_by(|a, b| a.0.total_cmp(&b.0));
    Some(match widest {
        Some((_, i)) => (merged[i + 1].0, merged[i].1),
        None => (first.0, last.1),
    })
}

/// Where a viewer opening an archive starts: the PMTiles header's center.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ArchiveCenter {
    pub longitude: f64,
    pub latitude: f64,
    pub zoom: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intersects_and_fits_bounds() {
        let a = CompileBounds::new(10.0, 0.0, 10.0, 0.0);
        assert_eq!(
            a.intersection(CompileBounds::new(20.0, 5.0, 8.0, -5.0)),
            Some(CompileBounds::new(10.0, 5.0, 8.0, 0.0))
        );
        assert_eq!(
            a.intersection(CompileBounds::new(20.0, 10.0, 20.0, 10.0)),
            None
        );
        assert_eq!(CompileBounds::new(85.0, -85.0, 180.0, -180.0).fit_zoom(), 0);
        // 10 degrees is 1/36 of the world's width: it fits in a z5 tile (1/32).
        assert_eq!(CompileBounds::new(1.0, 0.0, 10.0, 0.0).fit_zoom(), 5);
        // Monaco, about 0.04 degrees wide.
        assert_eq!(
            CompileBounds::new(43.75169, 43.72335, 7.448637, 7.409205).fit_zoom(),
            13
        );
    }

    #[test]
    fn wrapped_bounds_cross_the_antimeridian() {
        // Fiji: 177 E to 178 W.
        let fiji = CompileBounds::new(-12.0, -21.0, -178.0, 177.0);
        assert!(fiji.wraps());
        assert_eq!(fiji.width(), 5.0);
        assert_eq!(
            fiji.longitude_ranges().collect::<Vec<_>>(),
            [(177.0, 180.0), (-180.0, -178.0)]
        );
        assert_eq!(fiji.center_longitude(), 179.5);
        assert_eq!(
            CompileBounds::new(1.0, 0.0, -170.0, 178.0).center_longitude(),
            -176.0
        );
        assert_eq!(fiji.center_latitude(), -16.5);
        assert!(fiji.contains(179.9, -16.0) && fiji.contains(-179.9, -16.0));
        assert!(!fiji.contains(0.0, -16.0) && !fiji.contains(-170.0, -16.0));
        assert!(fiji.meets(-179.0, -17.0, -178.5, -16.0));
        assert!(!fiji.meets(-177.0, -17.0, 176.0, -16.0));
        assert!(!fiji.meets(178.0, -30.0, 179.0, -25.0));
        assert!(!fiji.meets(-100.0, -17.0, 100.0, -16.0));
        // 5 degrees of longitude would fit in a z6 tile (5.6 degrees wide),
        // but 9 degrees of latitude there need z5.
        assert_eq!(fiji.fit_zoom(), 5);
        assert_eq!(CompileBounds::new(1.0, 0.0, -178.0, 177.0).fit_zoom(), 6);
        assert_eq!(
            fiji.archive_bounds(),
            CompileBounds::new(-12.0, -21.0, 180.0, -180.0)
        );
        let monaco = CompileBounds::new(43.75169, 43.72335, 7.448637, 7.409205);
        assert_eq!(monaco.archive_bounds(), monaco);
        assert_eq!(monaco.width(), monaco.east - monaco.west);
    }

    #[test]
    fn normalizes_longitudes_past_the_antimeridian() {
        let wrapped = CompileBounds::new(1.0, 0.0, -170.0, 170.0);
        assert_eq!(
            CompileBounds::new(1.0, 0.0, 190.0, 170.0).normalized(),
            wrapped
        );
        assert_eq!(
            CompileBounds::new(1.0, 0.0, -170.0, -190.0).normalized(),
            wrapped
        );
        assert_eq!(wrapped.normalized(), wrapped);
        assert_eq!(
            CompileBounds::new(1.0, 0.0, 200.0, -170.0).normalized(),
            CompileBounds::new(1.0, 0.0, 180.0, -180.0)
        );
        // Wrapped, but only on one side of the antimeridian.
        assert_eq!(
            CompileBounds::new(1.0, 0.0, -170.0, 180.0).normalized(),
            CompileBounds::new(1.0, 0.0, -170.0, -180.0)
        );
        assert_eq!(
            CompileBounds::new(1.0, 0.0, -180.0, 170.0).normalized(),
            CompileBounds::new(1.0, 0.0, 180.0, 170.0)
        );
        let monaco = CompileBounds::new(43.75169, 43.72335, 7.448637, 7.409205);
        assert_eq!(monaco.normalized(), monaco);
    }

    #[test]
    fn intersects_wrapped_bounds() {
        let wrapped = CompileBounds::new(10.0, -10.0, -170.0, 170.0);
        let world = CompileBounds::new(85.0, -85.0, 180.0, -180.0);
        assert_eq!(wrapped.intersection(world), Some(wrapped));
        assert_eq!(world.intersection(wrapped), Some(wrapped));
        // One side only.
        assert_eq!(
            wrapped.intersection(CompileBounds::new(5.0, 0.0, -160.0, -175.0)),
            Some(CompileBounds::new(5.0, 0.0, -170.0, -175.0))
        );
        assert_eq!(
            wrapped.intersection(CompileBounds::new(5.0, 0.0, 175.0, 0.0)),
            Some(CompileBounds::new(5.0, 0.0, 175.0, 170.0))
        );
        // Both wrapped.
        assert_eq!(
            wrapped.intersection(CompileBounds::new(5.0, 0.0, -175.0, 175.0)),
            Some(CompileBounds::new(5.0, 0.0, -175.0, 175.0))
        );
        // Two separate pieces, 170 to 175 and -175 to -170: the narrowest
        // box holding both wraps.
        assert_eq!(
            wrapped.intersection(CompileBounds::new(5.0, 0.0, 175.0, -175.0)),
            Some(CompileBounds::new(5.0, 0.0, -170.0, 170.0))
        );
        // Disjoint in longitude, or in latitude.
        assert_eq!(
            wrapped.intersection(CompileBounds::new(5.0, 0.0, 20.0, 10.0)),
            None
        );
        assert_eq!(
            wrapped.intersection(CompileBounds::new(30.0, 20.0, -175.0, 175.0)),
            None
        );
    }

    #[test]
    fn covers_the_narrowest_longitude_range() {
        assert_eq!(narrowest_covering(vec![]), None);
        assert_eq!(
            narrowest_covering(vec![(0.0, 1.0), (5.0, 7.0)]),
            Some((0.0, 7.0))
        );
        // Pieces either side of the antimeridian join across it.
        assert_eq!(
            narrowest_covering(vec![(-180.0, -170.0), (170.0, 180.0)]),
            Some((170.0, -170.0))
        );
        // Alaska with the Aleutians, Hawaii and the lower 48 states.
        assert_eq!(
            narrowest_covering(vec![
                (172.4, 180.0),
                (-180.0, -130.0),
                (-160.3, -154.8),
                (-124.8, -66.9),
            ]),
            Some((172.4, -66.9))
        );
        // A tie between the gap across the antimeridian and another keeps
        // the box unwrapped.
        assert_eq!(
            narrowest_covering(vec![(-90.0, 0.0), (90.0, 90.0)]),
            Some((-90.0, 90.0))
        );
        assert_eq!(
            narrowest_covering(vec![(-180.0, 180.0), (10.0, 20.0)]),
            Some((-180.0, 180.0))
        );
    }

    #[test]
    fn tile_columns_of_wrapped_bounds() {
        let monaco = CompileBounds::new(43.75169, 43.72335, 7.448637, 7.409205);
        assert_eq!(monaco.tile_columns(14).collect::<Vec<_>>(), [(8529, 8530)]);
        let fiji = CompileBounds::new(-12.0, -21.0, -178.0, 177.0);
        // At z3 the columns are 45 degrees wide: 177 is in column 7, -178 in 0.
        assert_eq!(fiji.tile_columns(3).collect::<Vec<_>>(), [(0, 0), (7, 7)]);
        assert_eq!(
            fiji.tile_columns(10).collect::<Vec<_>>(),
            [(0, 5), (1015, 1023)]
        );
        // At z0 both ranges are the one tile.
        assert_eq!(fiji.tile_columns(0).collect::<Vec<_>>(), [(0, 0)]);
        // Ranges that meet become one.
        let nearly_all = CompileBounds::new(1.0, 0.0, 1.0, 2.0);
        assert_eq!(nearly_all.tile_columns(2).collect::<Vec<_>>(), [(0, 3)]);
    }

    #[test]
    fn preserves_the_existing_serialized_shape() {
        let bounds = CompileBounds::new(52.0, 51.0, 20.0, 19.0);
        assert_eq!(
            serde_json::to_value(bounds).unwrap(),
            serde_json::json!({
                "north": 52.0,
                "south": 51.0,
                "east": 20.0,
                "west": 19.0,
            })
        );
    }
}
