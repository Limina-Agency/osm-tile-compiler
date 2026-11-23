//! The opt-in `sar` (search and rescue) layer: what a search team needs that
//! OpenMapTiles leaves out or thins out. `docs/sar.md` is its schema; this is
//! version [`SCHEMA_VERSION`] of it.
//!
//! The layer is written only when asked for, and nothing here reads or
//! changes the OpenMapTiles layers. Unlike `poi` it has no label grid: every
//! matching feature is kept, since a missing shelter costs more than a
//! crowded tile.
use super::labels::{centroid_label_feature, elevation_meters};
use super::types::{Feature, Source, add_names};
use serde_json::json;

/// The layer's name in the archive.
pub const LAYER: &str = "sar";
/// The version of the layer's schema, written to the archive metadata as
/// `sar_schema`. Any change to classes, subclasses, attributes or zooms after
/// 1.0 raises it.
pub const SCHEMA_VERSION: u32 = 1;

/// The `sac_scale` values the layer writes, easiest first.
const SAC_SCALES: [&str; 6] = [
    "hiking",
    "mountain_hiking",
    "demanding_mountain_hiking",
    "alpine_hiking",
    "demanding_alpine_hiking",
    "difficult_alpine_hiking",
];
/// The `trail_visibility` values the layer writes, best first.
const TRAIL_VISIBILITIES: [&str; 6] =
    ["excellent", "good", "intermediate", "bad", "horrible", "no"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Landing,
    Emergency,
    Shelter,
    Water,
    Hazard,
    Trail,
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Self::Landing => "landing",
            Self::Emergency => "emergency",
            Self::Shelter => "shelter",
            Self::Water => "water",
            Self::Hazard => "hazard",
            Self::Trail => "trail",
        }
    }
}

/// Evaluates one OSM source, appending at most one `sar` feature.
pub fn process(source: &Source, out: &mut Vec<Feature>) {
    if source.source_layer != "osm" {
        return;
    }
    let feature = if source.is_line() {
        line(source)
    } else if source.is_point() || source.is_polygon() {
        point(source)
    } else {
        None
    };
    out.extend(feature);
}

fn point(source: &Source) -> Option<Feature> {
    let (class, subclass, min_zoom) = point_class(source)?;
    // A hut drawn as a building is placed at its interior point, as `poi`
    // places its polygons.
    let mut f = centroid_label_feature(source, LAYER, min_zoom)?;
    common(source, &mut f, class, subclass);
    if let Some(meters) = elevation_meters(source) {
        f.attrs
            .insert("ele".into(), json!((meters + 0.5).floor() as i64));
    }
    match class {
        Class::Shelter => {
            string(&mut f, "operator", source.tag("operator"));
            string(&mut f, "shelter_type", source.tag("shelter_type"));
            if let Some(capacity) = integer(source.tag("capacity")).filter(|c| *c >= 0) {
                f.attrs.insert("capacity".into(), json!(capacity));
            }
            flag(&mut f, "locked", source.tag("locked"));
            flag(&mut f, "fee", source.tag("fee"));
        }
        Class::Water => {
            flag(&mut f, "drinking_water", source.tag("drinking_water"));
            let seasonal = match boolean(source.tag("seasonal")) {
                Some(value) => Some(value),
                None => boolean(source.tag("intermittent")),
            };
            if let Some(seasonal) = seasonal {
                f.attrs
                    .insert("seasonal".into(), json!(i64::from(seasonal)));
            }
        }
        Class::Emergency | Class::Landing => {
            string(&mut f, "ref", source.tag("ref"));
            string(&mut f, "operator", source.tag("operator"));
        }
        Class::Hazard | Class::Trail => {}
    }
    // Icons are drawn around the point, so neighbouring tiles carry it too.
    f.buffer_pixels = 64.0;
    Some(f)
}

