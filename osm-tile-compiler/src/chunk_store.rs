use crate::{ArchiveCenter, CompileBounds};
use pmtiles::{Compression, PmTilesStreamWriter, PmTilesWriter, TileCoord, TileId, TileType};
use std::fs::File;

/// Writes through a borrowed file, so the caller can hold a lock on it while
/// the archive is written.
pub struct PmtilesArchiveWriter<'a> {
    writer: PmTilesStreamWriter<&'a File>,
}

impl<'a> PmtilesArchiveWriter<'a> {
    pub fn create(
        file: &'a File,
        bounds: &CompileBounds,
        center: ArchiveCenter,
        metadata_json: &str,
        min_zoom: u8,
        max_zoom: u8,
    ) -> Result<Self, String> {
        // The bytes arriving here are already gzip-compressed by the tile
        // encoder (`omt::store`). `precompressed` stores them verbatim
        // and declares `Compression::Gzip` in the header, which is a promise about the
        // stored bytes rather than the writer's Mvt default.
        // Bounds crossing the antimeridian are declared as every longitude.
        let bounds = bounds.archive_bounds();
        let writer = PmTilesWriter::new(TileType::Mvt)
            .precompressed(Compression::Gzip)
            .min_zoom(min_zoom)
            .max_zoom(max_zoom)
            .bounds(bounds.west, bounds.south, bounds.east, bounds.north)
            .center(center.longitude, center.latitude)
            .center_zoom(center.zoom)
            .metadata(metadata_json)
            .create(file)
            .map_err(|error| format!("Failed to initialize PMTiles writer: {}", error))?;
        Ok(Self { writer })
    }

    pub fn write_tile(&mut self, tile_id: u64, bytes: &[u8]) -> Result<(), String> {
        let coord = TileCoord::from(
            TileId::new(tile_id)
                .map_err(|error| format!("Invalid tile id {}: {}", tile_id, error))?,
        );
        self.writer
            .add_tile(coord, bytes)
            .map_err(|error| format!("Failed to append tile {:?}: {}", coord, error))
    }

    pub fn finalize(self) -> Result<(), String> {
        self.writer
            .finalize()
            .map_err(|error| format!("Failed to finalize PMTiles archive: {}", error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    #[tokio::test]
    async fn pmtiles_header_declares_gzip_and_tiles_round_trip_verbatim() {
        use pmtiles::AsyncPmTilesReader;

        let dir = tempdir().unwrap();
        let archive_path = dir.path().join("compression.pmtiles");
        // CompileBounds::new(north, south, east, west) — small Kraków-ish box.
        let bounds = CompileBounds::new(50.1, 50.0, 20.1, 20.0);

        // The production pipeline hands this writer gzip-compressed MVT bytes.
        let raw = fast_mvt::MvtLayerBuilder::new("water_polygons")
            .unwrap()
            .encode();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&raw).unwrap();
        let gzipped_tile = gzip.finish().unwrap();

        {
            let file = File::create(&archive_path).unwrap();
            let mut archive = PmtilesArchiveWriter::create(
                &file,
                &bounds,
                crate::ArchiveCenter {
                    longitude: 0.0,
                    latitude: 0.0,
                    zoom: 0,
                },
                "{}",
                0,
                0,
            )
            .unwrap();
            // tile_id 0 == z0/0/0.
            archive.write_tile(0, &gzipped_tile).unwrap();
            archive.finalize().unwrap();
        }

        let reader = AsyncPmTilesReader::new_with_path(&archive_path)
            .await
            .unwrap();

        // The header must describe the actual stored bytes: gzip-compressed.
        assert_eq!(
            reader.get_header().tile_compression,
            Compression::Gzip,
            "PMTiles header tile_compression must be Gzip to match the pre-gzipped bytes written by a precompressed writer"
        );

        // A precompressed writer stores verbatim: bytes round-trip identical, still gzipped.
        let stored = reader
            .get_tile(TileId::new(0).unwrap())
            .await
            .unwrap()
            .expect("tile 0 should exist");
        assert_eq!(
            stored.as_ref(),
            gzipped_tile.as_slice(),
            "stored tile bytes must be byte-identical to the gzipped bytes written"
        );
    }
}
