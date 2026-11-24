//! Well-Known Binary, as GeoPackage, GeoParquet and CSV hold it: ISO WKB
//! with Z, M or ZM (types 1000 to 3007), and PostGIS's extended WKB with
//! its flag bits and SRID. Z and M are dropped.

use geo_types::{
    Coord, Geometry, GeometryCollection, LineString, MultiLineString, MultiPoint, MultiPolygon,
    Point, Polygon,
};

/// The deepest nesting of collections read.
const MAX_DEPTH: usize = 32;

/// A geometry, and the SRID an extended WKB carries. An empty point
/// (NaN coordinates) is `None`.
pub(crate) fn parse(bytes: &[u8]) -> Result<(Option<Geometry<f64>>, Option<u32>), String> {
    let mut reader = Reader { bytes, at: 0 };
    let mut srid = None;
    let geometry = reader.geometry(0, &mut srid)?;
    Ok((geometry, srid))
}

/// Hex WKB, as CSV files and PostGIS text output hold it.
pub(crate) fn parse_hex(text: &str) -> Result<(Option<Geometry<f64>>, Option<u32>), String> {
    let text = text.trim();
    let text = text
        .strip_prefix("\\x")
        .or_else(|| text.strip_prefix("0x"))
        .unwrap_or(text);
    let bytes = hex::decode(text).map_err(|e| format!("hex WKB: {e}"))?;
    parse(&bytes)
}

