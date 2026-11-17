//! Water, waterway and water_name policy translated from planetiler-openmaptiles
//! commit 91516fdf477915b1a985015811049b9b96c7f87e (BSD-3-Clause).
//! See the upstream license and policy attribution in the OMT module.

use super::types::{Feature, Source, SourceKind, add_names};
use geo::{Area, BooleanOps, BoundingRect, Distance, Euclidean, MapCoords};
use geo_types::{Coord, Geometry, LineString, MultiLineString, MultiPolygon, Rect};
use serde_json::json;
use std::collections::BTreeMap;

const WORLD_METERS: f64 = 40_075_016.685_578_49;

#[derive(Default)]
pub struct State {
    lakes: Vec<Lake>,
    centerlines: BTreeMap<i64, MultiLineString<f64>>,
    marine_regions: BTreeMap<String, (Geometry<f64>, i32)>,
    river_lengths: BTreeMap<i64, f64>,
    river_members: Vec<(i64, Feature)>,
}

/// What one OSM source adds to `State`. The supplement-loaded data is read-only
/// while OSM sources are evaluated concurrently; everything accumulated across
/// OSM sources is replayed in source order by `State::apply`.
#[derive(Default)]
pub struct Contribution {
    /// River relation memberships: id, line length, member feature.
    rivers: Vec<(i64, f64, Feature)>,
    /// Natural Earth lake index, intersection area and OSM id candidates.
    lakes: Vec<(usize, f64, i64)>,
}

impl Contribution {
    pub fn is_empty(&self) -> bool {
        self.rivers.is_empty() && self.lakes.is_empty()
    }
}

struct Lake {
    source: Source,
    world: MultiPolygon<f64>,
    bounds: Option<Rect<f64>>,
    min_zoom: u8,
    max_zoom: u8,
    osm_id: Option<i64>,
    intersection_area: f64,
}

fn world(geometry: &Geometry<f64>) -> Geometry<f64> {
    geometry.map_coords(|c| {
        let lat =
            c.y.clamp(-85.051_128_779_806_6, 85.051_128_779_806_6)
                .to_radians();
        Coord {
            x: (c.x + 180.) / 360.,
            y: (1. - (lat.tan() + 1. / lat.cos()).ln() / std::f64::consts::PI) / 2.,
        }
    })
}

fn polygons(geometry: &Geometry<f64>) -> MultiPolygon<f64> {
    match geometry {
        Geometry::Polygon(p) => MultiPolygon(vec![p.clone()]),
        Geometry::MultiPolygon(p) => p.clone(),
        _ => MultiPolygon(vec![]),
    }
}

fn lines(geometry: &Geometry<f64>) -> Vec<LineString<f64>> {
    match geometry {
        Geometry::LineString(l) => vec![l.clone()],
        Geometry::MultiLineString(l) => l.0.clone(),
        Geometry::GeometryCollection(c) => c.0.iter().flat_map(lines).collect(),
        _ => vec![],
    }
}

fn world_length(geometry: &Geometry<f64>) -> f64 {
    lines(&world(geometry))
        .iter()
        .flat_map(|l| l.lines())
        .map(|line| (line.dx().powi(2) + line.dy().powi(2)).sqrt())
        .sum()
}

fn truth(value: &str) -> bool {
    !matches!(value, "" | "false" | "no" | "0")
}

fn brunnel(source: &Source, feature: &mut Feature) {
    let value = if truth(source.tag("bridge")) {
        "bridge"
    } else if truth(source.tag("tunnel")) {
        "tunnel"
    } else {
        return;
    };
    feature.attrs.insert("brunnel".into(), json!(value));
    feature.attr_minzoom.insert("brunnel".into(), 12);
}

fn water_polygon(source: &Source) -> bool {
    source.is_polygon()
        && source.tag("covered") != "yes"
        && (matches!(source.tag("landuse"), "reservoir" | "basin" | "salt_pond")
            || source.tag("leisure") == "swimming_pool"
            || matches!(source.tag("natural"), "water" | "bay" | "spring")
            || source.tag("waterway") == "dock"
            || matches!(
                source.tag("water"),
                "river" | "stream" | "canal" | "ditch" | "drain" | "pond" | "basin" | "wastewater"
            ))
}

