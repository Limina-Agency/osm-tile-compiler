//! Direct OMT 3.16 transport/aeroway/boundary source-policy port (91516fdf).
//! Renderer owns line merging/length-after-merge. Country borders wait for
//! `State::finish`, which adds ADM0_L/R once every OSM source has been read.
//! Country-dependent GB/IE inferred networks and junction way preprocessing
//! require supplied context; `_junction_highway` carries the latter.
use super::java_hashmap::{grouping_by_long_order, hash_set_long_order};
use super::types::{
    Feature, Source, SourceKind, TOP, Tags, add_names, parse_number, parse_profile_long,
};
use geo_types::{Coord, Geometry};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

fn listed(value: &str, values: &str) -> bool {
    !value.is_empty() && values.split_whitespace().any(|v| v == value)
}
fn yes(value: &str) -> bool {
    !matches!(value, "" | "0" | "false" | "no")
}
fn set(f: &mut Feature, key: &str, value: &str) {
    if !value.is_empty() {
        f.attrs.insert(key.into(), json!(value));
    }
}
/// `layer` as Planetiler reads it everywhere: generated Tables call
/// `SourceFeature.getLong` and `Imposm3Parsers.wayzorder` calls
/// `Parse.parseLong`, which both accept a numeric prefix, so `-1;0` is -1,
/// and give 0 for an unparseable value.
fn layer(source: &Source) -> i64 {
    parse_profile_long(source.tag("layer")).unwrap_or(0)
}
fn level(source: &Source) -> Option<i64> {
    parse_profile_long(source.tag("level"))
}
fn world_length(source: &Source) -> f64 {
    fn line_length(line: &geo_types::LineString<f64>) -> f64 {
        let mut prev = None;
        let mut length = 0.0;
        for p in &line.0 {
            let p = crate::coordinate_groups::lng_lat_to_world(p.x, p.y, 0);
            if let Some(last) = prev {
                let last: crate::coordinate_groups::WorldPoint = last;
                length += (p.x - last.x).hypot(p.y - last.y);
            }
            prev = Some(p);
        }
        length
    }
    match &source.geometry {
        geo_types::Geometry::LineString(line) => line_length(line),
        geo_types::Geometry::MultiLineString(lines) => lines.0.iter().map(line_length).sum(),
        _ => 0.0,
    }
}

// Pinned Transportation.TRUNK_Z0_UPGRADE_LENGTH compares normalized-world
// source.length() with metersToPixelAtEquator(0, 500), a zoom-zero *pixel*
// length. Preserve that 256x-unit mismatch for OMT compatibility.
const TRUNK_Z0_UPGRADE_LENGTH: f64 = 500.0 / ((2.0 * std::f64::consts::PI * 6_378_137.0) / 256.0);
fn size_minzoom(world_length: f64, pixels: f64, min: u8, max: u8) -> u8 {
    let length = world_length * 256.0;
    (min..max)
        .find(|z| length * 2_f64.powi(*z as i32) >= pixels)
        .unwrap_or(max)
}
fn brunnel(source: &Source) -> &'static str {
    if yes(source.tag("bridge")) {
        "bridge"
    } else if yes(source.tag("tunnel")) {
        "tunnel"
    } else if yes(source.tag("ford")) {
        "ford"
    } else {
        ""
    }
}
fn class(
    highway: &str,
    public_transport: &str,
    construction: &str,
    man_made: &str,
) -> Option<String> {
    let base = match highway {
        "motorway" | "motorway_link" => "motorway",
        "trunk" | "trunk_link" => "trunk",
        "primary" | "primary_link" => "primary",
        "secondary" | "secondary_link" => "secondary",
        "tertiary" | "tertiary_link" => "tertiary",
        "unclassified" | "residential" | "living_street" | "road" => "minor",
        "pedestrian" | "path" | "footway" | "cycleway" | "steps" | "bridleway" | "corridor" => {
            "path"
        }
        // The pinned FieldMappings.Class evaluates public_transport=platform
        // in its path entry before its service/track/construction entries.
        _ if public_transport == "platform" => "path",
        "service" => "service",
        "track" => "track",
        "raceway" => "raceway",
        "busway" => "busway",
        "bus_guideway" => "bus_guideway",
        "construction" => {
            let construction = if construction.is_empty() {
                "residential"
            } else {
                construction
            };
            let c = class(construction, public_transport, "", "")?;
            return listed(
                &c,
                "motorway trunk primary secondary tertiary minor path service track raceway",
            )
            .then(|| format!("{c}_construction"));
        }
        // Transportation.highwayClass reads man_made only when highway and
        // public_transport are both empty.
        "" if public_transport.is_empty() && listed(man_made, "bridge pier") => man_made,
        _ => return None,
    };
    Some(base.into())
}
fn network(network: &str, reference: &str) -> Option<(&'static str, u8)> {
    Some(match network {
        "US:I" => ("us-interstate", 0),
        "US:US" => ("us-highway", 1),
        n if n.starts_with("US:") && n.len() == 5 => ("us-state", 2),
        n if n.starts_with("CA:transcanada") => ("ca-transcanada", 3),
        "CA:QC:A" => ("ca-provincial-arterial", 4),
        "CA:ON:primary"
            if (reference.len() == 3 && reference.starts_with('4')) || reference == "QEW" =>
        {
            ("ca-provincial-arterial", 4)
        }
        "CA:MB:PTH" if reference == "75" => ("ca-provincial-arterial", 4),
        "CA:AB:primary" if listed(reference, "2 3 4") => ("ca-provincial-arterial", 4),
        "CA:BC" if listed(reference, "3 5 99") => ("ca-provincial-arterial", 4),
        n if n.starts_with("CA:") && (n.len() == 5 || n.as_bytes().get(5) == Some(&b':')) => {
            ("ca-provincial", 5)
        }
        "omt-gb-motorway" => ("gb-motorway", 6),
        "omt-gb-trunk" => ("gb-trunk", 7),
        "omt-gb-primary" => ("gb-primary", 8),
        "omt-ie-motorway" => ("ie-motorway", 9),
        "omt-ie-national" => ("ie-national", 10),
        "omt-ie-regional" => ("ie-regional", 11),
        _ => return None,
    })
}
fn relations(source: &Source) -> Vec<&Tags> {
    let mut result = source
        .relations
        .iter()
        .filter(|r| {
            let route = r.get("route").map(String::as_str).unwrap_or("");
            listed(route, "road hiking")
                && (r.contains_key("network")
                    || r.contains_key("osmc:symbol")
                    || r.contains_key("colour"))
        })
        .collect::<Vec<_>>();
    // Planetiler's RELATION_ORDERING compares Java strings: lengths and
    // characters in UTF-16 code units, not UTF-8 bytes ("SŁ-162s" is 7 long).
    result.sort_by_cached_key(|r| {
        let n = r.get("network").map(String::as_str).unwrap_or("");
        let reference = r.get("ref").map(String::as_str).unwrap_or("");
        let reference = reference.encode_utf16().collect::<Vec<_>>();
        (
            network(n, r.get("ref").map(String::as_str).unwrap_or(""))
                .map(|x| x.1)
                .unwrap_or(255),
            n.encode_utf16().collect::<Vec<_>>(),
            reference.len(),
            reference,
        )
    });
    result.dedup();
    result
}
fn road_zoom(source: &Source, class: &str, relations: &[&Tags]) -> u8 {
    let base = class.trim_end_matches("_construction");
    let rank = relations
        .iter()
        .map(
            |r| match r.get("network").map(String::as_str).unwrap_or("") {
                "iwn" | "nwn" | "rwn" => 1,
                "lwn" => 2,
                _ if r.contains_key("osmc:symbol") || r.contains_key("colour") => 2,
                _ => 3,
            },
        )
        .min()
        .unwrap_or(3);
    let networks = relations
        .iter()
        .filter_map(|r| {
            network(
                r.get("network").map(String::as_str).unwrap_or(""),
                r.get("ref").map(String::as_str).unwrap_or(""),
            )
        })
        .collect::<Vec<_>>();
    let main_network = networks
        .iter()
        .any(|(_, n)| matches!(n, 0 | 1 | 3 | 4 | 6 | 7 | 9 | 10));
    let main_motorway_network = networks
        .iter()
        .any(|(_, n)| matches!(n, 0 | 3 | 4 | 6 | 9 | 10));
    let mut zoom = if source.tag("man_made") == "pier" {
        13
    } else if listed(source.tag("highway"), "residential unclassified") {
        12
    } else {
        match base {
            "motorway" => {
                if networks.is_empty() || main_motorway_network {
                    4
                } else {
                    5
                }
            }
            "trunk" => {
                if main_motorway_network {
                    4
                } else if main_network || world_length(source) < TRUNK_Z0_UPGRADE_LENGTH {
                    5
                } else {
                    6
                }
            }
            "primary" => 7,
            "secondary" => 9,
            "tertiary" | "busway" | "bus_guideway" => 11,
            "raceway" => 12,
            "minor" => 13,
            "service" => {
                if listed(source.tag("service"), "driveway parking_aisle") {
                    14
                } else {
                    13
                }
            }
            "path" | "track" => {
                if rank == 1 {
                    12
                } else if rank == 2 || source.has("name") || source.has("sac_scale") {
                    13
                } else {
                    14
                }
            }
            _ => 14,
        }
    };
    if source.tag("highway").ends_with("_link") || source.tag("construction").ends_with("_link") {
        zoom = zoom.max(9);
    }
    zoom
}
fn zorder(source: &Source) -> i64 {
    // Exact Imposm3Parsers.wayzorder ranking from pinned Planetiler core.
    let rank = match source.tag("highway") {
        "motorway" => 9,
        "trunk" => 8,
        "primary" => 6,
        "secondary" => 5,
        "tertiary" => 4,
        "residential" | "unclassified" | "minor" | "road" | "tertiary_link" | "secondary_link"
        | "primary_link" | "trunk_link" | "motorway_link" => 3,
        _ if source.has("railway") => 7,
        _ => 0,
    };
    let z = layer(source)
        .wrapping_mul(10)
        .wrapping_add(rank)
        .wrapping_add(if yes(source.tag("bridge")) { 10 } else { 0 })
        .wrapping_sub(if yes(source.tag("tunnel")) { 10 } else { 0 });
    if (-9999..=9999).contains(&z) { z } else { 0 }
}
fn structural(source: &Source, f: &mut Feature, brunnel_zoom: u8, layer_zoom: u8) {
    set(f, "brunnel", brunnel(source));
    f.attr_minzoom.insert("brunnel".into(), brunnel_zoom);
    let l = layer(source);
    if l != 0 {
        f.attrs.insert("layer".into(), json!(l));
        f.attr_minzoom.insert("layer".into(), layer_zoom);
    }
}
fn service(source: &Source, f: &mut Feature, min: u8) {
    let s = source.tag("service");
    if listed(s, "spur yard siding crossover driveway alley parking_aisle") {
        set(f, "service", s);
        f.attr_minzoom.insert("service".into(), min);
    }
}
fn direction(source: &Source) -> i64 {
    match source.tag("oneway") {
        "-1" => -1,
        "yes" | "true" | "1" => 1,
        _ => 0,
    }
}
fn surface(source: &Source) -> &'static str {
    let value = if source.has("surface") {
        source.tag("surface")
    } else {
        source.tag("tracktype")
    };
    if listed(
        value,
        "paved asphalt cobblestone concrete concrete:lanes concrete:plates metal paving_stones sett unhewn_cobblestone wood grade1",
    ) {
        "paved"
    } else if listed(
        value,
        "unpaved compacted dirt earth fine_gravel grass grass_paver gravel gravel_turf ground ice mud pebblestone salt sand snow woodchips",
    ) {
        "unpaved"
    } else {
        ""
    }
}

