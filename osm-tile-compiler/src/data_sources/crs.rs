//! Coordinate systems of data inputs, and reprojection to WGS84.
//!
//! WGS84 and Web Mercator need nothing; with the `reproject` feature any
//! EPSG code, PROJ string or WKT that proj4rs can express is read too.
//! Datums within about a metre of WGS84 (ETRS89, NAD83, GDA94 and the
//! like) are read as WGS84 without a shift: at zoom 14 a pixel is about ten
//! metres.

use geo_types::{Coord, Geometry};
use std::sync::LazyLock;

const MERCATOR_RADIUS: f64 = 6_378_137.0;

pub(crate) enum Crs {
    Wgs84,
    WebMercator,
    #[cfg(feature = "reproject")]
    Proj {
        name: String,
        from: Box<proj4rs::Proj>,
    },
}

impl std::fmt::Debug for Crs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name())
    }
}

/// Geographic EPSG codes read as WGS84: WGS84 itself and the datums
/// that differ from it by about a metre or less.
const WGS84_LIKE: [u32; 12] = [
    4326, 4979, 4258, 4937, 4269, 4617, 6318, 4283, 7844, 4167, 4674, 4190,
];
/// Codes of Web Mercator, official and otherwise.
const WEB_MERCATOR: [u32; 5] = [3857, 900913, 3785, 102100, 102113];

impl Crs {
    /// A short name: `EPSG:4326`, `EPSG:3857`, `EPSG:<code>` or the PROJ
    /// string or WKT name it was made from.
    pub fn name(&self) -> String {
        match self {
            Self::Wgs84 => "EPSG:4326".into(),
            Self::WebMercator => "EPSG:3857".into(),
            #[cfg(feature = "reproject")]
            Self::Proj { name, .. } => name.clone(),
        }
    }