fn water_class(source: &Source) -> &'static str {
    if source.tag("waterway") == "dock" {
        "dock"
    } else if matches!(
        source.tag("water"),
        "river" | "stream" | "canal" | "ditch" | "drain"
    ) {
        "river"
    } else if matches!(
        source.tag("water"),
        "pond" | "basin" | "wastewater" | "salt_pond"
    ) {
        "pond"
    } else if source.tag("leisure") == "swimming_pool" {
        "swimming_pool"
    } else {
        "lake"
    }
}

fn make_water(source: &Source, class: &str, min: u8, max: u8) -> Feature {
    let mut f = Feature::new_original_geometry(source, "water", source.geometry.clone(), min, max);
    f.attrs.insert("class".into(), json!(class));
    f
}

/// Equatorial threshold converted to the 256-screen-pixel coordinate system.
pub fn merge_min_length_pixels(zoom: u8) -> Option<f64> {
    let meters = match zoom {
        6 => 500_000.,
        7 => 400_000.,
        8 => 300_000.,
        9 => 8_000.,
        10 => 4_000.,
        11 => 1_000.,
        _ => return None,
    };
    Some(meters / WORLD_METERS * 256. * 2_f64.powi(i32::from(zoom)))
}

impl State {
    /// Serial evaluation: supplements load state, OSM sources apply at once.
    pub fn process(&mut self, source: &Source, output: &mut Vec<Feature>) {
        if source.kind == SourceKind::Supplement {
            self.supplement(source, output);
            return;
        }
        let mut contribution = Contribution::default();
        self.evaluate(source, output, &mut contribution);
        self.apply(contribution);
    }

    /// Replays one source's contribution; call in source order.
    pub fn apply(&mut self, contribution: Contribution) {
        for (id, length, feature) in contribution.rivers {
            *self.river_lengths.entry(id).or_default() += length;
            self.river_members.push((id, feature));
        }
        for (index, area, osm_id) in contribution.lakes {
            let lake = &mut self.lakes[index];
            if area > lake.intersection_area {
                lake.intersection_area = area;
                lake.osm_id = Some(osm_id);
            }
        }
    }

    /// Evaluates an OSM source against the read-only supplement state.
    pub fn evaluate(
        &self,
        source: &Source,
        output: &mut Vec<Feature>,
        contribution: &mut Contribution,
    ) {
        self.river_relation(source, &mut contribution.rivers);
        if source.is_line()
            && matches!(
                source.tag("waterway"),
                "stream" | "river" | "canal" | "drain" | "ditch"
            )
        {
            let min = if source.tag("waterway") == "river" && !source.tag("name").is_empty() {
                9
            } else if matches!(source.tag("waterway"), "river" | "canal") {
                12
            } else {
                13
            };
            // Split zooms with overrides so the encoder applies the exact source policy.
            for zoom in min..=14 {
                let mut f = Feature::new_original_geometry(
                    source,
                    "waterway",
                    source.geometry.clone(),
                    zoom,
                    zoom,
                );
                f.attrs
                    .insert("class".into(), json!(source.tag("waterway")));
                add_names(source, &mut f);
                brunnel(source, &mut f);
                f.attrs.insert(
                    "intermittent".into(),
                    json!(u8::from(truth(source.tag("intermittent")))),
                );
                f.attr_minzoom.insert("intermittent".into(), 12);
                if zoom <= 11 {
                    f.min_size = 0.;
                }
                if let Some(buffer) = merge_min_length_pixels(zoom) {
                    f.buffer_pixels = buffer;
                }
                output.push(f);
            }
        }
        if source.is_point()
            && !source.tag("name").trim().is_empty()
            && (matches!(source.tag("place"), "sea" | "ocean")
                || matches!(source.tag("natural"), "bay" | "strait"))
        {
            self.marine_point(source, output);
        }
        if !water_polygon(source) {
            return;
        }
        if source.tag("natural") != "bay" {
            let mut f = make_water(source, water_class(source), 6, 11);
            f.min_size = 2.;
            f.attrs.insert("id".into(), json!(source.osm_id));
            f.attrs.insert(
                "intermittent".into(),
                json!(u8::from(truth(source.tag("intermittent")))),
            );
            brunnel(source, &mut f);
            let mut high = f.clone();
            high.min_zoom = 12;
            high.max_zoom = 14;
            high.min_size = 1.;
            output.extend([f, high]);
            self.match_ne_lakes(source, &mut contribution.lakes);
        }
        self.water_name(source, output);
    }