/// Evaluates a source without the country-border state, emitting its country
/// borders at once without ADM0_L/R. Use `evaluate` for OSM sources.
pub fn process(source: &Source, out: &mut Vec<Feature>) {
    let mut contribution = Contribution::default();
    evaluate(source, out, &mut contribution);
    out.extend(contribution.borders.into_iter().map(|b| b.feature));
}

/// Evaluates a source, holding country borders back in `contribution`.
pub fn evaluate(source: &Source, out: &mut Vec<Feature>, contribution: &mut Contribution) {
    boundary(source, out, contribution);
    if source.source_layer.starts_with("ne_") {
        return;
    }
    let a = source.tag("aeroway");
    let aa = source.tag("area:aeroway");
    if (source.is_polygon()
        && (listed(a, "aerodrome heliport runway helipad taxiway apron")
            || listed(aa, "aerodrome heliport runway helipad taxiway apron")))
        || (source.is_line() && listed(a, "runway taxiway"))
        || (source.is_point() && a == "gate")
    {
        let mut f = Feature::new_original_geometry(
            source,
            "aeroway",
            source.geometry.clone(),
            if source.is_point() { 14 } else { 10 },
            TOP,
        );
        set(
            &mut f,
            "class",
            if listed(a, "aerodrome heliport runway helipad taxiway apron gate") {
                a
            } else {
                aa
            },
        );
        set(&mut f, "ref", source.tag("ref"));
        if source.is_polygon() {
            f.min_size = 2.0;
        }
        out.push(f);
    }
    if source.is_point() && source.tag("highway") == "motorway_junction" {
        if let Some(class) = class(source.tag("_junction_highway"), "", "", "") {
            let mut f = Feature::new_original_geometry(
                source,
                "transportation_name",
                source.geometry.clone(),
                10,
                TOP,
            );
            f.buffer_pixels = 8.0;
            add_names(source, &mut f);
            set(&mut f, "class", &class);
            set(&mut f, "subclass", "junction");
            set(&mut f, "ref", source.tag("ref"));
            if source.has("ref") {
                f.attrs.insert(
                    "ref_length".into(),
                    json!(source.tag("ref").chars().count()),
                );
            }
            let l = layer(source);
            if l != 0 {
                f.attrs.insert("layer".into(), json!(l));
            }
            // setSortKeyDescending: SORT_KEY_MAX + SORT_KEY_MIN - zOrder.
            f.sort_key = -1 - zorder(source);
            out.push(f);
        }
        return;
    }
    if source.is_polygon() {
        let h = source.tag("highway");
        let m = source.tag("man_made");
        if (listed(m, "bridge pier")
            || ((yes(source.tag("area")) || matches!(source.kind, SourceKind::Relation))
                && layer(source) >= 0))
            && (listed(
                h,
                "path cycleway bridleway footway corridor pedestrian steps",
            ) || source.tag("public_transport") == "platform"
                || listed(m, "bridge pier"))
            && let Some(c) = class(h, source.tag("public_transport"), "", m)
        {
            let mut f = Feature::new_original_geometry(
                source,
                "transportation",
                source.geometry.clone(),
                13,
                TOP,
            );
            f.buffer_pixels = 4.0;
            set(&mut f, "class", &c);
            if c == "path" {
                set(
                    &mut f,
                    "subclass",
                    if source.has("public_transport") {
                        source.tag("public_transport")
                    } else {
                        h
                    },
                );
            }
            if m == "bridge" {
                set(&mut f, "brunnel", "bridge");
            }
            let l = layer(source);
            if l != 0 {
                f.attrs.insert("layer".into(), json!(l));
            }
            f.sort_key = zorder(source);
            out.push(f);
        }
        return;
    }
    if !source.is_line() {
        return;
    }
    let h = source.tag("highway");
    // Tables.OsmHighwayLinestring accepts man_made=pier, but bridge alone
    // belongs only to the polygon table. Other eligible tags still permit it.
    let highway_mapping = listed(
        h,
        "motorway motorway_link trunk trunk_link primary primary_link secondary secondary_link tertiary tertiary_link unclassified residential living_street road pedestrian path footway cycleway steps bridleway corridor service track raceway busway bus_guideway construction",
    ) || source.tag("public_transport") == "platform"
        || source.tag("man_made") == "pier"
        || listed(source.tag("service"), "driveway parking_aisle");
    if !yes(source.tag("area"))
        && highway_mapping
        && let Some(c) = class(
            h,
            source.tag("public_transport"),
            source.tag("construction"),
            source.tag("man_made"),
        )
    {
        let relations = relations(source);
        let min = road_zoom(source, &c, &relations);
        let mut f = Feature::new_original_geometry(
            source,
            "transportation",
            source.geometry.clone(),
            min,
            TOP,
        );
        f.buffer_pixels = 4.0;
        f.min_size = 0.0;
        f.sort_key = zorder(source);
        set(&mut f, "class", &c);
        if c == "path" {
            set(
                &mut f,
                "subclass",
                if source.has("public_transport") {
                    source.tag("public_transport")
                } else {
                    h
                },
            );
        }
        if let Some(r) = relations.first()
            && let Some((network, _)) = network(
                r.get("network").map(String::as_str).unwrap_or(""),
                r.get("ref").map(String::as_str).unwrap_or(""),
            )
        {
            set(&mut f, "network", network);
        }
        structural(source, &mut f, 4, 9);
        let length = world_length(source);
        f.attr_minzoom
            .insert("brunnel".into(), size_minzoom(length, 4.0, 4, 12));
        f.attr_minzoom
            .insert("layer".into(), size_minzoom(length, 4.0, 9, 12));
        for key in ["bicycle", "foot", "horse", "mtb:scale"] {
            let dest = if key == "mtb:scale" { "mtb_scale" } else { key };
            set(&mut f, dest, source.tag(key));
            f.attr_minzoom.insert(dest.into(), 9);
        }
        if listed(source.tag("access"), "private no") {
            set(&mut f, "access", "no");
            f.attr_minzoom.insert("access".into(), 9);
        }
        if yes(source.tag("toll")) {
            f.attrs.insert("toll".into(), json!(1));
            f.attr_minzoom.insert("toll".into(), 9);
        }
        if listed(h, "path footway cycleway bridleway") {
            let official = if source.tag("informal") == "yes" {
                Some(0)
            } else if source.tag("informal") == "no" || source.has("operator") {
                Some(1)
            } else {
                None
            };
            if let Some(v) = official {
                f.attrs.insert("official".into(), json!(v));
                f.attr_minzoom.insert("official".into(), 9);
            }
        }
        let link = h.ends_with("_link");
        let ramp = yes(source.tag("ramp"));
        if link || ramp {
            f.attrs.insert("ramp".into(), json!(1));
            f.attr_minzoom
                .insert("ramp".into(), if link { 9 } else { 12 });
        }
        if yes(source.tag("expressway")) && h != "motorway" && !link && !ramp {
            f.attrs.insert("expressway".into(), json!(1));
            f.attr_minzoom.insert("expressway".into(), 8);
        }
        service(source, &mut f, 12);
        let oneway = direction(source);
        if oneway != 0 {
            f.attrs.insert("oneway".into(), json!(oneway));
            f.attr_minzoom.insert("oneway".into(), 12);
        }
        set(&mut f, "surface", surface(source));
        f.attr_minzoom.insert("surface".into(), 12);
        if listed(h, "footway steps") {
            if let Some(value) = level(source) {
                f.attrs.insert("level".into(), json!(value));
            }
            if yes(source.tag("indoor")) {
                f.attrs.insert("indoor".into(), json!(1));
            }
        }
        if c.trim_end_matches("_construction") == "trunk"
            && min == 5
            && world_length(source) < TRUNK_Z0_UPGRADE_LENGTH
        {
            let mut low = f.clone();
            low.max_zoom = 5;
            set(&mut low, "class", &c.replace("trunk", "motorway"));
            out.push(low);
            f.min_zoom = 6;
        }
        out.push(f);
        // TransportationName calls highwayClass with a null publicTransport,
        // unlike Transportation. A platform may render as a path but must
        // not inherit a road name from highway=bus_stop or highway=platform.
        if !h.is_empty()
            && let Some(name_class) =
                class(h, "", source.tag("construction"), source.tag("man_made"))
        {
            road_name(source, &name_class, min, &relations, out);
        }
    }
    let rail = source.tag("railway");
    if listed(
        rail,
        "rail narrow_gauge preserved funicular subway light_rail monorail tram",
    ) {
        let min = if source.has("service") {
            14
        } else if rail == "rail" {
            if source.tag("usage") == "main" { 8 } else { 10 }
        } else if rail == "narrow_gauge" {
            10
        } else if rail == "light_rail" {
            11
        } else {
            14
        };
        let mut f = Feature::new_original_geometry(
            source,
            "transportation",
            source.geometry.clone(),
            min,
            TOP,
        );
        f.buffer_pixels = 4.0;
        f.min_size = 0.0;
        f.sort_key = zorder(source);
        set(
            &mut f,
            "class",
            if listed(rail, "rail narrow_gauge preserved funicular") {
                "rail"
            } else {
                "transit"
            },
        );
        set(&mut f, "subclass", rail);
        service(source, &mut f, 0);
        structural(source, &mut f, 10, 9);
        if yes(source.tag("ramp")) {
            f.attrs.insert("ramp".into(), json!(1));
        }
        out.push(f);
    }
    let aerial = source.tag("aerialway");
    let ferry = source.tag("route") == "ferry";
    if listed(
        aerial,
        "chair_lift drag_lift platter t-bar gondola cable_car j-bar mixed_lift",
    ) || ferry
    {
        let mut f = Feature::new_original_geometry(
            source,
            "transportation",
            source.geometry.clone(),
            if ferry { 4 } else { 12 },
            TOP,
        );
        f.buffer_pixels = 4.0;
        f.min_size = 0.0;
        f.sort_key = zorder(source);
        set(&mut f, "class", if ferry { "ferry" } else { "aerialway" });
        if !ferry {
            set(&mut f, "subclass", aerial);
            let d = direction(source);
            if d != 0 {
                f.attrs.insert("oneway".into(), json!(d));
            }
        }
        structural(source, &mut f, 0, 0);
        service(source, &mut f, 0);
        if yes(source.tag("ramp")) {
            f.attrs.insert("ramp".into(), json!(1));
        }
        if ferry {
            let mut low = f.clone();
            // The pinned setMinPixelSizeBelowZoom(10, 32) includes zoom 10.
            low.max_zoom = 10;
            low.min_size = 32.0;
            low.min_size_at_max_zoom = 32.0;
            out.push(low);
            f.min_zoom = 11;
        }
        out.push(f);
        if source.has("name") {
            let mut f = Feature::new_original_geometry(
                source,
                "transportation_name",
                source.geometry.clone(),
                12,
                TOP,
            );
            f.buffer_pixels = 8.0;
            f.min_size = 0.0;
            add_names(source, &mut f);
            set(&mut f, "class", if ferry { "ferry" } else { "aerialway" });
            if !ferry {
                set(&mut f, "subclass", aerial);
            }
            out.push(f);
        }
    }
}

