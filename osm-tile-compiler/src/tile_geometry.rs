use fast_mvt::{MvtCoord, MvtFeatureBuilder, MvtGeomType, MvtResult};
use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default,
)]
pub struct TilePoint {
    pub x: i32,
    pub y: i32,
}

impl TilePoint {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
pub struct TileRingGroup {
    pub exterior: Vec<TilePoint>,
    pub holes: Vec<Vec<TilePoint>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum TileGeometry {
    Point(Vec<TilePoint>),
    Line {
        scale_exp: u8,
        paths: Vec<Vec<TilePoint>>,
    },
    Polygon(Vec<TileRingGroup>),
    FilledPolygon {
        buffer_pixels: i16,
    },
}

impl TileGeometry {
    pub fn geometry_type(&self) -> MvtGeomType {
        match self {
            Self::Point(_) => MvtGeomType::Point,
            Self::Line { .. } => MvtGeomType::LineString,
            Self::Polygon(_) | Self::FilledPolygon { .. } => MvtGeomType::Polygon,
        }
    }

    pub fn geometry_type_id(&self) -> u8 {
        match self {
            Self::Point(_) => 1,
            Self::Line { .. } => 2,
            Self::Polygon(_) => 3,
            Self::FilledPolygon { .. } => 4,
        }
    }
}

/// The tile extent features are written at.
pub(crate) const TILE_EXTENT: i32 = 4096;

/// What of a [`TileGeometry`] is written to a tile, at the tile's
/// resolution. `None` from [`TileGeometry::mvt_shape`] means nothing is left,
/// and the feature is not written.
pub(crate) enum MvtShape<'a> {
    Points(&'a [TilePoint]),
    /// Paths rounded to the tile's resolution, without repeated points.
    Lines(Vec<Vec<MvtCoord>>),
    Polygon(&'a [TileRingGroup]),
    /// The square of the whole tile, `buffer_pixels` beyond each edge.
    Fill(i16),
}

impl TileGeometry {
    pub(crate) fn mvt_shape(&self) -> Option<MvtShape<'_>> {
        match self {
            Self::Point(points) => (!points.is_empty()).then_some(MvtShape::Points(points)),
            Self::Line { scale_exp, paths } => {
                let divisor = 2_f64.powi(i32::from(*scale_exp));
                let round = |v: i32| (f64::from(v) / divisor + 0.5).floor() as i32;
                let lines = paths
                    .iter()
                    .filter_map(|path| {
                        let mut line = path
                            .iter()
                            .map(|p| MvtCoord {
                                x: round(p.x),
                                y: round(p.y),
                            })
                            .collect::<Vec<_>>();
                        line.dedup();
                        (line.len() >= 2).then_some(line)
                    })
                    .collect::<Vec<_>>();
                (!lines.is_empty()).then_some(MvtShape::Lines(lines))
            }
            Self::Polygon(groups) => groups
                .iter()
                .flat_map(|group| std::iter::once(&group.exterior).chain(&group.holes))
                .any(|ring| written_ring(ring).is_some())
                .then_some(MvtShape::Polygon(groups)),
            Self::FilledPolygon { buffer_pixels } => Some(MvtShape::Fill(*buffer_pixels)),
        }
    }
}

impl MvtShape<'_> {
    /// Adds the shape to `feature`, a feature of this geometry's type.
    pub(crate) fn write(&self, feature: &mut MvtFeatureBuilder) -> MvtResult<()> {
        let coord = |p: &TilePoint| MvtCoord { x: p.x, y: p.y };
        match self {
            Self::Points(points) => {
                feature.points(points.iter().map(coord))?;
            }
            Self::Lines(lines) => {
                for line in lines {
                    feature.line(line.iter().copied())?;
                }
            }
            Self::Polygon(groups) => {
                for group in *groups {
                    let rings = std::iter::once((&group.exterior, true))
                        .chain(group.holes.iter().map(|hole| (hole, false)));
                    for (ring, exterior) in rings {
                        if let Some(ring) = written_ring(ring) {
                            feature.ring(ring.iter().map(coord), exterior)?;
                        }
                    }
                }
            }
            Self::Fill(buffer_pixels) => {
                let (low, high) = (
                    -i32::from(*buffer_pixels),
                    TILE_EXTENT + i32::from(*buffer_pixels),
                );
                let square = [(low, low), (high, low), (high, high), (low, high)];
                feature.ring(square.map(|(x, y)| MvtCoord { x, y }), true)?;
            }
        }
        Ok(())
    }
}

/// A closed ring with at least three distinct corners, as written; shorter
/// rings are left out.
fn written_ring(ring: &[TilePoint]) -> Option<&[TilePoint]> {
    if ring.len() < 4 {
        return None;
    }
    let corners = if ring.first() == ring.last() {
        ring.len() - 1
    } else {
        ring.len()
    };
    (corners >= 3).then_some(ring)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fast_mvt::{MvtLayerBuilder, MvtReaderRef};

    /// The commands written for `geometry`, read back from an encoded tile.
    fn commands(geometry: &TileGeometry) -> Option<Vec<u32>> {
        let shape = geometry.mvt_shape()?;
        let mut feature = MvtLayerBuilder::new("test")
            .unwrap()
            .feature_of(geometry.geometry_type());
        shape.write(&mut feature).unwrap();
        let tile = feature.end().encode();
        let reader = MvtReaderRef::new(&tile).unwrap();
        let layer = reader.layers().next().unwrap();
        let feature = layer.features().next().unwrap();
        Some(feature.geometry_commands().to_vec())
    }

    fn zigzag(value: i32) -> u32 {
        ((value << 1) ^ (value >> 31)) as u32
    }

    fn points(coords: &[(i32, i32)]) -> Vec<TilePoint> {
        coords.iter().map(|&(x, y)| TilePoint::new(x, y)).collect()
    }

    /// The index of the `n`th MoveTo command.
    fn move_to(commands: &[u32], n: usize) -> usize {
        let mut at = 0;
        let mut seen = 0;
        loop {
            let (id, count) = (commands[at] & 7, (commands[at] >> 3) as usize);
            if id == 1 {
                if seen == n {
                    return at;
                }
                seen += 1;
            }
            at += 1 + if id == 7 { 0 } else { 2 * count };
        }
    }

    #[test]
    fn polygon_rings_delta_encode_across_the_cursor() {
        let geometry = TileGeometry::Polygon(vec![TileRingGroup {
            exterior: points(&[(10, 10), (30, 10), (30, 30), (10, 30), (10, 10)]),
            holes: vec![points(&[(14, 14), (14, 18), (18, 18), (18, 14), (14, 14)])],
        }]);
        let commands = commands(&geometry).unwrap();
        let hole = move_to(&commands, 1);
        assert_eq!(commands[hole + 1..hole + 3], [zigzag(4), zigzag(-16)]);
    }

    #[test]
    fn line_paths_delta_encode_across_the_cursor() {
        let geometry = TileGeometry::Line {
            scale_exp: 0,
            paths: vec![points(&[(10, 10), (20, 10)]), points(&[(12, 14), (16, 14)])],
        };
        let commands = commands(&geometry).unwrap();
        let second = move_to(&commands, 1);
        assert_eq!(commands[second + 1..second + 3], [zigzag(-8), zigzag(4)]);
    }

    #[test]
    fn multipoints_use_one_moveto_with_coordinate_count() {
        let geometry = TileGeometry::Point(points(&[(10, 20), (12, 17), (-1, 17)]));
        assert_eq!(commands(&geometry).unwrap(), [25, 20, 40, 4, 5, 25, 0]);
    }

    #[test]
    fn lines_keep_the_large_omt_buffer() {
        let geometry = TileGeometry::Line {
            scale_exp: 0,
            paths: vec![points(&[(-4096, 0), (8192, 0)])],
        };
        let commands = commands(&geometry).unwrap();
        assert_eq!((commands[1], commands[4]), (8191, 24576));
    }

    #[test]
    fn a_fill_is_the_buffered_tile_square() {
        let side = TILE_EXTENT + 8;
        assert_eq!(
            commands(&TileGeometry::FilledPolygon { buffer_pixels: 4 }).unwrap(),
            [
                9,
                zigzag(-4),
                zigzag(-4),
                (3 << 3) | 2,
                zigzag(side),
                0,
                0,
                zigzag(side),
                zigzag(-side),
                0,
                15,
            ]
        );
    }

    #[test]
    fn geometry_with_nothing_left_at_tile_resolution_is_not_written() {
        // Both points round to x = 2 at half resolution.
        let line = TileGeometry::Line {
            scale_exp: 1,
            paths: vec![points(&[(3, 0), (4, 0)])],
        };
        assert!(line.mvt_shape().is_none());
        let sliver = TileGeometry::Polygon(vec![TileRingGroup {
            exterior: points(&[(0, 0), (5, 0), (0, 0)]),
            holes: vec![],
        }]);
        assert!(sliver.mvt_shape().is_none());
        assert!(TileGeometry::Point(vec![]).mvt_shape().is_none());
    }
}