    fn supplement(&mut self, source: &Source, output: &mut Vec<Feature>) {
        match source.source_layer.as_str() {
            "lake_centerlines" => {
                if let Some(id) = source
                    .tag("OSM_ID")
                    .parse::<f64>()
                    .ok()
                    .filter(|v| v.is_finite())
                    .map(|v| (v as i64).abs())
                    .filter(|v| *v != 0)
                {
                    self.centerlines
                        .entry(id)
                        .or_insert_with(|| MultiLineString(vec![]))
                        .0
                        .extend(lines(&source.geometry));
                }
            }
            "water_polygons" => output.push(make_water(source, "ocean", 6, 14)),
            "ne_110m_ocean" => output.push(make_water(source, "ocean", 0, 1)),
            "ne_50m_ocean" => output.push(make_water(source, "ocean", 2, 4)),
            "ne_10m_ocean" => output.push(make_water(source, "ocean", 5, 5)),
            "ne_110m_lakes" | "ne_50m_lakes" | "ne_10m_lakes" => {
                let (min_zoom, max_zoom) = match source.source_layer.as_str() {
                    "ne_110m_lakes" => (0, 1),
                    "ne_50m_lakes" => (2, 3),
                    _ => (4, 5),
                };
                let world = polygons(&world(&source.geometry));
                self.lakes.push(Lake {
                    source: source.clone(),
                    bounds: world.bounding_rect(),
                    world,
                    min_zoom,
                    max_zoom,
                    osm_id: None,
                    intersection_area: 0.,
                });
            }
            "ne_110m_rivers_lake_centerlines" | "ne_50m_rivers_lake_centerlines"
                if source.tag("featurecla") == "River" =>
            {
                let (min, max) = if source.source_layer == "ne_110m_rivers_lake_centerlines" {
                    (3, 3)
                } else {
                    (4, 5)
                };
                let mut f = Feature::new_original_geometry(
                    source,
                    "waterway",
                    source.geometry.clone(),
                    min,
                    max,
                );
                f.attrs.insert("class".into(), json!("river"));
                output.push(f);
            }
            "ne_10m_geography_marine_polys" => {
                if let Ok(rank) = source.tag("scalerank").parse::<i32>() {
                    let name = source
                        .tag("name")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                        .to_lowercase();
                    if !name.is_empty() {
                        self.marine_regions
                            .insert(name, (world(&source.geometry), rank));
                    }
                }
            }
            _ => {}
        }
    }

    fn match_ne_lakes(&self, source: &Source, matches: &mut Vec<(usize, f64, i64)>) {
        let geometry = polygons(&world(&source.geometry));
        if geometry.unsigned_area() < 4_f64.powi(-20) {
            return;
        }
        // Each NE table's largest lake with a name wins that name lookup.
        let mut by_name: BTreeMap<&str, usize> = BTreeMap::new();
        if !source.tag("name").is_empty() {
            for (index, lake) in self
                .lakes
                .iter()
                .enumerate()
                .filter(|(_, l)| l.source.tag("name") == source.tag("name"))
            {
                let entry = by_name.entry(&lake.source.source_layer).or_insert(index);
                if lake.world.unsigned_area() > self.lakes[*entry].world.unsigned_area() {
                    *entry = index;
                }
            }
        }
        let selected = if by_name.is_empty() {
            (0..self.lakes.len()).collect::<Vec<_>>()
        } else {
            by_name.into_values().collect()
        };
        let Some(bounds) = geometry.bounding_rect() else {
            return;
        };
        for index in selected {
            let lake = &self.lakes[index];
            let Some(other) = lake.bounds else {
                continue;
            };
            if bounds.max().x < other.min().x
                || bounds.min().x > other.max().x
                || bounds.max().y < other.min().y
                || bounds.min().y > other.max().y
            {
                continue;
            }
            let area = geometry.intersection(&lake.world).unsigned_area();
            // `apply` keeps the first source with the largest area; areas of
            // zero can never win because the running area starts at zero.
            if area > 0. {
                matches.push((index, area, source.osm_id));
            }
        }
    }