fn road_name(
    source: &Source,
    class: &str,
    road_min: u8,
    relations: &[&Tags],
    out: &mut Vec<Feature>,
) {
    let first = relations.iter().find(|r| {
        network(
            r.get("network").map(String::as_str).unwrap_or(""),
            r.get("ref").map(String::as_str).unwrap_or(""),
        )
        .is_some()
    });
    let reference = first
        .and_then(|r| r.get("ref"))
        .filter(|v| !v.is_empty())
        .map(String::as_str)
        .unwrap_or(source.tag("ref"));
    if !source.has("name") && reference.is_empty() {
        return;
    }
    let base = class.trim_end_matches("_construction");
    let min = match base {
        "trunk" => 8,
        "motorway" => 6,
        _ if source.tag("highway").ends_with("_link") => 13,
        _ => 12,
    };
    let mut f = Feature::new_original_geometry(
        source,
        "transportation_name",
        source.geometry.clone(),
        min.max(road_min),
        TOP,
    );
    f.buffer_pixels = 8.0;
    f.min_size = 0.0;
    f.sort_key = zorder(source);
    add_names(source, &mut f);
    set(&mut f, "class", class);
    set(&mut f, "ref", reference);
    if !reference.is_empty() {
        f.attrs
            .insert("ref_length".into(), json!(reference.chars().count()));
        set(
            &mut f,
            "network",
            first
                .and_then(|r| {
                    network(
                        r.get("network").map(String::as_str).unwrap_or(""),
                        reference,
                    )
                })
                .map(|v| v.0)
                .unwrap_or("road"),
        );
    }
    if class == "path" {
        set(&mut f, "subclass", source.tag("highway"));
    }
    // Pinned TransportationName deduplicates route display fields even when
    // distinct relation memberships differ in other tags. The unique-route
    // count, not the membership index, numbers route_1, route_2, ... .
    let mut seen_routes = BTreeSet::new();
    for r in relations {
        let signature = format!(
            "{}={}={}={}",
            r.get("network").map(String::as_str).unwrap_or("null"),
            r.get("ref").map(String::as_str).unwrap_or(""),
            r.get("name").map(String::as_str).unwrap_or(""),
            r.get("colour").map(String::as_str).unwrap_or(""),
        );
        if !seen_routes.insert(signature) {
            continue;
        }
        for key in ["network", "ref", "name", "colour"] {
            if let Some(v) = r.get(key).filter(|v| !v.is_empty()) {
                set(&mut f, &format!("route_{}_{}", seen_routes.len(), key), v);
            }
        }
    }
    if listed(source.tag("highway"), "footway steps") {
        for key in ["layer", "level"] {
            let value = if key == "level" {
                level(source)
            } else {
                let value = layer(source);
                (value != 0).then_some(value)
            };
            if let Some(value) = value {
                f.attrs.insert(key.into(), json!(value));
                f.attr_minzoom.insert(key.into(), 12);
            }
        }
        if yes(source.tag("indoor")) {
            f.attrs.insert("indoor".into(), json!(1));
            f.attr_minzoom.insert("indoor".into(), 12);
        }
    }
    for z in f.min_zoom..=11 {
        let meters = match z {
            6 | 7 => 20_000.0,
            8 => 14_000.0,
            9 => 8_000.0,
            10 => 4_000.0,
            11 => 2_000.0,
            _ => 0.0,
        };
        let mut low = f.clone();
        low.min_zoom = z;
        low.max_zoom = z;
        low.buffer_pixels = meters / 40_075_016.685_578_49 * 256.0 * 2_f64.powi(z as i32);
        out.push(low);
    }
    f.min_zoom = f.min_zoom.max(12);
    out.push(f);
}