    /// A coordinate system as written in a request or a file: an
    /// authority code (`EPSG:2180`, `urn:ogc:def:crs:EPSG::2180`,
    /// `OGC:CRS84`, an opengis.net URL, or a bare EPSG number), a PROJ
    /// string, WKT 1 or 2, or PROJJSON.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("an empty coordinate system".into());
        }
        if text.starts_with('{') {
            return Self::projjson(text);
        }
        if text.starts_with('+') || text.contains("+proj=") {
            return Self::proj(text.to_string(), text);
        }
        let upper = text.to_ascii_uppercase();
        let wkt = [
            "PROJCS[",
            "GEOGCS[",
            "GEOCCS[",
            "COMPD_CS[",
            "PROJCRS[",
            "GEOGCRS[",
            "GEODCRS[",
            "PROJECTEDCRS[",
            "GEOGRAPHICCRS[",
            "GEODETICCRS[",
            "COMPOUNDCRS[",
            "BOUNDCRS[",
        ];
        if wkt
            .iter()
            .any(|keyword| upper.replace(' ', "").starts_with(keyword))
        {
            return Self::wkt(text);
        }
        let (authority, code) =
            authority_code(text).ok_or_else(|| format!("unknown coordinate system '{text}'"))?;
        Self::from_authority(&authority, &code)
    }

    /// A coordinate system an authority names.
    pub fn from_authority(authority: &str, code: &str) -> Result<Self, String> {
        let authority = authority.to_ascii_uppercase();
        let code = code.trim();
        match authority.as_str() {
            "OGC" | "" if code.eq_ignore_ascii_case("CRS84") || code == "84" => Ok(Self::Wgs84),
            "OGC" if code.eq_ignore_ascii_case("CRS84H") => Ok(Self::Wgs84),
            "EPSG" | "ESRI" | "" => {
                let number: u32 = code
                    .parse()
                    .map_err(|_| format!("unknown coordinate system {authority}:{code}"))?;
                Self::from_epsg(number)
            }
            _ => Err(format!(
                "unknown coordinate system {authority}:{code}; give it as EPSG:<code>, a PROJ \
                 string or WKT"
            )),
        }
    }

    pub fn from_epsg(code: u32) -> Result<Self, String> {
        if WGS84_LIKE.contains(&code) {
            return Ok(Self::Wgs84);
        }
        if WEB_MERCATOR.contains(&code) {
            return Ok(Self::WebMercator);
        }
        #[cfg(feature = "reproject")]
        {
            let definition = u16::try_from(code)
                .ok()
                .and_then(crs_definitions::from_code)
                .ok_or_else(|| {
                    format!(
                        "EPSG:{code} is not among the codes this build knows; give its PROJ \
                         string instead"
                    )
                })?;
            Self::proj(format!("EPSG:{code}"), definition.proj4)
        }
        #[cfg(not(feature = "reproject"))]
        Err(format!(
            "EPSG:{code} needs reprojection, which this build leaves out (the `reproject` \
             feature); only WGS84 and Web Mercator are read"
        ))
    }

    #[cfg(feature = "reproject")]
    fn proj(name: String, definition: &str) -> Result<Self, String> {
        let from = proj4rs::Proj::from_proj_string(definition)
            .map_err(|e| format!("coordinate system {name}: {e}"))?;
        if from.is_latlong() && is_wgs84_like_proj(definition) {
            return Ok(Self::Wgs84);
        }
        Ok(Self::Proj {
            name,
            from: Box::new(from),
        })
    }

    #[cfg(not(feature = "reproject"))]
    fn proj(name: String, definition: &str) -> Result<Self, String> {
        let words = definition.split_whitespace().collect::<Vec<_>>();
        if words.contains(&"+proj=longlat") && is_wgs84_like_proj(definition) {
            return Ok(Self::Wgs84);
        }
        if words.contains(&"+proj=merc")
            && words.contains(&"+a=6378137")
            && words.contains(&"+b=6378137")
        {
            return Ok(Self::WebMercator);
        }
        Err(format!(
            "coordinate system {name} needs reprojection, which this build leaves out (the \
             `reproject` feature)"
        ))
    }

    fn wkt(text: &str) -> Result<Self, String> {
        if let Some((authority, code)) = top_level_authority(text)
            && let Ok(crs) = Self::from_authority(&authority, &code)
        {
            return Ok(crs);
        }
        let upper = text.to_ascii_uppercase();
        let name = first_quoted(text).unwrap_or("WKT").to_string();
        let flat = upper.replace([' ', '_', '-'], "");
        if flat.contains("WEBMERCATOR")
            || flat.contains("PSEUDOMERCATOR")
            || flat.contains("POPULARVISUALISATION")
        {
            return Ok(Self::WebMercator);
        }
        let geographic = ["GEOGCS[", "GEOGCRS[", "GEOGRAPHICCRS["]
            .iter()
            .any(|keyword| flat.starts_with(keyword));
        if geographic
            && [
                "WGS1984", "WGS84", "GRS1980", "ETRS1989", "NAD1983", "GRS80",
            ]
            .iter()
            .any(|datum| flat.contains(datum))
        {
            return Ok(Self::Wgs84);
        }
        #[cfg(feature = "reproject")]
        {
            let definition = proj4wkt::wkt_to_projstring(text)
                .map_err(|e| format!("coordinate system {name}: {e}"))?;
            Self::proj(name, &definition)
        }
        #[cfg(not(feature = "reproject"))]
        Err(format!(
            "coordinate system {name} needs reprojection, which this build leaves out (the \
             `reproject` feature)"
        ))
    }

    /// PROJJSON, as GeoParquet gives it: its `id`, or a WGS84 geographic
    /// system by name.
    fn projjson(text: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("PROJJSON coordinate system: {e}"))?;
        Self::from_projjson(&value)
    }

    pub fn from_projjson(value: &serde_json::Value) -> Result<Self, String> {
        let id = |value: &serde_json::Value| {
            let id = value.get("id").or_else(|| value.get("ids")?.get(0))?;
            let authority = id.get("authority")?.as_str()?.to_string();
            let code = match id.get("code")? {
                serde_json::Value::String(code) => code.clone(),
                code => code.to_string(),
            };
            Some((authority, code))
        };
        let found = id(value).or_else(|| id(value.get("source_crs")?));
        if let Some((authority, code)) = found {
            return Self::from_authority(&authority, &code);
        }
        let name = value.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let kind = value.get("type").and_then(|n| n.as_str()).unwrap_or("");
        if kind == "GeographicCRS" && (name.contains("WGS 84") || name.contains("CRS84")) {
            return Ok(Self::Wgs84);
        }
        Err(format!(
            "PROJJSON coordinate system '{name}' has no id; give the input's crs instead"
        ))
    }

    /// The geometry in WGS84 degrees; `None` when a point cannot be
    /// reprojected.
    pub fn to_wgs84(&self, geometry: Geometry<f64>) -> Option<Geometry<f64>> {
        use geo::MapCoords;
        match self {
            Self::Wgs84 => Some(geometry),
            Self::WebMercator => Some(geometry.map_coords(|c| {
                Coord {
                    x: (c.x / MERCATOR_RADIUS).to_degrees(),
                    y: (2.0 * (c.y / MERCATOR_RADIUS).exp().atan() - std::f64::consts::FRAC_PI_2)
                        .to_degrees(),
                }
            })),
            #[cfg(feature = "reproject")]
            Self::Proj { from, .. } => {
                let latlong = from.is_latlong();
                geometry
                    .try_map_coords(|c| {
                        let mut point = if latlong {
                            (c.x.to_radians(), c.y.to_radians(), 0.0)
                        } else {
                            (c.x, c.y, 0.0)
                        };
                        proj4rs::transform::transform(from, &WGS84, &mut point)?;
                        let (x, y) = (point.0.to_degrees(), point.1.to_degrees());
                        if x.is_finite() && y.is_finite() {
                            Ok(Coord { x, y })
                        } else {
                            Err(proj4rs::errors::Error::CoordinateOutOfRange)
                        }
                    })
                    .ok()
            }
        }
    }
}