/// Whether the text looks like hex WKB rather than WKT.
pub(crate) fn looks_like_hex(text: &str) -> bool {
    let text = text.trim();
    text.len() >= 18
        && text.len().is_multiple_of(2)
        && (text.starts_with("00") || text.starts_with("01"))
        && text.bytes().all(|b| b.is_ascii_hexdigit())
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let end = self.at + N;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or("WKB ends inside a geometry")?;
        self.at = end;
        Ok(slice.try_into().expect("N bytes"))
    }

    fn u32(&mut self, little: bool) -> Result<u32, String> {
        let bytes = self.take::<4>()?;
        Ok(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    }

    fn f64(&mut self, little: bool) -> Result<f64, String> {
        let bytes = self.take::<8>()?;
        Ok(if little {
            f64::from_le_bytes(bytes)
        } else {
            f64::from_be_bytes(bytes)
        })
    }

    /// A count, bounded by what the rest of the input could hold, so a
    /// corrupt count cannot ask for gigabytes.
    fn count(&mut self, little: bool, item_bytes: usize) -> Result<usize, String> {
        let count = self.u32(little)? as usize;
        let left = self.bytes.len().saturating_sub(self.at);
        if count.saturating_mul(item_bytes) > left {
            return Err(format!("WKB count {count} exceeds the bytes left"));
        }
        Ok(count)
    }

    fn coord(&mut self, little: bool, dims: usize) -> Result<Coord<f64>, String> {
        let x = self.f64(little)?;
        let y = self.f64(little)?;
        for _ in 2..dims {
            self.f64(little)?;
        }
        Ok(Coord { x, y })
    }

    fn line(&mut self, little: bool, dims: usize) -> Result<LineString<f64>, String> {
        let count = self.count(little, 8 * dims)?;
        (0..count)
            .map(|_| self.coord(little, dims))
            .collect::<Result<Vec<_>, _>>()
            .map(LineString)
    }

    fn polygon(&mut self, little: bool, dims: usize) -> Result<Polygon<f64>, String> {
        let rings = self.count(little, 4)?;
        let mut all = (0..rings)
            .map(|_| self.line(little, dims))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter();
        let exterior = all.next().unwrap_or_else(|| LineString(Vec::new()));
        Ok(Polygon::new(exterior, all.collect()))
    }

    fn geometry(
        &mut self,
        depth: usize,
        srid: &mut Option<u32>,
    ) -> Result<Option<Geometry<f64>>, String> {
        if depth > MAX_DEPTH {
            return Err("WKB collections nest too deeply".into());
        }
        let little = match self.take::<1>()?[0] {
            0 => false,
            1 => true,
            other => return Err(format!("WKB byte order {other} is neither 0 nor 1")),
        };
        let raw = self.u32(little)?;
        // Extended WKB's flags, then ISO's thousands.
        let mut dims = 2;
        if raw & 0x8000_0000 != 0 {
            dims += 1;
        }
        if raw & 0x4000_0000 != 0 {
            dims += 1;
        }
        if raw & 0x2000_0000 != 0 {
            let code = self.u32(little)?;
            srid.get_or_insert(code);
        }
        let code = raw & 0x0FFF_FFFF;
        let (kind, iso) = (code % 1000, code / 1000);
        dims += match iso {
            0 => 0,
            1 | 2 => 1,
            3 => 2,
            _ => return Err(format!("unknown WKB geometry type {code}")),
        };
        let parts = |reader: &mut Self, srid: &mut Option<u32>| {
            let count = reader.count(little, 9)?;
            let mut parts = Vec::with_capacity(count);
            for _ in 0..count {
                if let Some(part) = reader.geometry(depth + 1, srid)? {
                    parts.push(part);
                }
            }
            Ok::<_, String>(parts)
        };
        Ok(Some(match kind {
            1 => {
                let coord = self.coord(little, dims)?;
                if coord.x.is_nan() && coord.y.is_nan() {
                    return Ok(None);
                }
                Geometry::Point(Point(coord))
            }
            2 => Geometry::LineString(self.line(little, dims)?),
            3 => Geometry::Polygon(self.polygon(little, dims)?),
            4 => Geometry::MultiPoint(MultiPoint(
                parts(self, srid)?
                    .into_iter()
                    .filter_map(|g| match g {
                        Geometry::Point(p) => Some(p),
                        _ => None,
                    })
                    .collect(),
            )),
            5 => Geometry::MultiLineString(MultiLineString(
                parts(self, srid)?
                    .into_iter()
                    .filter_map(|g| match g {
                        Geometry::LineString(l) => Some(l),
                        _ => None,
                    })
                    .collect(),
            )),
            6 => Geometry::MultiPolygon(MultiPolygon(
                parts(self, srid)?
                    .into_iter()
                    .filter_map(|g| match g {
                        Geometry::Polygon(p) => Some(p),
                        _ => None,
                    })
                    .collect(),
            )),
            7 => Geometry::GeometryCollection(GeometryCollection(parts(self, srid)?)),
            _ => {
                return Err(format!(
                    "WKB geometry type {code} is not a point, line, polygon or collection"
                ));
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_iso_and_extended_wkb() {
        // POINT(1 2), little-endian.
        let point = parse_hex("0101000000000000000000F03F0000000000000040").unwrap();
        assert_eq!(point.0, Some(Geometry::Point(Point::new(1.0, 2.0))));
        // POINT Z (1 2 3), ISO, big-endian.
        let z = parse_hex("00000003E93FF000000000000040000000000000004008000000000000").unwrap();
        assert_eq!(z.0, Some(Geometry::Point(Point::new(1.0, 2.0))));
        // SRID=4326;POINT(1 2), extended.
        let ewkb = parse_hex("0101000020E6100000000000000000F03F0000000000000040").unwrap();
        assert_eq!(ewkb.1, Some(4326));
        // LINESTRING(0 0,1 1).
        let line = parse_hex(
            "01020000000200000000000000000000000000000000000000000000000000F03F000000000000F03F",
        )
        .unwrap();
        assert_eq!(
            line.0,
            Some(Geometry::LineString(LineString::from(vec![
                (0.0, 0.0),
                (1.0, 1.0)
            ])))
        );
        // POINT EMPTY.
        assert_eq!(
            parse_hex("0101000000000000000000F87F000000000000F87F")
                .unwrap()
                .0,
            None
        );
        assert!(parse_hex("01020000000000FFFF").is_err());
        assert!(looks_like_hex("0101000000000000000000F03F0000000000000040"));
        assert!(!looks_like_hex("POINT(1 2)"));
    }
}