fn disputed(tags: &Tags) -> bool {
    tags.get("disputed").is_some_and(|v| yes(v))
        || tags.get("dispute").is_some_and(|v| yes(v))
        || tags.get("border_status").is_some_and(|v| v == "dispute")
        || tags.contains_key("disputed_by")
        || tags.contains_key("claimed_by")
}
/// Boundary.editName.
fn edit_name(name: &str) -> String {
    name.replace(" at ", "")
        .chars()
        // Java's `\s` matches ASCII whitespace only.
        .filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '\u{c}' | '\r'))
        .collect::<String>()
        .replace("Extentof", "")
}
fn boundary(source: &Source, out: &mut Vec<Feature>, contribution: &mut Contribution) {
    if source.source_layer.starts_with("ne_") {
        let info = match source.source_layer.as_str() {
            "ne_110m_admin_0_boundary_lines_land" => Some((2, 0, 0)),
            "ne_50m_admin_0_boundary_lines_land" => Some((2, 1, 3)),
            "ne_10m_admin_0_boundary_lines_land" if source.tag("featurecla") != "Lease limit" => {
                Some((2, 4, 4))
            }
            "ne_10m_admin_1_states_provinces_lines" => parse_number(source.tag("min_zoom"))
                .and_then(|z| {
                    if z <= 7.0 {
                        Some((4, 1, 4))
                    } else if z <= 7.7 {
                        Some((4, 4, 4))
                    } else {
                        None
                    }
                }),
            _ => None,
        };
        if let Some((level, min, max)) = info
            && let Some(geometry) = source.line()
        {
            let mut f = Feature::new_original_geometry(source, "boundary", geometry, min, max);
            f.buffer_pixels = 4.0;
            f.min_size = 0.0;
            f.min_size_at_max_zoom = 0.0;
            f.attrs.insert("admin_level".into(), json!(level));
            f.attrs.insert("maritime".into(), json!(0));
            f.attrs.insert(
                "disputed".into(),
                json!(i32::from(source.tag("featurecla").starts_with("Disputed"))),
            );
            out.push(f);
        }
        return;
    }
    if source.is_polygon()
        && source.tag("boundary") == "aboriginal_lands"
        && source.tag("type") == "boundary"
    {
        let mut f =
            Feature::new_original_geometry(source, "boundary", source.geometry.clone(), 4, TOP);
        f.buffer_pixels = 4.0;
        set(&mut f, "class", "aboriginal_lands");
        add_names(source, &mut f);
        // setMinPixelSizeBelowZoom(13, 4): from z14 the default size.
        let mut low = f.clone();
        low.max_zoom = 13;
        low.min_size = 4.0;
        f.min_zoom = 14;
        out.extend([low, f]);
    }
    if !source.is_line() {
        return;
    }
    let rels = source
        .relations
        .iter()
        .filter(|r| {
            r.get("type").is_some_and(|v| v == "boundary")
                && r.get("boundary").is_some_and(|v| v == "administrative")
                && r.get("admin_level")
                    .and_then(|v| parse_number(v))
                    .is_some_and(|v| (2.0..=10.0).contains(&v))
        })
        .collect::<Vec<_>>();
    let Some(level) = rels
        .iter()
        .filter_map(|r| {
            r.get("admin_level")
                .and_then(|v| parse_number(v))
                .map(|v| v.round() as i64)
        })
        .min()
    else {
        return;
    };
    // The pinned profile defers country borders when its running minimum is
    // level 2 and the current relation has a country code. Boundary.finish()
    // rebuilds them through SimpleFeature.fromWorldGeometry(raw_way_id), whose
    // vector-tile ID ends in 0 rather than the original OSM way suffix 2.
    let mut running_level = i64::MAX;
    let mut regions = Vec::new();
    for rel in &rels {
        if let Some(relation_level) = rel
            .get("admin_level")
            .and_then(|v| parse_number(v))
            .map(|v| v.round() as i64)
        {
            running_level = running_level.min(relation_level);
            if running_level == 2
                && let Some(code) = rel.get("ISO3166-1:alpha3")
            {
                regions.push((relation_id(rel), code.clone()));
            }
        }
    }
    let deferred_country_border = !regions.is_empty();
    let d = disputed(&source.tags) || rels.iter().any(|r| disputed(r));
    let maritime = yes(source.tag("maritime"))
        || source.tag("natural") == "coastline"
        || source.tag("boundary_type") == "maritime";
    let min = if maritime && level == 2 {
        4
    } else if level <= 4 {
        5
    } else if level <= 6 {
        9
    } else if level <= 8 {
        11
    } else {
        12
    };
    let mut f =
        Feature::new_original_geometry(source, "boundary", source.geometry.clone(), min, TOP);
    if deferred_country_border {
        f.id = (source.id / 10) * 10;
    }
    f.buffer_pixels = 4.0;
    f.min_size = 0.0;
    f.min_size_at_max_zoom = 0.0;
    f.attrs.insert("admin_level".into(), json!(level));
    f.attrs.insert("disputed".into(), json!(i32::from(d)));
    f.attrs
        .insert("maritime".into(), json!(i32::from(maritime)));
    // The first disputed relation with each value wins, then the way itself
    // if it is disputed.
    let way_disputed = disputed(&source.tags);
    let disputed_value = |key: &str| {
        rels.iter()
            .filter(|r| disputed(r))
            .find_map(|r| r.get(key))
            .or_else(|| way_disputed.then(|| source.tags.get(key)).flatten())
    };
    if let Some(claimed_by) = disputed_value("claimed_by") {
        set(&mut f, "claimed_by", claimed_by);
    }
    if let Some(name) = disputed_value("name") {
        set(&mut f, "disputed_name", &edit_name(name));
    }
    if !deferred_country_border {
        out.push(f);
        return;
    }
    // LineMerger drops repeated points, and a way with fewer than two distinct
    // points merges to nothing.
    let Geometry::LineString(line) = &source.geometry else {
        return;
    };
    let mut world: Vec<Coord<f64>> = Vec::with_capacity(line.0.len());
    for p in &line.0 {
        let p = crate::coordinate_groups::lng_lat_to_world(p.x, p.y, 0);
        let p = Coord { x: p.x, y: p.y };
        if world.last() != Some(&p) {
            world.push(p);
        }
    }
    if world.len() < 2 {
        return;
    }
    // A way listed twice in a relation still outlines it once.
    let mut seen = BTreeSet::new();
    let outlines = rels
        .iter()
        .filter(|r| relation_level(r) <= 2)
        .map(|r| relation_id(r))
        .filter(|id| seen.insert(*id))
        .collect::<Vec<_>>();
    // regionIds is a HashSet<Long>; getBorderingRegions iterates it.
    let mut seen = BTreeSet::new();
    regions.retain(|(id, _)| seen.insert(*id));
    let ids = regions.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    regions = hash_set_long_order(&ids)
        .into_iter()
        .map(|i| regions[i].clone())
        .collect();
    contribution.borders.push(CountryBorder {
        feature: f,
        adm0: level == 2 && !d,
        regions,
        outlines,
        line: world,
    });
}

fn relation_id(rel: &Tags) -> i64 {
    rel.get("_id").and_then(|v| v.parse().ok()).unwrap_or(0)
}
fn relation_level(rel: &Tags) -> i64 {
    rel.get("admin_level")
        .and_then(|v| parse_number(v))
        .map_or(i64::MAX, |v| v.round() as i64)
}

/// Ten meters at the equator in world units, as Boundary.COUNTRY_TEST_OFFSET.
const COUNTRY_TEST_OFFSET: f64 = 10.0 / (2.0 * std::f64::consts::PI * 6_378_137.0);

