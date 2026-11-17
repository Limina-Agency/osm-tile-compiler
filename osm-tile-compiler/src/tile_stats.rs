//! Tile size statistics gathered while an archive is written: per zoom and
//! layer, the largest tiles, and the tiles above a warning size. Only
//! observes the output; it never changes a tile.
use pmtiles::{TileCoord, TileId};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Tiles larger than this, compressed, are reported as warnings.
pub const TILE_WARNING_BYTES: u64 = 1024 * 1024;
/// How many of the largest tiles the statistics list.
const LARGEST_TILES: usize = 10;

/// Sizes of the tiles written to an archive. Compressed sizes are the bytes
/// stored for a tile; layer sizes are uncompressed protobuf bytes, the
/// measure that compares across layers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TileStats {
    /// The compressed size above which a tile counts in `over_warning`.
    pub warning_bytes: u64,
    /// Tiles whose compressed size exceeds `warning_bytes`.
    pub over_warning: u64,
    /// Zooms that have tiles, ascending.
    pub zooms: Vec<ZoomStats>,
    /// The largest tiles by compressed size, largest first.
    pub largest: Vec<TileSize>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ZoomStats {
    pub zoom: u8,
    pub tiles: u64,
    /// Compressed bytes of all tiles at this zoom.
    pub bytes: u64,
    /// Compressed tile size percentiles (nearest rank) and maximum.
    pub p50_bytes: u64,
    pub p90_bytes: u64,
    pub p99_bytes: u64,
    pub max_bytes: u64,
    /// Uncompressed size per layer at this zoom.
    pub layers: BTreeMap<String, LayerStats>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LayerStats {
    /// Tiles that contain the layer.
    pub tiles: u64,
    pub bytes: u64,
    pub max_bytes: u64,
    /// The tile holding the largest instance of the layer, as `z/x/y`.
    pub max_tile: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TileSize {
    /// `z/x/y`.
    pub tile: String,
    /// Compressed bytes.
    pub bytes: u64,
    /// Uncompressed bytes per layer, largest first.
    pub layers: Vec<(String, u64)>,
}

#[derive(Default)]
struct ZoomAccumulator {
    sizes: Vec<u64>,
    layers: BTreeMap<String, LayerStats>,
}

/// Collects `TileStats` from tiles as they are written, in any order.
pub(crate) struct TileStatsBuilder {
    warning_bytes: u64,
    over_warning: u64,
    zooms: BTreeMap<u8, ZoomAccumulator>,
    /// The largest tiles so far, with their tile IDs, largest first and by
    /// ascending ID among equal sizes.
    largest: Vec<(u64, TileSize)>,
}

fn tile_name(tile_id: u64) -> String {
    match TileId::new(tile_id) {
        Ok(id) => {
            let coord = TileCoord::from(id);
            format!("{}/{}/{}", coord.z(), coord.x(), coord.y())
        }
        Err(_) => format!("#{tile_id}"),
    }
}

impl TileStatsBuilder {
    pub(crate) fn new(warning_bytes: u64) -> Self {
        Self {
            warning_bytes,
            over_warning: 0,
            zooms: BTreeMap::new(),
            largest: Vec::new(),
        }
    }

    /// Records one tile: its ID, its compressed size, and the uncompressed
    /// size of each of its layers.
    pub(crate) fn add(&mut self, tile_id: u64, bytes: u64, layers: &[(&str, u64)]) {
        let zoom = TileId::new(tile_id).map_or(u8::MAX, |id| TileCoord::from(id).z());
        let accumulator = self.zooms.entry(zoom).or_default();
        accumulator.sizes.push(bytes);
        for &(name, layer_bytes) in layers {
            let stats = accumulator.layers.entry(name.to_string()).or_default();
            stats.tiles += 1;
            stats.bytes += layer_bytes;
            if layer_bytes > stats.max_bytes || stats.max_tile.is_empty() {
                stats.max_bytes = layer_bytes;
                stats.max_tile = tile_name(tile_id);
            }
        }
        if bytes > self.warning_bytes {
            self.over_warning += 1;
        }
        let ranks_before = |(other_bytes, other_id): (u64, u64)| {
            bytes > other_bytes || (bytes == other_bytes && tile_id < other_id)
        };
        let full = self.largest.len() >= LARGEST_TILES;
        if full
            && !self
                .largest
                .last()
                .is_some_and(|(id, last)| ranks_before((last.bytes, *id)))
        {
            return;
        }
        let mut sizes = layers
            .iter()
            .map(|&(name, size)| (name.to_string(), size))
            .collect::<Vec<_>>();
        sizes.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let at = self
            .largest
            .iter()
            .position(|(id, other)| ranks_before((other.bytes, *id)))
            .unwrap_or(self.largest.len());
        self.largest.insert(
            at,
            (
                tile_id,
                TileSize {
                    tile: tile_name(tile_id),
                    bytes,
                    layers: sizes,
                },
            ),
        );
        self.largest.truncate(LARGEST_TILES);
    }

    pub(crate) fn finish(self) -> TileStats {
        let zooms = self
            .zooms
            .into_iter()
            .map(|(zoom, mut accumulator)| {
                accumulator.sizes.sort_unstable();
                let sizes = &accumulator.sizes;
                // Nearest rank: the smallest size with at least p% of tiles
                // at or below it.
                let percentile = |p: u64| {
                    let rank = (p * sizes.len() as u64).div_ceil(100).max(1);
                    sizes[rank as usize - 1]
                };
                ZoomStats {
                    zoom,
                    tiles: sizes.len() as u64,
                    bytes: sizes.iter().sum(),
                    p50_bytes: percentile(50),
                    p90_bytes: percentile(90),
                    p99_bytes: percentile(99),
                    max_bytes: *sizes.last().unwrap(),
                    layers: accumulator.layers,
                }
            })
            .collect();
        TileStats {
            warning_bytes: self.warning_bytes,
            over_warning: self.over_warning,
            zooms,
            largest: self.largest.into_iter().map(|(_, size)| size).collect(),
        }
    }
}

impl TileStats {
    /// The number of tiles written.
    pub fn tiles(&self) -> u64 {
        self.zooms.iter().map(|zoom| zoom.tiles).sum()
    }

    /// A warning naming the tiles over the warning size, if there are any.
    pub fn warning(&self) -> Option<String> {
        let largest = self.largest.first()?;
        (self.over_warning > 0).then(|| {
            let layers = largest
                .layers
                .iter()
                .take(3)
                .map(|(name, bytes)| format!("{name} {}", kib(*bytes)))
                .collect::<Vec<_>>()
                .join(", ");
            let tiles = match self.over_warning {
                1 => "1 tile exceeds".to_string(),
                n => format!("{n} tiles exceed"),
            };
            format!(
                "{tiles} {} compressed; the largest, {}, is {} (uncompressed: {layers})",
                kib(self.warning_bytes),
                largest.tile,
                kib(largest.bytes),
            )
        })
    }
}

/// Bytes as KiB with one decimal, e.g. `1024.0 KiB`.
pub fn kib(bytes: u64) -> String {
    format!("{:.1} KiB", bytes as f64 / 1024.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(z: u8, x: u32, y: u32) -> u64 {
        TileId::from(TileCoord::new(z, x, y).unwrap()).value()
    }

    #[test]
    fn summarizes_zooms_layers_and_percentiles() {
        let mut builder = TileStatsBuilder::new(1000);
        for size in 1..=100 {
            builder.add(id(14, size as u32, 0), size, &[("building", size * 2)]);
        }
        builder.add(id(13, 0, 0), 7, &[("water", 30), ("building", 5)]);
        let stats = builder.finish();
        assert_eq!(stats.zooms.len(), 2);
        let z13 = &stats.zooms[0];
        assert_eq!(
            (z13.zoom, z13.tiles, z13.max_bytes, z13.p50_bytes),
            (13, 1, 7, 7)
        );
        let z14 = &stats.zooms[1];
        assert_eq!((z14.tiles, z14.bytes), (100, 5050));
        assert_eq!(
            (z14.p50_bytes, z14.p90_bytes, z14.p99_bytes, z14.max_bytes),
            (50, 90, 99, 100)
        );
        let building = &z14.layers["building"];
        assert_eq!(
            (building.tiles, building.bytes, building.max_bytes),
            (100, 10100, 200)
        );
        assert_eq!(building.max_tile, "14/100/0");
        assert_eq!(stats.over_warning, 0);
        assert_eq!(stats.warning(), None);
    }

    #[test]
    fn keeps_the_largest_tiles_in_order_whatever_the_arrival_order() {
        let sizes = [5, 300, 42, 300, 7, 999, 1, 64, 128, 256, 512, 2, 3, 300];
        let mut forward = TileStatsBuilder::new(u64::MAX);
        let mut backward = TileStatsBuilder::new(u64::MAX);
        for (i, &size) in sizes.iter().enumerate() {
            forward.add(id(14, i as u32, 0), size, &[]);
        }
        for (i, &size) in sizes.iter().enumerate().rev() {
            backward.add(id(14, i as u32, 0), size, &[]);
        }
        let (forward, backward) = (forward.finish(), backward.finish());
        assert_eq!(forward, backward);
        let largest = forward
            .largest
            .iter()
            .map(|t| (t.tile.as_str(), t.bytes))
            .collect::<Vec<_>>();
        assert_eq!(
            largest,
            [
                ("14/5/0", 999),
                ("14/10/0", 512),
                ("14/1/0", 300),
                ("14/3/0", 300),
                ("14/13/0", 300),
                ("14/9/0", 256),
                ("14/8/0", 128),
                ("14/7/0", 64),
                ("14/2/0", 42),
                ("14/4/0", 7),
            ]
        );
    }

    #[test]
    fn warns_about_tiles_over_the_warning_size() {
        let mut builder = TileStatsBuilder::new(1024);
        builder.add(id(14, 0, 0), 1024, &[]);
        builder.add(
            id(14, 1, 0),
            4096,
            &[
                ("poi", 1024),
                ("building", 8192),
                ("water", 2048),
                ("park", 10),
            ],
        );
        let stats = builder.finish();
        assert_eq!(stats.over_warning, 1);
        assert_eq!(
            stats.warning().unwrap(),
            "1 tile exceeds 1.0 KiB compressed; the largest, 14/1/0, is 4.0 KiB \
             (uncompressed: building 8.0 KiB, water 2.0 KiB, poi 1.0 KiB)"
        );
    }
}