    fn river_relation(&self, source: &Source, rivers: &mut Vec<(i64, f64, Feature)>) {
        // Most sources have no qualifying river membership. Defer geometry
        // cloning/projection until one qualifies, then reuse its length.
        let mut geometry = None;
        for relation in &source.relations {
            let tag = |key: &str| relation.get(key).map(String::as_str).unwrap_or("");
            if tag("waterway") != "river"
                || tag("name").is_empty()
                || !matches!(tag("_role"), "" | "main_stream")
            {
                continue;
            }
            let Ok(id) = tag("_id").parse::<i64>() else {
                continue;
            };
            if geometry.is_none() {
                geometry = source.line().map(|line| {
                    let length = world_length(&line);
                    (line, length)
                });
            }
            let Some((line, length)) = &geometry else {
                return;
            };
            let mut f = Feature::new_original_geometry(source, "waterway", line.clone(), 6, 8);
            f.attrs.insert("class".into(), json!("river"));
            f.min_size = 0.;
            let mut named = source.clone();
            named.tags = relation.clone();
            add_names(&named, &mut f);
            rivers.push((id, *length, f));
        }
    }

    fn marine_point(&self, source: &Source, output: &mut Vec<Feature>) {
        let name = source.tag("name").to_lowercase();
        let candidate = self
            .marine_regions
            .get(&name)
            .or_else(|| {
                self.marine_regions
                    .get(&source.tag("name:en").to_lowercase())
            })
            .or_else(|| {
                self.marine_regions
                    .get(&source.tag("name:es").to_lowercase())
            })
            .or_else(|| {
                self.marine_regions
                    .range(name.clone()..)
                    .next()
                    .filter(|(n, _)| n.starts_with(&name))
                    .map(|(_, v)| v)
            });
        let rank = candidate
            .filter(|(g, _)| {
                Euclidean.distance(g, &world(&source.geometry)) <= 50_000. / WORLD_METERS
            })
            .map(|(_, r)| *r)
            .or_else(|| source.tag("rank").parse().ok());
        let min = if source.tag("place") == "ocean" {
            0
        } else if let Some(rank) = rank {
            rank.clamp(0, 255) as u8
        } else if source.tag("natural") == "bay" {
            13
        } else {
            8
        };
        if min > 14 {
            return;
        }
        let mut f = Feature::new(source, "water_name", source.geometry.clone(), min, 14);
        f.buffer_pixels = 256.;
        add_names(source, &mut f);
        let class = if source.tag("natural").is_empty() {
            source.tag("place")
        } else {
            source.tag("natural")
        };
        f.attrs.insert("class".into(), json!(class));
        f.attrs.insert(
            "intermittent".into(),
            json!(u8::from(truth(source.tag("intermittent")))),
        );
        output.push(f);
    }

    fn water_name(&self, source: &Source, output: &mut Vec<Feature>) {
        if source.tag("name").is_empty() || source.tag("leisure") == "swimming_pool" {
            return;
        }
        let (class, centerline_min) = if source.tag("natural") == "bay" {
            ("bay", 9)
        } else if source.tag("place") == "sea" {
            ("sea", 9)
        } else {
            ("lake", 3)
        };
        let centerline = self.centerlines.get(&source.osm_id);
        let setup = |geometry, min, max| {
            let mut f = Feature::new(source, "water_name", geometry, min, max);
            f.buffer_pixels = 256.;
            add_names(source, &mut f);
            f.attrs.insert("class".into(), json!(class));
            f.attrs.insert(
                "intermittent".into(),
                json!(u8::from(truth(source.tag("intermittent")))),
            );
            f
        };
        if let Some(line) = centerline {
            let mut f = setup(Geometry::MultiLineString(line.clone()), centerline_min, 13);
            f.min_size = 6. * source.tag("name").encode_utf16().count() as f64;
            let mut high = f.clone();
            high.min_zoom = 14;
            high.max_zoom = 14;
            output.extend([f, high]);
        }
        let min = if matches!(source.tag("place"), "sea" | "ocean") {
            0
        } else {
            3
        };
        if centerline.is_none() || min < centerline_min {
            let max = if centerline.is_some() {
                centerline_min - 1
            } else {
                14
            };
            let original_osm = matches!(source.kind, SourceKind::Way | SourceKind::Relation);
            let world_point = if original_osm {
                super::interior_point::osm_point_world(&source.geometry)
            } else {
                None
            };
            let point = if original_osm {
                world_point.map(|p| Geometry::Point(super::interior_point::world_to_latlon(p)))
            } else {
                source.point()
            };
            if let Some(point) = point {
                let mut f = setup(point, min, max.min(13));
                f.world_point = world_point;
                f.min_size = 128.;
                f.min_size_source_geometry = Some(source.geometry.clone());
                if max == 14 {
                    let mut high = f.clone();
                    high.min_zoom = 14;
                    high.max_zoom = 14;
                    output.push(high);
                }
                output.push(f);
            }
        }
    }

