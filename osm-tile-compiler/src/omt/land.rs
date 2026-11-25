//! OMT 3.16 source policy port from planetiler-openmaptiles 91516fdf.
//! Per-tile polygon merging remains the renderer's responsibility (including
//! landcover vertex-count-dependent grouping and z13 nearby building union).
use super::types::{Feature, Source, TOP, java_meters, parse_number, parse_profile_double};
use serde_json::json;

fn listed(value: &str, values: &str) -> bool {
    !value.is_empty() && values.split_whitespace().any(|v| v == value)
}
fn first<'a>(source: &'a Source, keys: &[&str]) -> &'a str {
    keys.iter()
        .map(|key| source.tag(key))
        .find(|v| !v.is_empty())
        .unwrap_or("")
}
fn material_color(value: &str) -> Option<&'static str> {
    Some(match value {
        "cement_block" => "#6a7880",
        "brick" | "traditional" | "masonry" | "Brick" => "#bd8161",
        "plaster" => "#dadbdb",
        "wood" => "#d48741",
        "concrete" => "#d3c2b0",
        "metal" | "steel" | "tin" => "#b7b1a6",
        "stone" | "sandstone" => "#b4a995",
        "mud" | "clay" => "#9d8b75",
        "glass" => "#5a81a0",
        "timber_framing" => "#b3b0a9",
        _ => return None,
    })
}
fn cover_class(value: &str) -> Option<&'static str> {
    Some(
        if listed(value, "farmland farm orchard vineyard plant_nursery") {
            "farmland"
        } else if listed(value, "glacier ice_shelf") {
            "ice"
        } else if listed(value, "wood forest") {
            "wood"
        } else if listed(value, "bare_rock scree") {
            "rock"
        } else if listed(
            value,
            "fell flowerbed grassland heath scrub shrubbery tundra grass meadow allotments park village_green recreation_ground garden golf_course",
        ) {
            "grass"
        } else if listed(
            value,
            "wetland bog swamp wet_meadow marsh reedbed saltern tidalflat saltmarsh mangrove",
        ) {
            "wetland"
        } else if listed(value, "beach sand dune") {
            "sand"
        } else {
            return None;
        },
    )
}

