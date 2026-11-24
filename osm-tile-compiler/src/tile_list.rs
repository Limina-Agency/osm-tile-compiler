//! Lists of tiles, one `z/x/y` per line, as osm2pgsql's expire lists are
//! written and Tilekiln and other renderers read them: which tiles `update`'s
//! changes reach, and which tiles `diff` found changed.

use crate::{CompileBounds, ZoomRange};
use std::collections::BTreeSet;
use std::io::{BufWriter, Write};
use std::path::Path;

/// A tile's zoom, column and row; rows count from the north, as in XYZ
/// tile URLs.
pub type TileAddress = (u8, u32, u32);

/// How far around a changed object `update` reaches, in pixels of a 256-pixel
/// tile: the widest buffer any layer renders with, so a label placed next to
/// a tile's edge is still found in the tile beside it.
pub const CHANGED_TILE_BUFFER_PIXELS: f64 = 256.0;

/// A box in degrees, for the tile math.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Degrees {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
}

/// Web Mercator's limit, in degrees of latitude.
const MAX_LATITUDE: f64 = 85.051_128_779_806_59;

fn column(lon: f64, scale: f64) -> f64 {
    (lon + 180.0) / 360.0 * scale
}

fn row(lat: f64, scale: f64) -> f64 {
    let lat = lat.clamp(-MAX_LATITUDE, MAX_LATITUDE).to_radians();
    (1.0 - (lat.tan() + 1.0 / lat.cos()).ln() / std::f64::consts::PI) / 2.0 * scale
}

/// Every tile at `zooms` that a box, widened by `buffer_pixels`, reaches.
/// With `clip`, boxes are first cut to it, as a compile writes only the
/// tiles of its bounds; a clip across the antimeridian is not applied.
pub(crate) fn tiles_reached(
    boxes: &[Degrees],
    zooms: ZoomRange,
    buffer_pixels: f64,
    clip: Option<CompileBounds>,
) -> BTreeSet<TileAddress> {
    let clip = clip.filter(|c| c.west <= c.east && c.east <= 180.0);
    let mut tiles = BTreeSet::new();
    for found in boxes {
        let mut found = *found;
        if let Some(clip) = clip {
            found.west = found.west.max(clip.west);
            found.east = found.east.min(clip.east);
            found.south = found.south.max(clip.south);
            found.north = found.north.min(clip.north);
            if found.west > found.east || found.south > found.north {
                continue;
            }
        }
        for zoom in zooms.min()..=zooms.max() {
            let scale = f64::from(1u32 << zoom);
            let buffer = buffer_pixels / 256.0;
            let last = (1u32 << zoom) - 1;
            let index = |value: f64| (value.floor().max(0.0) as u32).min(last);
            let (x0, x1) = (
                index(column(found.west, scale) - buffer),
                index(column(found.east, scale) + buffer),
            );
            let (y0, y1) = (
                index(row(found.north, scale) - buffer),
                index(row(found.south, scale) + buffer),
            );
            for x in x0..=x1 {
                for y in y0..=y1 {
                    tiles.insert((zoom, x, y));
                }
            }
        }
    }
    tiles
}

/// Writes `tiles` to `path`, which must not exist yet, one `z/x/y` per line.
pub(crate) fn write(
    path: &Path,
    tiles: impl IntoIterator<Item = TileAddress>,
) -> Result<u64, String> {
    let file = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| format!("Failed to create {}: {e}", path.display()))?;
    let mut out = BufWriter::new(file);
    let mut count = 0;
    let result = tiles.into_iter().try_for_each(|(z, x, y)| {
        count += 1;
        writeln!(out, "{z}/{x}/{y}")
    });
    if let Err(e) = result.and_then(|()| out.flush()) {
        drop(out);
        let _ = std::fs::remove_file(path);
        return Err(format!("Failed to write {}: {e}", path.display()));
    }
    Ok(count)
}

/// Reads a tile list: one `z/x/y` per line; blank lines are skipped.
pub fn read_tile_list(path: &Path) -> Result<BTreeSet<TileAddress>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut parts = line.split('/');
            let parsed = (|| {
                let z: u8 = parts.next()?.parse().ok()?;
                let x: u32 = parts.next()?.parse().ok()?;
                let y: u32 = parts.next()?.parse().ok()?;
                (parts.next().is_none() && z <= 31).then_some((z, x, y))
            })();
            parsed.ok_or_else(|| format!("{}: '{line}' is not z/x/y", path.display()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_reaches_its_tile_and_with_a_buffer_its_neighbours() {
        // Kraków's main square.
        let point = Degrees {
            west: 19.9373,
            south: 50.0614,
            east: 19.9373,
            north: 50.0614,
        };
        let zooms = ZoomRange::new(14, 14).unwrap();
        let alone = tiles_reached(&[point], zooms, 0.0, None);
        assert_eq!(alone.into_iter().collect::<Vec<_>>(), [(14, 9099, 5552)]);
        let buffered = tiles_reached(&[point], zooms, 256.0, None);
        assert_eq!(buffered.len(), 9);
        assert!(buffered.contains(&(14, 9098, 5551)));
        let all = tiles_reached(&[point], ZoomRange::ALL, 0.0, None);
        assert_eq!(all.len(), 15);
        assert!(all.contains(&(0, 0, 0)));
    }

    #[test]
    fn boxes_are_cut_to_the_clip() {
        let wide = Degrees {
            west: -10.0,
            south: 40.0,
            east: 30.0,
            north: 60.0,
        };
        let clip = CompileBounds::new(50.1, 50.0, 20.0, 19.9);
        let zooms = ZoomRange::new(10, 10).unwrap();
        let cut = tiles_reached(&[wide], zooms, 0.0, Some(clip));
        let exact = tiles_reached(
            &[Degrees {
                west: 19.9,
                south: 50.0,
                east: 20.0,
                north: 50.1,
            }],
            zooms,
            0.0,
            None,
        );
        assert_eq!(cut, exact);
        let outside = CompileBounds::new(10.0, 9.0, 10.0, 9.0);
        assert!(tiles_reached(&[wide], zooms, 0.0, Some(outside)).is_empty());
    }

    #[test]
    fn lists_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiles.txt");
        let tiles = [(0, 0, 0), (14, 9099, 5552)];
        assert_eq!(write(&path, tiles).unwrap(), 2);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "0/0/0\n14/9099/5552\n"
        );
        assert_eq!(read_tile_list(&path).unwrap(), tiles.into_iter().collect());
        assert!(write(&path, tiles).is_err());
        std::fs::write(&path, "1/2\n").unwrap();
        assert!(read_tile_list(&path).is_err());
    }
}