#[cfg(feature = "reproject")]
static WGS84: LazyLock<proj4rs::Proj> = LazyLock::new(|| {
    proj4rs::Proj::from_proj_string("+proj=longlat +datum=WGS84 +no_defs")
        .expect("WGS84 is a valid PROJ string")
});

/// Whether a geographic PROJ string is WGS84 or a datum within a metre
/// of it.
fn is_wgs84_like_proj(definition: &str) -> bool {
    let words = definition.split_whitespace().collect::<Vec<_>>();
    let shifted = words.iter().any(|w| {
        w.strip_prefix("+towgs84=").is_some_and(|v| {
            v.split(',')
                .any(|n| n.parse::<f64>().is_ok_and(|n| n != 0.0))
        })
    });
    !shifted
        && (words.contains(&"+datum=WGS84")
            || words.contains(&"+ellps=WGS84")
            || words.contains(&"+ellps=GRS80")
            || words.contains(&"+datum=NAD83"))
        && !words
            .iter()
            .any(|w| w.starts_with("+pm=") && *w != "+pm=greenwich")
}

/// The authority and code of `EPSG:2180`, `EPSG::2180`, `epsg:2180`,
/// `urn:ogc:def:crs:EPSG::2180`, `urn:ogc:def:crs:OGC:1.3:CRS84`,
/// `http://www.opengis.net/def/crs/EPSG/0/2180`, `CRS84` or `2180`.
fn authority_code(text: &str) -> Option<(String, String)> {
    if text.chars().all(|c| c.is_ascii_digit()) {
        return Some((String::new(), text.into()));
    }
    if text.eq_ignore_ascii_case("CRS84") || text.eq_ignore_ascii_case("WGS84") {
        return Some(("OGC".into(), "CRS84".into()));
    }
    if let Some(rest) = text
        .strip_prefix("http://www.opengis.net/def/crs/")
        .or_else(|| text.strip_prefix("https://www.opengis.net/def/crs/"))
    {
        let parts = rest.split('/').collect::<Vec<_>>();
        return Some((parts.first()?.to_string(), parts.last()?.to_string()));
    }
    let lower = text.to_ascii_lowercase();
    let rest = if lower.starts_with("urn:ogc:def:crs:") {
        &text["urn:ogc:def:crs:".len()..]
    } else {
        text
    };
    let parts = rest.split(':').collect::<Vec<_>>();
    if parts.len() < 2 {
        return None;
    }
    let code = parts.last()?.to_string();
    if code.is_empty() {
        return None;
    }
    Some((parts[0].to_string(), code))
}

/// The `AUTHORITY["EPSG","2180"]` or `ID["EPSG",2180]` of the outermost
/// WKT node, not of a node inside it.
fn top_level_authority(text: &str) -> Option<(String, String)> {
    static AUTHORITY: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r#"(?i)\b(?:AUTHORITY|ID)\s*[\[(]\s*"([^"]+)"\s*,\s*"?([0-9A-Za-z]+)"?"#)
            .unwrap()
    });
    for found in AUTHORITY.captures_iter(text) {
        let start = found.get(0)?.start();
        if depth_at(text, start) == 1 {
            return Some((found[1].to_string(), found[2].to_string()));
        }
    }
    None
}