pub fn process(source: &Source, out: &mut Vec<Feature>) {
    if !source.is_polygon() {
        return;
    }
    match source.source_layer.as_str() {
        "ne_50m_urban_areas" => {
            let min = if parse_number(source.tag("scalerank")).is_some_and(|v| v <= 2.0) {
                4
            } else {
                5
            };
            let mut f = Feature::new(source, "landuse", source.geometry.clone(), min, 5);
            f.buffer_pixels = 4.0;
            f.attrs.insert("class".into(), json!("residential"));
            out.push(f);
            return;
        }
        "ne_110m_glaciated_areas"
        | "ne_50m_glaciated_areas"
        | "ne_10m_glaciated_areas"
        | "ne_50m_antarctic_ice_shelves_polys"
        | "ne_10m_antarctic_ice_shelves_polys" => {
            let (min, max) = if source.source_layer.contains("110m") {
                (0, 1)
            } else if source.source_layer.contains("50m") {
                (2, 4)
            } else {
                (5, 6)
            };
            let mut f = Feature::new(source, "landcover", source.geometry.clone(), min, max);
            f.buffer_pixels = 4.0;
            f.attrs.insert("class".into(), json!("ice"));
            f.attrs.insert(
                "subclass".into(),
                json!(if source.source_layer.contains("shelves") {
                    "ice_shelf"
                } else {
                    "glacier"
                }),
            );
            out.push(f);
            return;
        }
        _ => {}
    }
    if source.source_layer.starts_with("ne_") {
        return;
    }
    if (source.has("building")
        || source.has("building:part")
        || listed(source.tag("aeroway"), "terminal hangar"))
        && !listed(source.tag("building"), "no none No")
        && !listed(source.tag("building:part"), "no none No")
        && source.tag("man_made") != "bridge"
        && source.tag("location") != "underground"
    {
        // Building.process: heights go through Parse.meters and levels
        // through Parse.parseDoubleOrNull, coalescing the parsed values; then
        // `(int) Math.ceil` and `(int) Math.floor`, which `as i32` matches
        // (saturating, NaN to 0).
        let levels = |keys: [&str; 2]| {
            keys.iter()
                .find_map(|key| parse_profile_double(source.tag(key)))
        };
        let height = java_meters(first(source, &["height", "building:height"]))
            .unwrap_or_else(|| levels(["levels", "building:levels"]).map_or(5.0, |v| v * 3.66))
            .ceil() as i32;
        let min_height = java_meters(first(source, &["min_height", "building:min_height"]))
            .unwrap_or_else(|| {
                levels(["min_level", "building:min_level"]).map_or(0.0, |v| v * 3.66)
            })
            .floor() as i32;
        if height < 3660 && min_height < 3660 {
            let mut f = Feature::new_original_geometry(
                source,
                "building",
                source.geometry.clone(),
                13,
                TOP,
            );
            f.buffer_pixels = 4.0;
            f.min_size = 0.1;
            f.tolerance = 0.25;
            f.attrs.insert("render_height".into(), json!(height));
            f.attrs
                .insert("render_min_height".into(), json!(min_height));
            for key in ["render_height", "render_min_height"] {
                f.attr_minzoom.insert(key.into(), 14);
            }
            let color = if source.has("building:colour") {
                Some(source.tag("building:colour"))
            } else {
                material_color(source.tag("building:material"))
            };
            if let Some(color) = color {
                f.attrs.insert("colour".into(), json!(color.to_lowercase()));
                f.attr_minzoom.insert("colour".into(), 14);
            }
            if source.relations.iter().any(|r| {
                r.get("type").is_some_and(|v| v == "building")
                    && r.get("_role").is_some_and(|v| v == "outline")
            }) {
                f.attrs.insert("hide_3d".into(), json!(true));
                f.attr_minzoom.insert("hide_3d".into(), 14);
            }
            f.sort_key = i64::from(height);
            out.push(f);
        }
    }
    let cover_keys = [
        (
            "landuse",
            "allotments farm farmland orchard flowerbed plant_nursery vineyard grass grassland meadow forest village_green recreation_ground",
        ),
        (
            "natural",
            "wood wetland fell grassland heath scrub shrubbery tundra glacier bare_rock scree beach sand dune",
        ),
        ("leisure", "park garden golf_course"),
        (
            "wetland",
            "bog swamp wet_meadow marsh reedbed saltern tidalflat saltmarsh mangrove",
        ),
    ];
    if let Some(subclass) = cover_keys
        .iter()
        .find_map(|(key, values)| listed(source.tag(key), values).then_some(source.tag(key)))
        && let Some(class) = cover_class(subclass)
    {
        // Above z13 the thresholds end and Planetiler's defaults apply: 1 pixel
        // and 0.1 tolerance below the archive's top, 1/16 at it.
        for (min, max, size, tolerance, at_max) in [
            (7, 9, 2.0, 0.25, None),
            (10, 10, 4.0, 0.25, None),
            (11, 13, 8.0, 0.1, None),
            (14, TOP, 1.0, 0.1, Some(0.0625)),
        ] {
            let mut f = Feature::new_original_geometry(
                source,
                "landcover",
                source.geometry.clone(),
                min,
                max,
            );
            f.buffer_pixels = 4.0;
            f.min_size = size;
            f.min_size_at_max_zoom = at_max.unwrap_or(size);
            f.tolerance = tolerance;
            f.tolerance_at_max_zoom = at_max.unwrap_or(tolerance);
            f.attrs.insert("class".into(), json!(class));
            f.attrs.insert("subclass".into(), json!(subclass));
            // Renderer replaces this marker with the zoom-simplified,
            // untiled vertex count, as FeatureRenderer.setNumPointsAttr does.
            // The tile postprocessor consumes it before writing MVT attrs.
            f.attrs.insert("_numpoints".into(), json!(0));
            out.push(f);
        }
    }
    let use_keys = [
        (
            "landuse",
            "railway cemetery military quarry residential commercial industrial garages retail",
        ),
        (
            "amenity",
            "bus_station school university kindergarten college library hospital grave_yard",
        ),
        ("leisure", "stadium pitch playground track"),
        ("tourism", "theme_park zoo"),
        ("place", "suburb quarter neighbourhood"),
        ("waterway", "dam"),
    ];
    if use_keys
        .iter()
        .any(|(key, values)| listed(source.tag(key), values))
    {
        let class = first(
            source,
            &[
                "landuse", "amenity", "leisure", "tourism", "place", "waterway",
            ],
        );
        let class = if class == "grave_yard" {
            "cemetery"
        } else {
            class
        };
        let min = if listed(class, "residential suburb quarter neighbourhood") {
            6
        } else {
            9
        };
        for (lo, hi, size) in [(6, 6, 1.0), (7, 7, 2.0), (8, 13, 4.0), (14, TOP, 1.0)] {
            if hi < min {
                continue;
            }
            let mut f = Feature::new_original_geometry(
                source,
                "landuse",
                source.geometry.clone(),
                lo.max(min),
                hi,
            );
            f.buffer_pixels = 4.0;
            f.min_size = if class == "residential" { 0.1 } else { size };
            f.min_size_at_max_zoom = 0.0625;
            if class == "residential" {
                f.tolerance = 0.25;
            }
            f.attrs.insert("class".into(), json!(class));
            out.push(f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn building_filter_height_and_zoom_gates_match_source_policy() {
        let mut source = Source {
            id: 11,
            osm_id: 1,
            kind: super::super::types::SourceKind::Way,
            source_layer: String::new(),
            tags: crate::omt::types::Tags::from([
                ("building".into(), "yes".into()),
                ("building:levels".into(), "3".into()),
                ("building:material".into(), "brick".into()),
            ]),
            geometry: geo_types::Geometry::Polygon(geo_types::Polygon::new(
                geo_types::LineString::from(vec![
                    (19.0, 50.0),
                    (19.1, 50.0),
                    (19.1, 50.1),
                    (19.0, 50.0),
                ]),
                vec![],
            )),
            relations: vec![],
        };
        let mut out = vec![];
        process(&source, &mut out);
        let building = out.iter().find(|f| f.layer == "building").unwrap();
        assert_eq!(building.min_zoom, 13);
        assert_eq!(building.attrs["render_height"], json!(11));
        assert_eq!(building.attr_minzoom["render_height"], 14);
        assert_eq!(building.attrs["colour"], json!("#bd8161"));
        source.tags.insert("building".into(), "no".into());
        out.clear();
        process(&source, &mut out);
        assert!(out.is_empty());
    }
    #[test]
    fn material_and_landcover_mappings_match_profile() {
        assert_eq!(material_color("brick"), Some("#bd8161"));
        assert_eq!(cover_class("allotments"), Some("grass"));
        assert_eq!(cover_class("plant_nursery"), Some("farmland"));
        assert_eq!(cover_class("residential"), None);
    }
    #[test]
    fn osm_landcover_requests_zoom_specific_vertex_count_but_supplements_do_not() {
        let mut source = Source {
            id: 12,
            osm_id: 1,
            kind: super::super::types::SourceKind::Way,
            source_layer: String::new(),
            tags: crate::omt::types::Tags::from([("natural".into(), "wood".into())]),
            geometry: geo_types::Geometry::Polygon(geo_types::Polygon::new(
                geo_types::LineString::from(vec![(0., 0.), (1., 0.), (1., 1.), (0., 0.)]),
                vec![],
            )),
            relations: vec![],
        };
        let mut out = vec![];
        process(&source, &mut out);
        assert_eq!(out.len(), 4);
        assert!(
            out.iter()
                .all(|f| f.attrs.get("_numpoints") == Some(&json!(0)))
        );
        source.source_layer = "ne_10m_glaciated_areas".into();
        out.clear();
        process(&source, &mut out);
        assert_eq!(out.len(), 1);
        assert!(!out[0].attrs.contains_key("_numpoints"));
    }
    #[test]
    fn building_heights_parse_like_planetiler() {
        let heights = |tags: &[(&str, &str)]| {
            let mut tags = tags
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect::<std::collections::BTreeMap<_, _>>();
            tags.insert("building".into(), "yes".into());
            let source = Source {
                id: 11,
                osm_id: 1,
                kind: super::super::types::SourceKind::Way,
                source_layer: String::new(),
                tags: tags.into(),
                geometry: geo_types::Geometry::Polygon(geo_types::Polygon::new(
                    geo_types::LineString::from(vec![
                        (19.0, 50.0),
                        (19.1, 50.0),
                        (19.1, 50.1),
                        (19.0, 50.0),
                    ]),
                    vec![],
                )),
                relations: vec![],
            };
            let mut out = vec![];
            process(&source, &mut out);
            let building = out.iter().find(|f| f.layer == "building").unwrap();
            (
                building.attrs["render_height"].as_i64().unwrap(),
                building.attrs["render_min_height"].as_i64().unwrap(),
                building.sort_key,
            )
        };
        // Parse.meters finds the number after a decimal comma (ways
        // 232083069 and 231816262 in the Malopolskie extract).
        assert_eq!(heights(&[("height", "2,5")]), (5, 0, 5));
        assert_eq!(
            heights(&[("height", "3,5"), ("building:levels", "1")]),
            (5, 0, 5)
        );
        assert_eq!(heights(&[("height", "10 ft")]), (4, 0, 4));
        assert_eq!(
            heights(&[("height", "12m"), ("min_height", "2.5")]),
            (12, 2, 12)
        );
        // Parse.parseDoubleOrNull reads a decimal comma as a grouping
        // separator (way 769281255), and the parsed level values are
        // coalesced, not the tag strings.
        assert_eq!(heights(&[("building:levels", "0,5")]), (19, 0, 19));
        assert_eq!(
            heights(&[("levels", "x"), ("building:levels", "2")]),
            (8, 0, 8)
        );
        assert_eq!(heights(&[("building:min_level", "1d")]), (5, 3, 5));
        // (int) Math.ceil(NaN) is 0.
        assert_eq!(heights(&[("building:levels", "NaN")]), (0, 0, 0));
    }
}