/// A country border way held back until every OSM source has supplied the
/// country outlines.
pub struct CountryBorder {
    feature: Feature,
    /// Only undisputed admin_level 2 borders get ADM0_L/R.
    adm0: bool,
    /// Relation IDs and ISO3166-1:alpha3 codes, in Java `HashSet` order.
    regions: Vec<(i64, String)>,
    /// Admin level 2 relations whose outlines include this way.
    outlines: Vec<i64>,
    /// The way in world coordinates, without repeated points.
    line: Vec<Coord<f64>>,
}

/// The country borders one OSM source adds to `State`.
#[derive(Default)]
pub struct Contribution {
    borders: Vec<CountryBorder>,
}

impl Contribution {
    pub fn is_empty(&self) -> bool {
        self.borders.is_empty()
    }
}

/// Boundary's country borders and the ways that outline each country.
#[derive(Default)]
pub struct State {
    borders: Vec<CountryBorder>,
}

impl State {
    /// Replays one source's contribution; call in source order.
    pub fn apply(&mut self, contribution: Contribution) {
        self.borders.extend(contribution.borders);
    }

    /// Boundary.finish(): polygonizes each country's border ways, then names
    /// the countries 10 m to the left and right of each border.
    pub fn finish(&mut self, out: &mut Vec<Feature>) {
        let borders = std::mem::take(&mut self.borders);
        let mut outlines: BTreeMap<i64, Vec<&[Coord<f64>]>> = BTreeMap::new();
        for border in &borders {
            for id in &border.outlines {
                outlines.entry(*id).or_default().push(&border.line);
            }
        }
        let needed = borders
            .iter()
            .filter(|b| b.adm0)
            .flat_map(|b| b.regions.iter().map(|(id, _)| *id))
            .collect::<BTreeSet<_>>();
        let countries = needed
            .into_iter()
            .filter_map(|id| {
                let polygon = CountryPolygon::new(outlines.get(&id)?)?;
                Some((id, polygon))
            })
            .collect::<BTreeMap<_, _>>();
        drop(outlines);
        for border in borders {
            let mut f = border.feature;
            if border.adm0 {
                let (left, right) = bordering_regions(&countries, &border.regions, &border.line);
                let code = |id: Option<i64>| {
                    id.and_then(|id| border.regions.iter().find(|(r, _)| *r == id))
                        .map(|(_, code)| code.clone())
                };
                for (key, code) in [("adm0_l", code(left)), ("adm0_r", code(right))] {
                    if let Some(code) = code {
                        f.attrs.insert(key.into(), json!(code));
                        f.attr_minzoom.insert(key.into(), 5);
                    }
                }
            }
            out.push(f);
        }
    }
}

/// GeoUtils.pointAlongOffset: `offset` world units to the right (positive)
/// or left of the middle of the segment at `ratio` along `line`.
fn point_along_offset(line: &[Coord<f64>], ratio: f64, offset: f64) -> Coord<f64> {
    let middle = ((line.len() as f64 * ratio) as usize).min(line.len() - 2);
    let (a, b) = (line[middle], line[middle + 1]);
    let (dx, dy) = (b.x - a.x, b.y - a.y);
    let len = (dx * dx + dy * dy).sqrt();
    let (ux, uy) = (offset * dx / len, offset * dy / len);
    Coord {
        x: a.x + 0.5 * dx - uy,
        y: a.y + 0.5 * dy + ux,
    }
}

/// Boundary.mode: the most frequent ID, ties going to the first in Java
/// `HashMap` order.
fn mode(ids: &[i64]) -> Option<i64> {
    let firsts = grouping_by_long_order(ids);
    let mut best: Option<(i64, usize)> = None;
    for first in firsts {
        let count = ids.iter().filter(|id| **id == ids[first]).count();
        if best.is_none_or(|(_, most)| count > most) {
            best = Some((ids[first], count));
        }
    }
    best.map(|(id, _)| id)
}

/// Boundary.getBorderingRegions: the (left, right) relation IDs of `line`.
fn bordering_regions(
    countries: &BTreeMap<i64, CountryPolygon>,
    regions: &[(i64, String)],
    line: &[Coord<f64>],
) -> (Option<i64>, Option<i64>) {
    let valid = regions
        .iter()
        .filter_map(|(id, _)| countries.get(id).map(|c| (*id, c)))
        .collect::<Vec<_>>();
    let (mut rights, mut lefts) = (Vec::new(), Vec::new());
    for i in 0..10 {
        let ratio = f64::from(i + 1) / 12.0;
        let right = point_along_offset(line, ratio, COUNTRY_TEST_OFFSET);
        let left = point_along_offset(line, ratio, -COUNTRY_TEST_OFFSET);
        for (id, country) in &valid {
            if country.contains(right) {
                rights.push(*id);
            } else if country.contains(left) {
                lefts.push(*id);
            }
        }
    }
    let right = mode(&rights);
    lefts.retain(|id| Some(*id) != right);
    (mode(&lefts), right)
}

/// The union of the polygons JTS's Polygonizer forms from a country's border
/// ways, indexed for point queries like a PreparedGeometry.
///
/// That union is every bounded face of the ways' planar graph, which is the
/// union of the interiors of the faces' outer rings: a face's holes are
/// filled by the faces inside them. Ways are graph edges between their end
/// points, as Polygonizer assumes correctly noded input.
struct CountryPolygon {
    /// Segments of the bounded faces' outer rings, with their ring index.
    segments: Vec<(Coord<f64>, Coord<f64>, u32)>,
    min_y: f64,
    band_height: f64,
    /// Segment indices by horizontal band of `band_height`.
    bands: Vec<Vec<u32>>,
}