fn point_class(source: &Source) -> Option<(Class, &str, u8)> {
    let named = !source.tag("name").is_empty();
    let hut_zoom = if named { 10 } else { 12 };
    let tourism = source.tag("tourism");
    if matches!(tourism, "alpine_hut" | "wilderness_hut") {
        return Some((Class::Shelter, tourism, hut_zoom));
    }
    let amenity = source.tag("amenity");
    if amenity == "shelter" {
        return Some((Class::Shelter, amenity, 12));
    }
    let building = source.tag("building");
    if named && matches!(building, "hut" | "cabin") {
        return Some((Class::Shelter, building, 10));
    }
    let aeroway = source.tag("aeroway");
    if matches!(aeroway, "helipad" | "heliport") {
        return Some((Class::Landing, aeroway, 10));
    }
    let emergency = source.tag("emergency");
    match emergency {
        "landing_site" => return Some((Class::Landing, emergency, 10)),
        "mountain_rescue" | "ambulance_station" => {
            return Some((Class::Emergency, emergency, 10));
        }
        "access_point" => return Some((Class::Emergency, emergency, 13)),
        "phone" | "defibrillator" | "assembly_point" | "lifeguard" => {
            return Some((Class::Emergency, emergency, 14));
        }
        _ => {}
    }
    let natural = source.tag("natural");
    if natural == "spring" {
        return Some((Class::Water, natural, 12));
    }
    if matches!(amenity, "drinking_water" | "water_point") {
        return Some((Class::Water, amenity, 12));
    }
    let man_made = source.tag("man_made");
    if man_made == "water_well" {
        return Some((Class::Water, man_made, 12));
    }
    if natural == "cave_entrance" {
        return Some((Class::Hazard, natural, 14));
    }
    if matches!(man_made, "adit" | "mineshaft") {
        return Some((Class::Hazard, man_made, 14));
    }
    None
}

fn line(source: &Source) -> Option<Feature> {
    let (class, subclass, min_zoom) = line_class(source)?;
    let mut f =
        Feature::new_original_geometry(source, LAYER, source.geometry.clone(), min_zoom, 14);
    common(source, &mut f, class, subclass);
    match class {
        Class::Hazard => {
            if let Some(voltage) = voltage(source.tag("voltage")) {
                f.attrs.insert("voltage".into(), json!(voltage));
            }
        }
        Class::Trail => {
            if let Some(scale) = sac_scale(source) {
                f.attrs.insert("sac_scale".into(), json!(scale));
            }
            if let Some(visibility) = trail_visibility(source) {
                f.attrs.insert("trail_visibility".into(), json!(visibility));
            }
            if let Some(grade) = via_ferrata_scale(source) {
                f.attrs.insert("via_ferrata_scale".into(), json!(grade));
            }
        }
        _ => {}
    }
    Some(f)
}

fn line_class(source: &Source) -> Option<(Class, &str, u8)> {
    match source.tag("power") {
        "line" => return Some((Class::Hazard, "power_line", 11)),
        "minor_line" => return Some((Class::Hazard, "minor_line", 13)),
        _ => {}
    }
    if source.tag("natural") == "cliff" {
        return Some((Class::Hazard, "cliff", 13));
    }
    let highway = source.tag("highway");
    let graded = sac_scale(source).is_some()
        || trail_visibility(source).is_some()
        || via_ferrata_scale(source).is_some();
    match highway {
        "via_ferrata" => Some((Class::Trail, highway, 12)),
        "path" | "footway" | "track" | "bridleway" if graded => Some((Class::Trail, highway, 12)),
        _ => None,
    }
}

/// Class, subclass, names and the order features of the layer are drawn in.
fn common(source: &Source, f: &mut Feature, class: Class, subclass: &str) {
    f.attrs.insert("class".into(), json!(class.name()));
    f.attrs.insert("subclass".into(), json!(subclass));
    add_names(source, f);
    f.sort_key = class as i64;
}

fn sac_scale(source: &Source) -> Option<&'static str> {
    let value = source.tag("sac_scale");
    SAC_SCALES.iter().copied().find(|known| *known == value)
}

fn trail_visibility(source: &Source) -> Option<&'static str> {
    let value = source.tag("trail_visibility");
    TRAIL_VISIBILITIES
        .iter()
        .copied()
        .find(|known| *known == value)
}