    pub fn finish(&mut self, output: &mut Vec<Feature>) {
        for lake in self.lakes.drain(..) {
            let mut f = make_water(&lake.source, "lake", lake.min_zoom, lake.max_zoom);
            // Planetiler emits delayed NE lakes as synthetic source features, id0.
            f.id = 0;
            if let Some(id) = lake.osm_id {
                f.attrs.insert("id".into(), json!(id));
            }
            output.push(f);
        }
        for (id, feature) in self.river_members.drain(..) {
            let length = self.river_lengths.get(&id).copied().unwrap_or(0.);
            for zoom in 6..=8 {
                let threshold =
                    merge_min_length_pixels(zoom).unwrap() / (256. * 2_f64.powi(i32::from(zoom)));
                if length >= threshold {
                    let mut f = feature.clone();
                    f.min_zoom = zoom;
                    f.max_zoom = zoom;
                    output.push(f);
                }
            }
        }
        self.river_lengths.clear();
        self.centerlines.clear();
        self.marine_regions.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{Point, polygon};
    fn source(tags: &[(&str, &str)], geometry: Geometry<f64>) -> Source {
        Source {
            id: 102,
            osm_id: 10,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            geometry,
            relations: vec![],
        }
    }
    fn lake() -> Geometry<f64> {
        polygon![(x:19.,y:49.),(x:20.,y:49.),(x:20.,y:50.),(x:19.,y:50.),(x:19.,y:49.)].into()
    }
    #[test]
    fn river_memberships_keep_duplicate_lengths_and_feature_order() {
        let mut s = source(&[], LineString::from(vec![(19., 49.), (20., 50.)]).into());
        let relation = |id: &str, role: &str, name: &str| {
            [
                ("_id", id),
                ("_role", role),
                ("name", name),
                ("waterway", "river"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
        };
        s.relations = vec![
            relation("1", "side_stream", "Side"),
            relation("bad", "", "Bad"),
            relation("2", "main_stream", "First"),
            relation("3", "", "Second"),
            relation("2", "", "Third"),
            relation("4", "", ""),
        ];
        let mut state = State::default();
        let mut contribution = Contribution::default();
        state.river_relation(&s, &mut contribution.rivers);
        state.apply(contribution);
        assert_eq!(
            state
                .river_members
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>(),
            vec![2, 3, 2]
        );
        let length = world_length(&s.line().unwrap());
        assert_eq!(state.river_lengths[&2], length + length);
        assert_eq!(state.river_lengths[&3], length);
        assert_eq!(state.river_members[0].1.attrs["name"], json!("First"));
        assert_eq!(state.river_members[2].1.attrs["name"], json!("Third"));
        s.geometry = Point::new(19., 50.).into();
        let mut contribution = Contribution::default();
        State::default().river_relation(&s, &mut contribution.rivers);
        assert!(contribution.is_empty());
    }
    #[test]
    fn water_filters_and_class_precedence() {
        assert!(truth("building_passage"));
        assert!(truth("viaduct"));
        assert!(!truth("no"));
        let s = source(
            &[
                ("natural", "water"),
                ("water", "river"),
                ("waterway", "dock"),
            ],
            lake(),
        );
        assert!(water_polygon(&s));
        assert_eq!(water_class(&s), "dock");
        let mut blocked = s.clone();
        blocked.tags.insert("covered".into(), "yes".into());
        assert!(!water_polygon(&blocked));
    }
    #[test]
    fn named_river_zoom_and_attribute_policy() {
        let s = source(
            &[("waterway", "river"), ("name", "Wisla"), ("bridge", "yes")],
            LineString::from(vec![(19., 49.), (20., 50.)]).into(),
        );
        let mut output = vec![];
        State::default().process(&s, &mut output);
        assert_eq!(output.first().unwrap().min_zoom, 9);
        assert_eq!(output.first().unwrap().attr_minzoom["brunnel"], 12);
        assert_eq!(output.first().unwrap().min_size, 0.);
        assert_eq!(output.last().unwrap().max_zoom, 14);
    }
    #[test]
    fn lake_label_preserves_polygon_size_and_ne_id_join() {
        let mut state = State::default();
        let mut output = vec![];
        let mut ne = source(&[("name", "Lake")], lake());
        ne.kind = SourceKind::Supplement;
        ne.source_layer = "ne_10m_lakes".into();
        state.process(&ne, &mut output);
        let osm = source(&[("natural", "water"), ("name", "Lake")], lake());
        state.process(&osm, &mut output);
        assert!(
            output
                .iter()
                .filter(|f| f.layer == "water_name")
                .all(|f| f.min_size_source_geometry.is_some())
        );
        state.finish(&mut output);
        assert!(output.iter().any(|f| f.layer == "water"
            && f.min_zoom == 4
            && f.attrs.get("id") == Some(&json!(10))));
    }
    #[test]
    fn marine_ocean_is_visible_at_zoom_zero() {
        let s = source(
            &[("place", "ocean"), ("name", "Ocean")],
            Point::new(19., 50.).into(),
        );
        let mut output = vec![];
        State::default().process(&s, &mut output);
        assert_eq!(output[0].min_zoom, 0);
        assert_eq!(output[0].buffer_pixels, 256.);
    }

    #[test]
    fn actual_osm_water_name_uses_packed_node_interior_point() {
        // Way 178849078, z14/9065/5555: the raw PBF coordinates put the
        // interior point at tile Y=4858.508 (rounds to 4859). Planetiler's
        // packed node store puts it at 4858.46875 (rounds to 4858).
        let ring = vec![
            (19.2184104, 50.0070799),
            (19.2206873, 50.0063701),
            (19.2208037, 50.0062916),
            (19.2208605, 50.0062113),
            (19.2208832, 50.0060125),
            (19.2209570, 50.0058902),
            (19.2210848, 50.0058282),
            (19.2218627, 50.0057004),
            (19.2221126, 50.0056311),
            (19.2241936, 50.0050307),
            (19.2242391, 50.0049778),
            (19.2242362, 50.0048702),
            (19.2241993, 50.0047625),
            (19.2240744, 50.0046913),
            (19.2240517, 50.0045983),
            (19.2240829, 50.0044432),
            (19.2240119, 50.0042534),
            (19.2239041, 50.0040892),
            (19.2235605, 50.0037406),
            (19.2233987, 50.0035052),
            (19.2231517, 50.0033611),
            (19.2227031, 50.0033191),
            (19.2225384, 50.0033592),
            (19.2222176, 50.0031348),
            (19.2212154, 50.0033611),
            (19.2201933, 50.0034760),
            (19.2190406, 50.0035837),
            (19.2181634, 50.0036731),
            (19.2177119, 50.0037351),
            (19.2176040, 50.0038538),
            (19.2171668, 50.0047315),
            (19.2169454, 50.0051895),
            (19.2169482, 50.0053227),
            (19.2170561, 50.0054924),
            (19.2175416, 50.0060690),
            (19.2179788, 50.0066803),
            (19.2182854, 50.0070598),
            (19.2183451, 50.0070872),
            (19.2184104, 50.0070799),
        ];
        let mut source = source(
            &[("natural", "water"), ("name", "Staw Górny")],
            Geometry::Polygon(geo_types::Polygon::new(LineString::from(ring), vec![])),
        );
        source.id = 1_788_490_782;
        source.osm_id = 178_849_078;
        let mut output = Vec::new();
        State::default().process(&source, &mut output);
        let point = output
            .iter()
            .find(|f| f.layer == "water_name" && f.min_zoom == 14)
            .and_then(|f| f.world_point)
            .unwrap();
        assert_eq!(
            ((point.y * 16384. - 5555.) * 4096. + 0.5).floor() as i32,
            4858
        );
    }
}