impl CountryPolygon {
    fn new(lines: &[&[Coord<f64>]]) -> Option<Self> {
        // Half-edge 2e runs along line e and 2e + 1 against it.
        let key = |c: Coord<f64>| (c.x.to_bits(), c.y.to_bits());
        let mut nodes: BTreeMap<(u64, u64), Vec<usize>> = BTreeMap::new();
        for (e, line) in lines.iter().enumerate() {
            nodes.entry(key(line[0])).or_default().push(2 * e);
            nodes
                .entry(key(line[line.len() - 1]))
                .or_default()
                .push(2 * e + 1);
        }
        let coords = |h: usize| -> Box<dyn Iterator<Item = Coord<f64>> + '_> {
            let line = lines[h / 2];
            if h.is_multiple_of(2) {
                Box::new(line.iter().copied())
            } else {
                Box::new(line.iter().rev().copied())
            }
        };
        let angle = |h: usize| {
            let mut points = coords(h);
            let (a, b) = (points.next().unwrap(), points.next().unwrap());
            (b.y - a.y).atan2(b.x - a.x)
        };
        // Each node's outgoing half-edges counterclockwise by angle.
        let mut position = vec![(0usize, 0usize); 2 * lines.len()];
        let mut outgoing = Vec::with_capacity(nodes.len());
        for (_, mut edges) in nodes {
            edges.sort_by(|a, b| angle(*a).total_cmp(&angle(*b)));
            for (i, h) in edges.iter().enumerate() {
                position[*h] = (outgoing.len(), i);
            }
            outgoing.push(edges);
        }
        // A face continues from u->v along the half-edge clockwise after v->u
        // at v, which keeps the face on the left: bounded faces wind
        // counterclockwise (positive area), the outside of each connected
        // part clockwise.
        let next = |h: usize| {
            let (node, i) = position[h ^ 1];
            let edges = &outgoing[node];
            edges[(i + edges.len() - 1) % edges.len()]
        };
        let mut visited = vec![false; 2 * lines.len()];
        let mut segments = Vec::new();
        let mut rings = 0u32;
        for start in 0..visited.len() {
            if visited[start] {
                continue;
            }
            let mut ring = Vec::new();
            let mut h = start;
            while !visited[h] {
                visited[h] = true;
                let skip = usize::from(!ring.is_empty());
                ring.extend(coords(h).skip(skip));
                h = next(h);
            }
            let area: f64 = ring
                .windows(2)
                .map(|w| w[0].x * w[1].y - w[1].x * w[0].y)
                .sum();
            if h == start && area > 0.0 {
                segments.extend(ring.windows(2).map(|w| (w[0], w[1], rings)));
                rings += 1;
            }
        }
        if segments.is_empty() {
            return None; // Unable to form a closed polygon.
        }
        let (min_y, max_y) = segments
            .iter()
            .flat_map(|(a, b, _)| [a.y, b.y])
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), y| {
                (lo.min(y), hi.max(y))
            });
        let count = (segments.len() / 8).clamp(1, 1 << 16);
        let band_height = (max_y - min_y) / count as f64;
        let band = |y: f64| {
            if band_height > 0.0 {
                (((y - min_y) / band_height) as usize).min(count - 1)
            } else {
                0
            }
        };
        let mut bands = vec![Vec::new(); count];
        for (i, (a, b, _)) in segments.iter().enumerate() {
            for band in &mut bands[band(a.y.min(b.y))..=band(a.y.max(b.y))] {
                band.push(i as u32);
            }
        }
        Some(Self {
            segments,
            min_y,
            band_height,
            bands,
        })
    }

    /// Whether `p` is inside any bounded face, by each ring's winding number.
    fn contains(&self, p: Coord<f64>) -> bool {
        let count = self.bands.len();
        let max_y = self.min_y + self.band_height * count as f64;
        if !(self.min_y..=max_y).contains(&p.y) {
            return false;
        }
        let band = if self.band_height > 0.0 {
            (((p.y - self.min_y) / self.band_height) as usize).min(count - 1)
        } else {
            0
        };
        let mut winding: BTreeMap<u32, i32> = BTreeMap::new();
        for &i in &self.bands[band] {
            let (a, b, ring) = self.segments[i as usize];
            let side = (b.x - a.x) * (p.y - a.y) - (p.x - a.x) * (b.y - a.y);
            if a.y <= p.y {
                if b.y > p.y && side > 0.0 {
                    *winding.entry(ring).or_default() += 1;
                }
            } else if b.y <= p.y && side < 0.0 {
                *winding.entry(ring).or_default() -= 1;
            }
        }
        winding.values().any(|w| *w != 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn named_pier_without_highway_has_no_transportation_name() {
        // Actual way367626534 appeared as an extra name at z14/9099/5553.
        // Pinned TransportationName rejects highway == null even when the
        // shared class mapping accepts man_made=pier for Transportation.
        let source = Source {
            id: 3_676_265_342,
            osm_id: 367_626_534,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: crate::omt::types::Tags::from([
                ("man_made".into(), "pier".into()),
                ("name".into(), "Wypożyczalnia kajaków".into()),
                ("name:en".into(), "Kayak rentals".into()),
            ]),
            geometry: geo_types::LineString::from(vec![(19.0, 50.0), (19.001, 50.001)]).into(),
            relations: vec![],
        };
        let mut output = Vec::new();
        process(&source, &mut output);
        assert!(output.iter().any(|f| f.layer == "transportation"));
        assert!(!output.iter().any(|f| f.layer == "transportation_name"));
    }

    #[test]
    fn actual_indoor_steps_levels_follow_pinned_long_parser() {
        // Exact OSM level tags behind opt40 z14 transportation count deltas.
        // Pinned Planetiler 0.10.2 Parse.parseLongOrNull returns these values;
        // integer-only parsing had collapsed every noninteger case to zero.
        for (osm_id, raw, expected) in [
            (403_552_116, "-1.5", Some(-1)),
            (403_552_206, "-0.5", Some(0)),
            (766_325_863, "0;1", Some(0)),
            (766_325_900, "-1;0", Some(-1)),
            (1_357_545_656, "-2;0", Some(-2)),
            (1_529_356_749, "1;2", Some(1)),
            (0, "bad", None),
            (0, "1.2.3", Some(1)),
            (0, " 1", None),
            (0, "1 ", Some(1)),
            (0, "1,234", Some(1234)),
            (0, "1.2,3", Some(1)),
            (0, "1e3", Some(1)),
            (0, "NaN", Some(0)),
            (0, "+1", Some(1)),
            (0, "-.5", Some(0)),
            (0, "+.5", None),
            (0, "+1.5", None),
            (0, "1E3", Some(1000)),
            (0, "1E+3", Some(1)),
            (0, "1E-3", Some(0)),
            (0, "NaNrest", Some(0)),
        ] {
            let source = Source {
                id: (osm_id as u64) * 10 + 2,
                osm_id,
                kind: SourceKind::Way,
                source_layer: "osm".into(),
                tags: crate::omt::types::Tags::from([
                    ("highway".into(), "steps".into()),
                    ("level".into(), raw.into()),
                ]),
                geometry: geo_types::LineString::from(vec![(19.0, 50.0), (19.001, 50.001)]).into(),
                relations: vec![],
            };
            assert_eq!(level(&source), expected, "OSM way {osm_id}");
            let mut output = Vec::new();
            process(&source, &mut output);
            let transport = output.iter().find(|f| f.layer == "transportation").unwrap();
            assert_eq!(
                transport.attrs.get("level").cloned(),
                expected.map(|v| json!(v))
            );
        }
    }

    #[test]
    fn actual_steps_decimal_layer_matches_pinned_get_long() {
        // way 403552134 at z14/9099/5551 has layer=-1.5. Planetiler emits
        // transportation feature 4035521342 with layer=-1; strict Rust i64
        // parsing had omitted that single attribute occurrence.
        let source = Source {
            id: 4_035_521_342,
            osm_id: 403_552_134,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: crate::omt::types::Tags::from([
                ("highway".into(), "steps".into()),
                ("incline".into(), "down".into()),
                ("indoor".into(), "yes".into()),
                ("layer".into(), "-1.5".into()),
            ]),
            geometry: geo_types::LineString::from(vec![(19.0, 50.0), (19.001, 50.001)]).into(),
            relations: vec![],
        };
        let mut out = Vec::new();
        process(&source, &mut out);
        let transport = out.iter().find(|f| f.layer == "transportation").unwrap();
        assert_eq!(transport.attrs["layer"], json!(-1));
    }

    #[test]
    fn platform_mapping_precedes_service_for_actual_way_232980390() {
        // Pinned OpenMapTilesSchema transportation FieldMappings.Class has the
        // platform path entry before highway=service. This source was an extra
        // native service feature at z13/4536/2783 in the opt40 archive.
        let source = Source {
            id: 2_329_803_902,
            osm_id: 232_980_390,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: crate::omt::types::Tags::from([
                ("highway".into(), "service".into()),
                ("public_transport".into(), "platform".into()),
                ("bus".into(), "yes".into()),
                ("psv".into(), "yes".into()),
                ("access".into(), "no".into()),
                ("oneway".into(), "yes".into()),
                ("surface".into(), "asphalt".into()),
            ]),
            geometry: geo_types::LineString::from(vec![(19.0, 50.0), (19.001, 50.001)]).into(),
            relations: vec![],
        };
        let mut output = Vec::new();
        process(&source, &mut output);
        let road = output.iter().find(|f| f.layer == "transportation").unwrap();
        assert_eq!(road.attrs["class"], json!("path"));
        assert_eq!(road.attrs["subclass"], json!("platform"));
        assert_eq!(road.min_zoom, 14);
        assert_eq!(class("primary", "platform", "", ""), Some("primary".into()));
    }

    #[test]
    fn actual_platform_ways_do_not_inherit_transportation_names() {
        // PBF SHA-256 4c3a6b117c6d1e79f7fbaae4efc8d8565866a3ea1c075488fb04746acec1c971.
        // The pinned TransportationName passes null publicTransport to
        // highwayClass; Transportation itself still accepts the platform.
        for (osm_id, highway, name, a, b) in [
            (
                207_415_030_i64,
                "bus_stop",
                "Jawiszowice Osiedle",
                (19.1337596, 49.9723097),
                (19.1341584, 49.9723111),
            ),
            (
                254_802_615,
                "platform",
                "Zasole I - ulica Więźniów Oświęcimia",
                (19.2010479, 50.0355875),
                (19.2011614, 50.0353973),
            ),
        ] {
            let source = Source {
                id: (osm_id as u64) * 10 + 2,
                osm_id,
                kind: SourceKind::Way,
                source_layer: "osm".into(),
                tags: crate::omt::types::Tags::from([
                    ("highway".into(), highway.into()),
                    ("name".into(), name.into()),
                    ("public_transport".into(), "platform".into()),
                ]),
                geometry: geo_types::LineString::from(vec![a, b]).into(),
                relations: vec![],
            };
            let mut features = Vec::new();
            process(&source, &mut features);
            assert!(features.iter().any(|f| f.layer == "transportation"));
            assert!(!features.iter().any(|f| f.layer == "transportation_name"));
        }
    }

    #[test]
    fn duplicate_route_display_fields_do_not_split_name_merges() {
        // On actual z14/9120/5576, two native road-name features differed
        // only by a duplicated pl:national/28 route_2, while pinned Java
        // merged them into one feature with route_1 only.
        let route = |id: &str| {
            Tags::from([
                ("_id".into(), id.into()),
                ("type".into(), "route".into()),
                ("route".into(), "road".into()),
                ("network".into(), "pl:national".into()),
                ("ref".into(), "28".into()),
                ("name".into(), "Droga krajowa nr 28".into()),
            ])
        };
        let source = Source {
            id: 1_668_922_672,
            osm_id: 166_892_267,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: crate::omt::types::Tags::from([
                ("highway".into(), "primary".into()),
                ("name".into(), "Józefa Piłsudskiego".into()),
                ("ref".into(), "28".into()),
            ]),
            geometry: geo_types::LineString::from(vec![(20.0, 49.5), (20.001, 49.5)]).into(),
            relations: vec![route("101"), route("102")],
        };
        let mut features = Vec::new();
        process(&source, &mut features);
        let name = features
            .iter()
            .find(|feature| feature.layer == "transportation_name")
            .unwrap();
        assert_eq!(name.attrs["route_1_ref"], json!("28"));
        assert!(!name.attrs.contains_key("route_2_ref"));
    }

    #[test]
    fn route_refs_order_by_java_string_length() {
        // Pomorskie 14/8964/5260 way 268775422: Java counts "SŁ-162s" as 7
        // UTF-16 units, so it sorts before "PM-1521y" (8); in UTF-8 bytes
        // both are 8 and "PM" would come first.
        let route = |id: &str, reference: &str, name: &str| {
            Tags::from([
                ("_id".into(), id.into()),
                ("type".into(), "route".into()),
                ("route".into(), "hiking".into()),
                ("osmc:symbol".into(), "yellow:white:yellow_bar".into()),
                ("ref".into(), reference.into()),
                ("name".into(), name.into()),
            ])
        };
        let source = Source {
            id: 2_687_754_222,
            osm_id: 268_775_422,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: crate::omt::types::Tags::from([
                ("highway".into(), "path".into()),
                ("name".into(), "Ścieżka".into()),
            ]),
            geometry: geo_types::LineString::from(vec![(17.0, 54.0), (17.001, 54.0)]).into(),
            relations: vec![
                route("1", "PM-1521y", "Szlak Doliny Brdy"),
                route("2", "SŁ-162s", "Szlak Doliny Wieprzy i Studnicy"),
            ],
        };
        let mut features = Vec::new();
        process(&source, &mut features);
        let name = features
            .iter()
            .find(|feature| feature.layer == "transportation_name")
            .unwrap();
        assert_eq!(name.attrs["route_1_ref"], json!("SŁ-162s"));
        assert_eq!(name.attrs["route_2_ref"], json!("PM-1521y"));
    }

    #[test]
    fn actual_trunk_way_212664723_uses_pinned_z5_upgrade_units() {
        // PBF SHA-256 4c3a6b117c6d1e79f7fbaae4efc8d8565866a3ea1c075488fb04746acec1c971.
        // This E77 trunk segment is about 736 m: above 500 m, but below the
        // pinned zoom-zero pixel threshold that is compared to world length.
        let source = Source {
            id: 2_126_647_232,
            osm_id: 212_664_723,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: crate::omt::types::Tags::from([
                ("highway".into(), "trunk".into()),
                ("ref".into(), "7".into()),
            ]),
            geometry: geo_types::LineString::from(vec![
                (19.6930528, 49.475238),
                (19.6930734, 49.4753662),
                (19.6930841, 49.4754289),
                (19.6932883, 49.4766471),
                (19.6933858, 49.4772287),
                (19.6934434, 49.4775726),
                (19.6934506, 49.4776164),
                (19.6936289, 49.4787073),
                (19.6936663, 49.4789428),
                (19.6937244, 49.4792564),
                (19.6937763, 49.4795105),
            ])
            .into(),
            relations: vec![crate::omt::types::Tags::from([
                ("route".into(), "road".into()),
                ("network".into(), "e-road".into()),
                ("ref".into(), "E 77".into()),
            ])],
        };
        assert!(world_length(&source) > 500.0 / 40_075_016.685_578_49);
        assert!(world_length(&source) < TRUNK_Z0_UPGRADE_LENGTH);
        let mut output = Vec::new();
        process(&source, &mut output);
        let roads = output
            .iter()
            .filter(|f| f.layer == "transportation")
            .collect::<Vec<_>>();
        assert!(
            roads.iter().any(|f| f.min_zoom == 5
                && f.max_zoom == 5
                && f.attrs["class"] == json!("motorway"))
        );
        assert!(
            roads
                .iter()
                .any(|f| f.min_zoom == 6 && f.attrs["class"] == json!("trunk"))
        );
    }
    #[test]
    fn country_boundary_finish_uses_simple_feature_id_only_when_deferred() {
        let relation = |level: &str, code: Option<&str>| {
            let mut tags = Tags::from([
                ("type".into(), "boundary".into()),
                ("boundary".into(), "administrative".into()),
                ("admin_level".into(), level.into()),
            ]);
            if let Some(code) = code {
                tags.insert("ISO3166-1:alpha3".into(), code.into());
            }
            tags
        };
        let mut source = Source {
            id: 1_392_250_432,
            osm_id: 139_225_043,
            kind: SourceKind::Way,
            source_layer: String::new(),
            tags: crate::omt::types::Tags::new(),
            geometry: geo_types::LineString::from(vec![(19., 50.), (20., 50.)]).into(),
            relations: vec![relation("2", Some("POL"))],
        };
        let mut features = Vec::new();
        process(&source, &mut features);
        assert_eq!(features[0].id, 1_392_250_430);

        source.relations = vec![relation("2", None)];
        features.clear();
        process(&source, &mut features);
        assert_eq!(features[0].id, source.id);

        // Match the Java loop's running minimum, including relation order.
        source.relations = vec![relation("4", Some("POL")), relation("2", None)];
        features.clear();
        process(&source, &mut features);
        assert_eq!(features[0].id, source.id);
        source.relations.reverse();
        features.clear();
        process(&source, &mut features);
        assert_eq!(features[0].id, 1_392_250_430);
    }
    fn country(id: i64, code: &str) -> Tags {
        Tags::from([
            ("_id".into(), id.to_string()),
            ("type".into(), "boundary".into()),
            ("boundary".into(), "administrative".into()),
            ("admin_level".into(), "2".into()),
            ("ISO3166-1:alpha3".into(), code.into()),
        ])
    }

    #[test]
    fn country_border_finish_names_left_and_right_countries() {
        let (a, b) = (country(1, "AAA"), country(2, "BBB"));
        let way = |id: u64, points: Vec<(f64, f64)>, relations: Vec<Tags>| Source {
            id: id * 10 + 2,
            osm_id: id as i64,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: crate::omt::types::Tags::new(),
            geometry: geo_types::LineString::from(points).into(),
            relations,
        };
        // AAA is west of lon 20 and BBB east of it; the shared way runs north.
        let ways = [
            way(1, vec![(20., 49.), (20., 50.)], vec![a.clone(), b.clone()]),
            way(
                2,
                vec![(20., 50.), (19., 50.), (19., 49.), (20., 49.)],
                vec![a],
            ),
            way(
                3,
                vec![(20., 49.), (21., 49.), (21., 50.), (20., 50.)],
                vec![b],
            ),
        ];
        let mut state = State::default();
        let mut out = Vec::new();
        for source in &ways {
            let mut contribution = Contribution::default();
            evaluate(source, &mut out, &mut contribution);
            state.apply(contribution);
        }
        assert!(out.is_empty());
        state.finish(&mut out);
        assert_eq!(out.len(), 3);
        let shared = &out[0];
        assert_eq!(shared.id, 10);
        assert_eq!(shared.attrs["adm0_l"], json!("AAA"));
        assert_eq!(shared.attrs["adm0_r"], json!("BBB"));
        assert_eq!(shared.attr_minzoom["adm0_l"], 5);
        assert_eq!(shared.attr_minzoom["adm0_r"], 5);
        // Each outer way only borders its own country, on its left.
        assert_eq!(out[1].attrs["adm0_l"], json!("AAA"));
        assert!(!out[1].attrs.contains_key("adm0_r"));
        assert_eq!(out[2].attrs["adm0_l"], json!("BBB"));

        // Disputed borders keep no country names.
        let mut state = State::default();
        let mut disputed = ways[0].clone();
        disputed.tags.insert("disputed".into(), "yes".into());
        disputed
            .tags
            .insert("name".into(), "Extent of a claim at X".into());
        for source in [&disputed, &ways[1], &ways[2]] {
            let mut contribution = Contribution::default();
            evaluate(source, &mut out, &mut contribution);
            state.apply(contribution);
        }
        out.clear();
        state.finish(&mut out);
        assert!(!out[0].attrs.contains_key("adm0_l"));
        assert_eq!(out[0].attrs["disputed"], json!(1));
        assert_eq!(out[0].attrs["disputed_name"], json!("aclaimX"));
    }

    #[test]
    fn country_polygon_is_the_union_of_bounded_faces() {
        let c = |x: f64, y: f64| Coord { x, y };
        let outer = [c(0., 0.), c(10., 0.), c(10., 10.), c(0., 10.), c(0., 0.)];
        // An enclave ring of its own, as for a country around another, and a
        // dangling way that bounds nothing.
        let enclave = [c(4., 4.), c(6., 4.), c(6., 6.), c(4., 6.), c(4., 4.)];
        let dangle = [c(10., 10.), c(12., 12.)];
        let polygon = CountryPolygon::new(&[&outer, &enclave, &dangle]).unwrap();
        assert!(polygon.contains(c(1., 1.)));
        assert!(polygon.contains(c(5., 5.)));
        assert!(!polygon.contains(c(11., 11.5)));
        assert!(!polygon.contains(c(-1., 5.)));
        // Two ways that meet at both ends close a ring; one alone does not.
        let north = [c(0., 0.), c(5., 5.), c(10., 0.)];
        let south = [c(10., 0.), c(5., -5.), c(0., 0.)];
        assert!(CountryPolygon::new(&[&north]).is_none());
        let polygon = CountryPolygon::new(&[&north, &south]).unwrap();
        assert!(polygon.contains(c(5., 0.)));
    }

    #[test]
    fn bridge_outline_is_polygon_only_unless_another_line_mapping_matches() {
        // Actual way 753058771 has only man_made=bridge; its closed line
        // duplicate must not accompany the polygon emitted by ingest.
        let mut source = Source {
            id: 7530587712,
            osm_id: 753058771,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: crate::omt::types::Tags::from([("man_made".into(), "bridge".into())]),
            geometry: geo_types::LineString::from(vec![
                (19., 50.),
                (19.001, 50.),
                (19.001, 50.001),
                (19., 50.),
            ])
            .into(),
            relations: vec![],
        };
        let mut output = Vec::new();
        process(&source, &mut output);
        assert!(output.is_empty());
        source.tags.insert("service".into(), "driveway".into());
        process(&source, &mut output);
        assert!(output.iter().any(|f| f.layer == "transportation"));
        output.clear();
        source.tags.remove("service");
        source.tags.insert("man_made".into(), "pier".into());
        process(&source, &mut output);
        assert!(output.iter().any(|f| f.layer == "transportation"));
    }
    #[test]
    fn primary_road_emits_real_transport_and_name_attributes() {
        let source = Source {
            id: 11,
            osm_id: 1,
            kind: SourceKind::Way,
            source_layer: String::new(),
            tags: crate::omt::types::Tags::from([
                ("highway".into(), "primary".into()),
                ("name".into(), "Main".into()),
                ("bridge".into(), "yes".into()),
                ("oneway".into(), "-1".into()),
            ]),
            geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![
                (19.0, 50.0),
                (19.1, 50.1),
            ])),
            relations: vec![],
        };
        let mut out = vec![];
        process(&source, &mut out);
        let road = out.iter().find(|f| f.layer == "transportation").unwrap();
        assert_eq!(road.min_zoom, 7);
        assert_eq!(road.attrs["class"], json!("primary"));
        assert_eq!(road.attrs["brunnel"], json!("bridge"));
        assert_eq!(road.attrs["oneway"], json!(-1));
        assert_eq!(road.attr_minzoom["oneway"], 12);
        let label = out
            .iter()
            .find(|f| f.layer == "transportation_name")
            .unwrap();
        assert_eq!(label.min_zoom, 12);
        assert_eq!(label.attrs["name"], json!("Main"));
        assert!(!label.attrs.contains_key("brunnel"));
    }
    #[test]
    fn zorder_matches_imposm_link_railway_boolean_and_invalid_layer_rules() {
        let mut source = Source {
            id: 12,
            osm_id: 1,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: crate::omt::types::Tags::new(),
            geometry: geo_types::Geometry::LineString(geo_types::LineString::from(vec![
                (0.0, 0.0),
                (1.0, 1.0),
            ])),
            relations: vec![],
        };
        source.tags.insert("highway".into(), "motorway_link".into());
        assert_eq!(zorder(&source), 3);
        source.tags.insert("bridge".into(), "viaduct".into());
        assert_eq!(zorder(&source), 13);
        source.tags.remove("highway");
        source.tags.insert("railway".into(), "rail".into());
        assert_eq!(zorder(&source), 17);
        source.tags.insert("layer".into(), "1000".into());
        assert_eq!(zorder(&source), 0);
        // Parse.parseLong reads the numeric prefix: 1.5 is layer 1.
        source.tags.insert("layer".into(), "1.5".into());
        assert_eq!(zorder(&source), 27);
        source.tags.insert("layer".into(), "-1;0".into());
        assert_eq!(zorder(&source), 7);
        assert!(!yes("no"));
        assert!(yes("building_passage"));
    }
    fn area(id: u64, tags: &[(&str, &str)]) -> Source {
        Source {
            id: id * 10 + 1,
            osm_id: id as i64,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            geometry: geo_types::Polygon::new(
                geo_types::LineString::from(vec![
                    (18.0, 54.0),
                    (18.001, 54.0),
                    (18.001, 54.001),
                    (18.0, 54.0),
                ]),
                vec![],
            )
            .into(),
            relations: vec![],
        }
    }
    #[test]
    fn footway_area_reads_layer_prefix_like_planetiler() {
        // Way 1052128904 in Pomorskie (13/4520/2616): Planetiler reads
        // layer=-1;0 as -1 and drops the underground area.
        let mut out = Vec::new();
        process(
            &area(
                1_052_128_904,
                &[("highway", "footway"), ("area", "yes"), ("layer", "-1;0")],
            ),
            &mut out,
        );
        assert!(out.iter().all(|f| f.layer != "transportation"), "{out:?}");
        process(
            &area(
                1,
                &[("highway", "footway"), ("area", "yes"), ("layer", "0;1")],
            ),
            &mut out,
        );
        assert_eq!(
            out.iter().filter(|f| f.layer == "transportation").count(),
            1
        );
    }
    #[test]
    fn man_made_bridge_class_needs_empty_public_transport() {
        // Way 405475171 in Berlin: man_made=bridge with
        // public_transport=platform_access has no class in Planetiler.
        let mut out = Vec::new();
        process(
            &area(
                405_475_171,
                &[
                    ("man_made", "bridge"),
                    ("public_transport", "platform_access"),
                ],
            ),
            &mut out,
        );
        assert!(out.iter().all(|f| f.layer != "transportation"), "{out:?}");
        assert_eq!(class("", "platform_access", "", "bridge"), None);
        assert_eq!(class("", "", "", "bridge").as_deref(), Some("bridge"));
    }
    #[test]
    fn motorway_junction_has_layer_and_descending_sort_key() {
        // Berlin motorway_junction nodes tagged layer=2 or -1 carry layer in
        // Planetiler's transportation_name.
        let source = Source {
            id: 1,
            osm_id: 1,
            kind: SourceKind::Node,
            source_layer: "osm".into(),
            tags: crate::omt::types::Tags::from([
                ("highway".into(), "motorway_junction".into()),
                ("_junction_highway".into(), "motorway".into()),
                ("ref".into(), "5".into()),
                ("layer".into(), "2".into()),
            ]),
            geometry: geo_types::Point::new(13.4, 52.5).into(),
            relations: vec![],
        };
        let mut out = Vec::new();
        process(&source, &mut out);
        let f = out
            .iter()
            .find(|f| f.layer == "transportation_name")
            .unwrap();
        assert_eq!(f.attrs["layer"], json!(2));
        assert!(!f.attr_minzoom.contains_key("layer"));
        assert_eq!(f.sort_key, -21);
    }
    #[test]
    fn construction_and_negative_highways_are_not_active_minor_roads() {
        assert_eq!(
            class("construction", "", "primary", "").as_deref(),
            Some("primary_construction")
        );
        assert_eq!(class("proposed", "", "", ""), None);
        assert_eq!(class("footway", "", "", "").as_deref(), Some("path"));
    }
    #[test]
    fn canadian_network_rules_preserve_ref_exceptions() {
        assert_eq!(
            network("CA:ON:primary", "401"),
            Some(("ca-provincial-arterial", 4))
        );
        assert_eq!(network("CA:ON:primary", "7"), Some(("ca-provincial", 5)));
        assert_eq!(network("US:I", "80"), Some(("us-interstate", 0)));
    }
}
