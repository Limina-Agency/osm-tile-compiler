//! The part of the world an area-only compile reads OSM data from: the
//! tiles the compile writes at its highest zoom, and one tile around them.
//!
//! A feature reaches a tile when its geometry, widened by the feature's
//! buffer, touches the tile. No buffer is wider than a tile (256 pixels), so
//! an object whose bounding box misses this box cannot reach a tile written
//! at the highest zoom. Objects that do touch it are read whole, with every
//! node, so those tiles get the same features they get from a full read.
use crate::CompileBounds;
use crate::coordinate_groups::lng_lat_to_world;
use geo_types::Coord;

/// The margin around the written tiles, in tiles at the highest zoom: the
/// widest buffer, 256 pixels, and a little for the point buffer's epsilon.
const MARGIN_TILES: f64 = 1.0 + 1.0 / 64.0;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InputArea {
    /// West to east, in degrees; two ranges when the box crosses ±180.
    longitudes: Vec<(f64, f64)>,
    south: f64,
    north: f64,
}

/// A bounding box in degrees, grown point by point.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Extent {
    west: f64,
    east: f64,
    south: f64,
    north: f64,
}

impl Extent {
    pub(crate) const EMPTY: Self = Self {
        west: f64::INFINITY,
        east: f64::NEG_INFINITY,
        south: f64::INFINITY,
        north: f64::NEG_INFINITY,
    };

    pub(crate) fn add(&mut self, latitude: f64, longitude: f64) {
        self.west = self.west.min(longitude);
        self.east = self.east.max(longitude);
        self.south = self.south.min(latitude);
        self.north = self.north.max(latitude);
    }

    pub(crate) fn extend(&mut self, other: &Self) {
        self.west = self.west.min(other.west);
        self.east = self.east.max(other.east);
        self.south = self.south.min(other.south);
        self.north = self.north.max(other.north);
    }

    pub(crate) fn of(coords: &[Coord<f64>]) -> Self {
        let mut extent = Self::EMPTY;
        for c in coords {
            extent.add(c.y, c.x);
        }
        extent
    }

    fn is_empty(&self) -> bool {
        !(self.west <= self.east && self.south <= self.north)
    }
}

fn longitude(x: f64, tiles: f64) -> f64 {
    (x / tiles * 360.0 - 180.0).clamp(-180.0, 180.0)
}

fn latitude(y: f64, tiles: f64) -> f64 {
    let y = y.clamp(0.0, tiles);
    (std::f64::consts::PI * (1.0 - 2.0 * y / tiles))
        .sinh()
        .atan()
        .to_degrees()
}

impl InputArea {
    /// The box around the tiles that `bounds` covers at `max_zoom`.
    pub(crate) fn new(bounds: CompileBounds, max_zoom: u8) -> Self {
        let tiles = f64::from(1u32 << max_zoom);
        let nw = lng_lat_to_world(bounds.west, bounds.north, max_zoom);
        let se = lng_lat_to_world(bounds.east, bounds.south, max_zoom);
        let north = latitude(nw.y.floor() - MARGIN_TILES, tiles);
        let south = latitude(se.y.floor() + 1.0 + MARGIN_TILES, tiles);
        let mut longitudes = Vec::new();
        for (first, last) in bounds.tile_columns(max_zoom) {
            let west = f64::from(first) - MARGIN_TILES;
            let east = f64::from(last) + 1.0 + MARGIN_TILES;
            longitudes.push((longitude(west, tiles), longitude(east, tiles)));
            // The margin continues across ±180.
            if west < 0.0 {
                longitudes.push((longitude(west + tiles, tiles), 180.0));
            }
            if east > tiles {
                longitudes.push((-180.0, longitude(east - tiles, tiles)));
            }
        }
        Self {
            longitudes,
            south,
            north,
        }
    }

    pub(crate) fn contains(&self, latitude: f64, longitude: f64) -> bool {
        (self.south..=self.north).contains(&latitude)
            && self
                .longitudes
                .iter()
                .any(|&(west, east)| (west..=east).contains(&longitude))
    }

    /// Whether the box touches `extent`. An empty extent touches nothing.
    pub(crate) fn intersects(&self, extent: &Extent) -> bool {
        !extent.is_empty()
            && extent.south <= self.north
            && extent.north >= self.south
            && self
                .longitudes
                .iter()
                .any(|&(west, east)| extent.west <= east && extent.east >= west)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_the_written_tiles_and_one_tile_around_them() {
        // Zakopane, about 9 by 8 km.
        let bounds = CompileBounds::new(49.32, 49.25, 20.02, 19.90);
        let area = InputArea::new(bounds, 14);
        assert_eq!(area.longitudes.len(), 1);
        let tile = 360.0 / f64::from(1 << 14);
        let (west, east) = area.longitudes[0];
        // The tile edges lie just outside the bounds, the margin a tile beyond.
        assert!(west < 19.90 - tile && west > 19.90 - 2.1 * tile);
        assert!(east > 20.02 + tile && east < 20.02 + 2.1 * tile);
        assert!(area.north > 49.32 && area.north < 49.32 + 0.03);
        assert!(area.south < 49.25 && area.south > 49.25 - 0.03);
        assert!(area.contains(49.3, 19.95));
        assert!(!area.contains(49.3, 19.7));
        assert!(!area.contains(49.5, 19.95));
        // A line crossing the box with no node in it, and a polygon around it.
        let crossing = Extent::of(&[Coord { x: 19.0, y: 49.3 }, Coord { x: 21.0, y: 49.3 }]);
        assert!(area.intersects(&crossing));
        let around = Extent::of(&[Coord { x: 19.0, y: 49.0 }, Coord { x: 21.0, y: 50.0 }]);
        assert!(area.intersects(&around));
        let beside = Extent::of(&[Coord { x: 21.0, y: 49.0 }, Coord { x: 22.0, y: 50.0 }]);
        assert!(!area.intersects(&beside));
        assert!(!area.intersects(&Extent::EMPTY));
    }

    #[test]
    fn margin_continues_across_the_antimeridian() {
        // Ending in the last column but one at zoom 10, whose tiles are
        // 0.35 degrees wide: the margin reaches a 64th of a tile past 180.
        let tile = 360.0 / 1024.0;
        let east = 180.0 - 1.5 * tile;
        let area = InputArea::new(CompileBounds::new(-16.0, -17.0, east, 178.0), 10);
        assert!(area.contains(-16.5, 179.9));
        assert!(area.contains(-16.5, -180.0 + tile / 128.0));
        assert!(!area.contains(-16.5, -180.0 + tile / 32.0));
        // Fiji: the bounds themselves cross 180.
        let fiji = InputArea::new(CompileBounds::new(-12.0, -21.0, -178.0, 177.0), 14);
        assert!(fiji.contains(-17.0, 179.0));
        assert!(fiji.contains(-17.0, -178.0));
        assert!(!fiji.contains(-17.0, 0.0));
    }
}