/// How many brackets are open at `offset`, outside quoted strings.
fn depth_at(text: &str, offset: usize) -> usize {
    let mut depth = 0usize;
    let mut quoted = false;
    for byte in text.as_bytes()[..offset].iter() {
        match byte {
            b'"' => quoted = !quoted,
            b'[' | b'(' if !quoted => depth += 1,
            b']' | b')' if !quoted => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    depth
}

fn first_quoted(text: &str) -> Option<&str> {
    let start = text.find('"')? + 1;
    let end = start + text[start..].find('"')?;
    Some(&text[start..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_codes_parse_in_every_spelling() {
        for text in [
            "EPSG:4326",
            "epsg:4326",
            "EPSG::4326",
            "urn:ogc:def:crs:EPSG::4326",
            "urn:ogc:def:crs:OGC:1.3:CRS84",
            "OGC:CRS84",
            "CRS84",
            "4326",
            "EPSG:4258",
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
            "+proj=longlat +datum=WGS84 +no_defs",
        ] {
            assert!(matches!(Crs::parse(text), Ok(Crs::Wgs84)), "{text}");
        }
        for text in ["EPSG:3857", "EPSG:900913", "urn:ogc:def:crs:EPSG::3857"] {
            assert!(matches!(Crs::parse(text), Ok(Crs::WebMercator)), "{text}");
        }
        assert!(Crs::parse("nonsense").is_err());
        assert!(Crs::parse("IAU:30100").is_err());
    }

    #[test]
    fn wkt_takes_the_outermost_authority() {
        // An ESRI .prj with no authority of its own, only its unit's.
        let esri = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433,AUTHORITY["EPSG","9122"]]]"#;
        assert!(matches!(Crs::parse(esri), Ok(Crs::Wgs84)));
        let mercator = r#"PROJCS["WGS_1984_Web_Mercator_Auxiliary_Sphere",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Mercator_Auxiliary_Sphere"],UNIT["Meter",1.0]]"#;
        assert!(matches!(Crs::parse(mercator), Ok(Crs::WebMercator)));
        let ogc = r#"PROJCS["WGS 84 / Pseudo-Mercator",GEOGCS["WGS 84",AUTHORITY["EPSG","4326"]],AUTHORITY["EPSG","3857"]]"#;
        assert!(matches!(Crs::parse(ogc), Ok(Crs::WebMercator)));
        assert_eq!(
            top_level_authority(ogc),
            Some(("EPSG".into(), "3857".into()))
        );
    }

    #[test]
    fn web_mercator_inverts_exactly() {
        let geometry = Geometry::Point(geo_types::Point::new(2_226_389.8158654715, 0.0));
        let Some(Geometry::Point(p)) = Crs::WebMercator.to_wgs84(geometry) else {
            panic!()
        };
        assert!((p.x() - 20.0).abs() < 1e-9 && p.y().abs() < 1e-9);
    }

    #[test]
    fn projjson_reads_its_id() {
        let value = serde_json::json!({"type": "ProjectedCRS", "name": "WGS 84 / Pseudo-Mercator",
            "id": {"authority": "EPSG", "code": 3857}});
        assert!(matches!(Crs::from_projjson(&value), Ok(Crs::WebMercator)));
        let named = serde_json::json!({"type": "GeographicCRS", "name": "WGS 84 (CRS84)"});
        assert!(matches!(Crs::from_projjson(&named), Ok(Crs::Wgs84)));
    }

    #[cfg(feature = "reproject")]
    #[test]
    fn epsg_codes_reproject() {
        // Poland's CS92 (EPSG:2180) on its central meridian, 19°E, at 50°N:
        // 0.9993 times the GRS80 meridian arc, less 5,300 km.
        let crs = Crs::parse("EPSG:2180").unwrap();
        assert_eq!(crs.name(), "EPSG:2180");
        let point = Geometry::Point(geo_types::Point::new(500_000.0, 236_968.449));
        let Some(Geometry::Point(p)) = crs.to_wgs84(point) else {
            panic!()
        };
        assert!((p.x() - 19.0).abs() < 1e-7, "{p:?}");
        assert!((p.y() - 50.0).abs() < 1e-7, "{p:?}");
        // UTM 33N, through its WKT as a .prj gives it.
        let utm = r#"PROJCS["WGS_1984_UTM_Zone_33N",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",15.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;
        let crs = Crs::parse(utm).unwrap();
        let point = Geometry::Point(geo_types::Point::new(500_000.0, 5_800_000.0));
        let Some(Geometry::Point(p)) = crs.to_wgs84(point) else {
            panic!()
        };
        assert!((p.x() - 15.0).abs() < 1e-6, "{p:?}");
        assert!((p.y() - 52.350_293).abs() < 1e-6, "{p:?}");
    }
}