fn via_ferrata_scale(source: &Source) -> Option<i64> {
    integer(source.tag("via_ferrata_scale")).filter(|grade| (0..=6).contains(grade))
}

/// The highest voltage of a `;`-separated list, in volts.
fn voltage(value: &str) -> Option<i64> {
    value
        .split(';')
        .filter_map(integer)
        .filter(|volts| *volts > 0)
        .max()
}

fn integer(value: &str) -> Option<i64> {
    value.trim().parse().ok()
}

fn boolean(value: &str) -> Option<bool> {
    match value {
        "yes" | "true" | "1" => Some(true),
        "no" | "false" | "0" => Some(false),
        _ => None,
    }
}

fn flag(f: &mut Feature, key: &str, value: &str) {
    if let Some(value) = boolean(value) {
        f.attrs.insert(key.into(), json!(i64::from(value)));
    }
}

fn string(f: &mut Feature, key: &str, value: &str) {
    if !value.is_empty() {
        f.attrs.insert(key.into(), json!(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::omt::types::SourceKind;
    use geo_types::{Geometry, LineString, Point, Polygon};

    fn source(kind: SourceKind, geometry: Geometry<f64>, tags: &[(&str, &str)]) -> Source {
        Source {
            id: 11,
            osm_id: 1,
            kind,
            source_layer: "osm".into(),
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            geometry,
            relations: vec![],
        }
    }

    fn node(tags: &[(&str, &str)]) -> Source {
        source(
            SourceKind::Node,
            Geometry::Point(Point::new(19.95, 49.25)),
            tags,
        )
    }

    fn way(tags: &[(&str, &str)]) -> Source {
        source(
            SourceKind::Way,
            Geometry::LineString(LineString::from(vec![(19.95, 49.25), (19.96, 49.26)])),
            tags,
        )
    }

    fn evaluate(source: &Source) -> Option<Feature> {
        let mut out = vec![];
        process(source, &mut out);
        assert!(out.len() <= 1);
        out.pop()
    }

    fn text<'a>(f: &'a Feature, key: &str) -> Option<&'a str> {
        f.attrs.get(key).and_then(|v| v.as_str())
    }

    fn int(f: &Feature, key: &str) -> Option<i64> {
        f.attrs.get(key).and_then(|v| v.as_i64())
    }

    #[test]
    fn every_point_class_has_its_subclass_and_zoom() {
        for (tags, class, subclass, zoom) in [
            (
                vec![("tourism", "alpine_hut"), ("name", "Murowaniec")],
                "shelter",
                "alpine_hut",
                10,
            ),
            (
                vec![("tourism", "wilderness_hut")],
                "shelter",
                "wilderness_hut",
                12,
            ),
            (
                vec![("amenity", "shelter"), ("name", "Wiata")],
                "shelter",
                "shelter",
                12,
            ),
            (
                vec![("building", "hut"), ("name", "Bacówka")],
                "shelter",
                "hut",
                10,
            ),
            (
                vec![("building", "cabin"), ("name", "Chata")],
                "shelter",
                "cabin",
                10,
            ),
            (vec![("natural", "spring")], "water", "spring", 12),
            (
                vec![("amenity", "drinking_water")],
                "water",
                "drinking_water",
                12,
            ),
            (vec![("amenity", "water_point")], "water", "water_point", 12),
            (vec![("man_made", "water_well")], "water", "water_well", 12),
            (
                vec![("emergency", "mountain_rescue")],
                "emergency",
                "mountain_rescue",
                10,
            ),
            (
                vec![("emergency", "ambulance_station")],
                "emergency",
                "ambulance_station",
                10,
            ),
            (
                vec![("emergency", "access_point"), ("ref", "MP-12")],
                "emergency",
                "access_point",
                13,
            ),
            (vec![("emergency", "phone")], "emergency", "phone", 14),
            (
                vec![("emergency", "defibrillator")],
                "emergency",
                "defibrillator",
                14,
            ),
            (
                vec![("emergency", "assembly_point")],
                "emergency",
                "assembly_point",
                14,
            ),
            (
                vec![("emergency", "lifeguard")],
                "emergency",
                "lifeguard",
                14,
            ),
            (vec![("aeroway", "helipad")], "landing", "helipad", 10),
            (vec![("aeroway", "heliport")], "landing", "heliport", 10),
            (
                vec![("emergency", "landing_site")],
                "landing",
                "landing_site",
                10,
            ),
            (
                vec![("natural", "cave_entrance")],
                "hazard",
                "cave_entrance",
                14,
            ),
            (vec![("man_made", "adit")], "hazard", "adit", 14),
            (vec![("man_made", "mineshaft")], "hazard", "mineshaft", 14),
        ] {
            let f = evaluate(&node(&tags)).unwrap_or_else(|| panic!("{tags:?}"));
            assert_eq!(f.layer, "sar");
            assert_eq!(text(&f, "class"), Some(class), "{tags:?}");
            assert_eq!(text(&f, "subclass"), Some(subclass), "{tags:?}");
            assert_eq!((f.min_zoom, f.max_zoom), (zoom, 14), "{tags:?}");
            assert_eq!(f.grid_limit, 0, "no label grid thins the layer");
            assert_eq!(f.id, 11);
        }
    }

    #[test]
    fn unnamed_buildings_and_other_tags_are_left_out() {
        for tags in [
            vec![("building", "hut")],
            vec![("emergency", "fire_hydrant")],
            vec![("amenity", "hospital")],
            vec![("natural", "peak"), ("name", "Rysy")],
            vec![("tourism", "hotel")],
        ] {
            assert!(evaluate(&node(&tags)).is_none(), "{tags:?}");
        }
    }

    #[test]
    fn supplemental_sources_are_ignored() {
        let mut spring = node(&[("natural", "spring")]);
        spring.source_layer = "ne_10m_lakes".into();
        assert!(evaluate(&spring).is_none());
    }

    #[test]
    fn shelter_attributes() {
        let f = evaluate(&node(&[
            ("tourism", "alpine_hut"),
            ("name", "Schronisko"),
            ("ele", "1500.6"),
            ("capacity", "40"),
            ("operator", "PTTK"),
            ("locked", "no"),
            ("fee", "yes"),
            ("shelter_type", "basic_hut"),
        ]))
        .unwrap();
        assert_eq!(text(&f, "name"), Some("Schronisko"));
        assert_eq!(int(&f, "ele"), Some(1501));
        assert!(!f.attrs.contains_key("ele_ft"));
        assert_eq!(int(&f, "capacity"), Some(40));
        assert_eq!(text(&f, "operator"), Some("PTTK"));
        assert_eq!(int(&f, "locked"), Some(0));
        assert_eq!(int(&f, "fee"), Some(1));
        assert_eq!(text(&f, "shelter_type"), Some("basic_hut"));
        assert_eq!(f.buffer_pixels, 64.0);
    }

    #[test]
    fn unparsed_values_are_left_out() {
        let f = evaluate(&node(&[
            ("tourism", "alpine_hut"),
            ("capacity", "about 20"),
            ("locked", "sometimes"),
        ]))
        .unwrap();
        for key in ["capacity", "locked", "fee", "ele", "name"] {
            assert!(!f.attrs.contains_key(key), "{key}");
        }
    }

    #[test]
    fn water_attributes() {
        let f = evaluate(&node(&[
            ("natural", "spring"),
            ("drinking_water", "yes"),
            ("intermittent", "yes"),
        ]))
        .unwrap();
        assert_eq!(int(&f, "drinking_water"), Some(1));
        assert_eq!(int(&f, "seasonal"), Some(1));
        let f = evaluate(&node(&[
            ("natural", "spring"),
            ("seasonal", "no"),
            ("intermittent", "yes"),
        ]))
        .unwrap();
        assert_eq!(
            int(&f, "seasonal"),
            Some(0),
            "seasonal wins over intermittent"
        );
        assert!(!f.attrs.contains_key("drinking_water"));
    }

    #[test]
    fn emergency_and_landing_carry_ref_and_operator() {
        let f = evaluate(&node(&[("emergency", "access_point"), ("ref", "KR-101")])).unwrap();
        assert_eq!(text(&f, "ref"), Some("KR-101"));
        let f = evaluate(&node(&[
            ("aeroway", "helipad"),
            ("operator", "LPR"),
            ("ele", "900"),
        ]))
        .unwrap();
        assert_eq!(text(&f, "operator"), Some("LPR"));
        assert_eq!(int(&f, "ele"), Some(900));
    }

    #[test]
    fn a_hut_drawn_as_a_polygon_becomes_a_point() {
        let ring = LineString::from(vec![
            (19.0, 49.0),
            (19.001, 49.0),
            (19.001, 49.001),
            (19.0, 49.001),
            (19.0, 49.0),
        ]);
        let hut = source(
            SourceKind::Way,
            Geometry::Polygon(Polygon::new(ring.clone(), vec![])),
            &[("tourism", "alpine_hut"), ("name", "Hala")],
        );
        let f = evaluate(&hut).unwrap();
        assert!(matches!(f.geometry, Geometry::Point(_)));
        // The line view of the same closed way adds nothing.
        let outline = source(
            SourceKind::Way,
            Geometry::LineString(ring),
            &[("tourism", "alpine_hut"), ("name", "Hala")],
        );
        assert!(evaluate(&outline).is_none());
    }

    #[test]
    fn hazard_lines() {
        let f = evaluate(&way(&[("power", "line"), ("voltage", "110000;220000")])).unwrap();
        assert_eq!(text(&f, "class"), Some("hazard"));
        assert_eq!(text(&f, "subclass"), Some("power_line"));
        assert_eq!(int(&f, "voltage"), Some(220_000));
        assert_eq!(f.min_zoom, 11);
        let f = evaluate(&way(&[("power", "minor_line"), ("voltage", "unknown")])).unwrap();
        assert_eq!(text(&f, "subclass"), Some("minor_line"));
        assert_eq!(f.min_zoom, 13);
        assert!(!f.attrs.contains_key("voltage"));
        let f = evaluate(&way(&[("natural", "cliff")])).unwrap();
        assert_eq!(text(&f, "subclass"), Some("cliff"));
        assert_eq!(f.min_zoom, 13);
        assert!(matches!(f.geometry, Geometry::LineString(_)));
    }

    #[test]
    fn trails_need_a_grade() {
        assert!(evaluate(&way(&[("highway", "path")])).is_none());
        assert!(evaluate(&way(&[("highway", "path"), ("sac_scale", "very hard")])).is_none());
        assert!(evaluate(&way(&[("highway", "residential"), ("sac_scale", "hiking")])).is_none());
        let f = evaluate(&way(&[
            ("highway", "path"),
            ("sac_scale", "demanding_alpine_hiking"),
            ("trail_visibility", "bad"),
            ("name", "Orla Perć"),
        ]))
        .unwrap();
        assert_eq!(text(&f, "class"), Some("trail"));
        assert_eq!(text(&f, "subclass"), Some("path"));
        assert_eq!(text(&f, "sac_scale"), Some("demanding_alpine_hiking"));
        assert_eq!(text(&f, "trail_visibility"), Some("bad"));
        assert_eq!(text(&f, "name"), Some("Orla Perć"));
        assert_eq!(f.min_zoom, 12);
        for highway in ["footway", "track", "bridleway"] {
            let f = evaluate(&way(&[("highway", highway), ("trail_visibility", "good")])).unwrap();
            assert_eq!(text(&f, "subclass"), Some(highway));
        }
        let f = evaluate(&way(&[
            ("highway", "via_ferrata"),
            ("via_ferrata_scale", "3"),
        ]))
        .unwrap();
        assert_eq!(text(&f, "subclass"), Some("via_ferrata"));
        assert_eq!(int(&f, "via_ferrata_scale"), Some(3));
        let f = evaluate(&way(&[
            ("highway", "via_ferrata"),
            ("via_ferrata_scale", "9"),
        ]))
        .unwrap();
        assert!(!f.attrs.contains_key("via_ferrata_scale"));
        let f = evaluate(&way(&[("highway", "path"), ("via_ferrata_scale", "2")])).unwrap();
        assert_eq!(int(&f, "via_ferrata_scale"), Some(2));
    }
}
