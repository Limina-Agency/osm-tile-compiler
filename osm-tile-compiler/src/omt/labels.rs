/*
Copyright (c) 2024, MapTiler.com & OpenMapTiles contributors.
All rights reserved.

Code license: BSD 3-Clause License

Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions are met:

* Redistributions of source code must retain the above copyright notice, this
  list of conditions and the following disclaimer.

* Redistributions in binary form must reproduce the above copyright notice,
  this list of conditions and the following disclaimer in the documentation
  and/or other materials provided with the distribution.

* Neither the name of the copyright holder nor the names of its
  contributors may be used to endorse or promote products derived from
  this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

Design license: CC-BY 4.0

See https://github.com/openmaptiles/openmaptiles/blob/master/LICENSE.md for details on usage
*/
// Native port of pinned planetiler-openmaptiles 91516fdf label layer rules.
use super::types::{Feature, Source, add_names, java_meters, parse_number, parse_profile_long};
use geo::{Area, BoundingRect, Centroid, Intersects, MapCoords};
use geo_types::{Coord, Geometry};
use icu_experimental::transliterate::Transliterator;
use serde_json::json;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use unicode_script::{Script, UnicodeScript};

const EARTH_CIRCUMFERENCE: f64 = 40_075_016.68557849;

/// The languages whose `name:<lang>` tags the pinned schema copies into
/// `name:<lang>` attributes. Codes with an underscore are listed but never
/// written: Planetiler accepts only valid language tags.
pub const LANGUAGES: &[&str] = &[
    "af", "am", "ar", "az", "be", "bg", "bn", "br", "bs", "ca", "co", "cs", "cy", "da", "de", "el",
    "en", "eo", "es", "et", "eu", "fa", "fi", "fr", "fy", "ga", "gd", "he", "hi", "hr", "hu", "hy",
    "id", "is", "it", "ja", "ja_kana", "ja_rm", "ja-Latn", "ja-Hira", "ka", "kk", "kn", "ko",
    "ko-Latn", "ku", "la", "lb", "lt", "lv", "mk", "mt", "ml", "nl", "no", "oc", "pa", "pnb", "pl",
    "pt", "rm", "ro", "ru", "sk", "sl", "sq", "sr", "sr-Latn", "sv", "ta", "te", "th", "tok", "tr",
    "uk", "ur", "vi", "zh", "zh-Hant", "zh-Hans",
];

fn only_latin(value: &str) -> bool {
    static NON_LATIN: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"[\p{L}&&\P{Latin}]").unwrap());
    !value.is_empty() && !NON_LATIN.is_match(value)
}

/// `name:<lang>` keys that pass `valid_name_key`, in `LANGUAGES` order.
static LANGUAGE_KEYS: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(|| {
    LANGUAGES
        .iter()
        .map(|lang| format!("name:{lang}"))
        .filter(|key| valid_name_key(key))
        .collect()
});

fn valid_name_key(key: &str) -> bool {
    // Locale.Builder rejects underscore variants even when listed in schema.
    static LANGUAGE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^name:[a-z]{2,3}(?:-[A-Za-z0-9]{2,8})*$").unwrap()
    });
    LANGUAGE.is_match(key)
}

fn nonlatin_name(value: &str) -> String {
    static LETTERS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"[\p{L}&&\p{Latin}]+").unwrap());
    static PARENS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"\([ -.]*\)|\[[ -.]*\]").unwrap());
    if !LETTERS.is_match(value) {
        return value.trim().to_string();
    }
    let cleaned = LETTERS.replace_all(value, "");
    let cleaned = PARENS.replace_all(&cleaned, "");
    cleaned
        .trim_matches(|c: char| c.is_whitespace() || matches!(c, '.' | '/' | '-'))
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

thread_local! {
    // ICU4X's transliterators are not Send/Sync. The compiled CLDR rules are
    // local to the binary; each processing thread initializes only scripts it
    // encounters, and caches absent rules too.
    static LATIN_TRANSLITERATORS: RefCell<HashMap<Script, Option<Transliterator>>> =
        RefCell::new(HashMap::new());
}

/// Apply the compiled ICU4X script-to-Latin transforms available for this
/// name. ICU4X has no bundled Any-Latin aggregate, so this is a qualified
/// subset of Planetiler's ICU Any-Latin fallback. A partially transformed
/// string is not emitted as `name:latin` when unsupported letters remain.
/// Planetiler emits Any-Latin's result even when it equals the name, which it
/// does when every letter is Latin, Common or Inherited script ("Café µ",
/// "Thobenʼs Backwaren"): such a name fails `containsOnlyLatinCharacters`
/// on its Common-script letters, so it reaches this fallback.
fn transliterate_name(value: &str) -> Option<String> {
    LATIN_TRANSLITERATORS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let mut scripts = Vec::new();
        let mut output = value.to_owned();
        for script in value.chars().map(|ch| ch.script()) {
            if matches!(
                script,
                Script::Common | Script::Inherited | Script::Latin | Script::Unknown
            ) || scripts.contains(&script)
            {
                continue;
            }
            scripts.push(script);
            let transform = cache.entry(script).or_insert_with(|| {
                // Unicode identifies Han characters as Hani, while the
                // compiled default Han-to-Latin rule uses Hans.
                let source = if script == Script::Han {
                    "Hans"
                } else {
                    script.short_name()
                };
                let id = format!("und-Latn-t-und-{source}");
                id.parse()
                    .ok()
                    .and_then(|locale| Transliterator::try_new(&locale).ok())
            });
            if let Some(transform) = transform {
                output = transform.transliterate(output);
            }
        }
        // ICU's Cyrillic-Latin rules can legitimately emit Common-script
        // modifier letters such as U+02BA in Bʺlgariâ. Still withhold a
        // fallback if any unconverted source-script letters remain.
        let latin_scripts_only = output.chars().all(|ch| {
            matches!(
                ch.script(),
                Script::Latin | Script::Common | Script::Inherited
            )
        });
        latin_scripts_only.then_some(output)
    })
}

/// Tags in the order `OmtLanguageUtils` walks Planetiler's tag `HashMap`.
/// The PBF reader keeps tags sorted by key, not in file order, so keys are
/// treated as put in that order; that only matters within a hash bin.
/// Internal `_` keys are not OSM tags and do not size the Java map.
fn java_tag_order(source: &Source) -> Vec<(&str, &str)> {
    let tags: Vec<(&str, &str)> = source
        .tags
        .iter()
        .filter(|(k, _)| !k.starts_with('_'))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let keys: Vec<&str> = tags.iter().map(|(k, _)| *k).collect();
    super::java_hashmap::new_hash_map_put_order(&keys)
        .into_iter()
        .map(|i| tags[i])
        .collect()
}

/// OmtLanguageUtils default fields plus the pinned schema's OSM languages.
/// Wikidata enrichment requires an explicit provider.
pub fn add_profile_names(source: &Source, feature: &mut Feature) {
    add_profile_names_inner(source, feature, true);
}

fn add_profile_names_inner(source: &Source, feature: &mut Feature, translations: bool) {
    let name = source.tag("name");
    let en = source.tag("name:en");
    let de = source.tag("name:de");
    let int = source.tag("int_name");
    let fallback = |values: &[&str]| {
        values
            .iter()
            .find(|v| !v.is_empty())
            .copied()
            .unwrap_or("")
            .to_string()
    };
    let name_is_latin = only_latin(name);
    let latin = if name_is_latin {
        name
    } else {
        [en, int, de]
            .into_iter()
            .chain(
                java_tag_order(source)
                    .into_iter()
                    .filter(|(k, _)| *k != "name:en" && *k != "name:de" && valid_name_key(k))
                    .map(|(_, v)| v),
            )
            .find(|v| only_latin(v))
            .unwrap_or("")
    };
    let transliterated = if latin.is_empty() && translations {
        transliterate_name(name)
    } else {
        None
    };
    let latin = transliterated.as_deref().unwrap_or(latin);
    string_attr(feature, "name", name);
    string_attr(feature, "name_en", &fallback(&[en, name]));
    string_attr(feature, "name_de", &fallback(&[de, name, en]));
    string_attr(feature, "name:latin", latin);
    string_attr(feature, "name_int", &fallback(&[int, en, latin, name]));
    if !name_is_latin {
        let nonlatin = nonlatin_name(name);
        if nonlatin != latin {
            string_attr(feature, "name:nonlatin", &nonlatin);
        }
    }
    if translations {
        for key in LANGUAGE_KEYS.iter() {
            if let Some(value) = source.tags.get(key) {
                feature.attrs.insert(key.clone(), json!(value));
            }
        }
    }
}

// Commons Lang 3.20.0 stripAccents: NFKD, stroke map, U+0300..036F only.
// Matching key only; display names are preserved. Unicode table versions may differ.
fn strip_city_accents(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.nfkd()
        .filter_map(|c| match c {
            c if (0x0300..=0x036f).contains(&(c as u32)) => None,
            'Ł' => Some('L'),
            'ł' => Some('l'),
            'Đ' => Some('D'),
            'đ' => Some('d'),
            'Ɨ' | 'ᵻ' | 'ᶧ' => Some('I'),
            'ɨ' | 'ᶤ' => Some('i'),
            'Ʉ' | 'ᵾ' => Some('U'),
            'ʉ' | 'ᶶ' => Some('u'),
            'Ŧ' => Some('T'),
            'ŧ' => Some('t'),
            other => Some(other),
        })
        .collect()
}

struct Region {
    geometry: Geometry<f64>,
    name: String,
    rank: i64,
}
struct City {
    /// World-plane position of the populated place.
    world: Coord<f64>,
    name: String,
    names: Vec<String>,
    wikidata: String,
    rank: i64,
}

#[derive(Default)]
pub struct State {
    countries: Vec<Region>,
    states: Vec<Region>,
    cities: Vec<City>,
    /// The Natural Earth US geometry with its bounding rect, which lets most
    /// points skip the polygon test.
    us: Option<(Geometry<f64>, geo_types::Rect<f64>)>,
    stops: BTreeMap<String, Vec<Feature>>,
}

/// Transit stop candidates one OSM source adds to `State`: aggregation key and
/// feature. The reference-data lookups are read-only while OSM sources are
/// evaluated concurrently; `State::apply` replays these in source order.
#[derive(Default)]
pub struct Contribution {
    stops: Vec<(String, Feature)>,
}

impl Contribution {
    pub fn is_empty(&self) -> bool {
        self.stops.is_empty()
    }
}

impl State {
    /// Serial evaluation: supplements load state, OSM sources apply at once.
    pub fn process(&mut self, source: &Source, out: &mut Vec<Feature>) {
        if source.source_layer.starts_with("ne_") {
            self.supplement(source);
            return;
        }
        let mut contribution = Contribution::default();
        self.evaluate(source, out, &mut contribution);
        self.apply(contribution);
    }

    /// Replays one source's contribution; call in source order.
    pub fn apply(&mut self, contribution: Contribution) {
        for (key, feature) in contribution.stops {
            self.stops.entry(key).or_default().push(feature);
        }
    }

    /// Evaluates an OSM source against the read-only supplement state.
    pub fn evaluate(
        &self,
        source: &Source,
        out: &mut Vec<Feature>,
        contribution: &mut Contribution,
    ) {
        let start = out.len();
        process(source, out);
        if source.is_point()
            && source.has("name")
            && matches!(source.tag("place"), "state" | "province")
            && let Some(region) = self
                .states
                .iter()
                .find(|r| r.geometry.intersects(&source.geometry))
            && let Some(mut f) = point_feature(source, "place", 2)
        {
            f.buffer_pixels = 256.0;
            f.sort_key = region.rank.clamp(1, 6);
            f.attrs.insert("rank".into(), json!(f.sort_key));
            add_names(source, &mut f);
            string_attr(&mut f, "class", source.tag("place"));
            if !f.attrs.contains_key("name_en") {
                string_attr(&mut f, "name_en", &region.name);
            }
            out.push(f);
        }
        for feature in &mut out[start..] {
            if feature.layer == "mountain_peak"
                && source.is_point()
                && self.us.as_ref().is_some_and(|(us, bounds)| {
                    source.geometry.bounding_rect().is_some_and(|r| {
                        r.max().x >= bounds.min().x
                            && r.min().x <= bounds.max().x
                            && r.max().y >= bounds.min().y
                            && r.min().y <= bounds.max().y
                    }) && us.intersects(&source.geometry)
                })
            {
                feature.attrs.insert("customary_ft".into(), json!(1));
            }
            if feature.layer == "place"
                && source.tag("place") == "country"
                && let Some(region) = self
                    .countries
                    .iter()
                    .find(|r| r.geometry.intersects(&source.geometry))
            {
                let rank = region.rank.clamp(1, 6);
                feature.min_zoom = (rank - 1) as u8;
                feature.sort_key = rank;
                feature.attrs.insert("rank".into(), json!(rank));
                if !feature.attrs.contains_key("name_en") {
                    string_attr(feature, "name_en", &region.name);
                }
            }
            if feature.layer == "place"
                && matches!(source.tag("place"), "city" | "town" | "village")
                && let Some(p) = source.geometry.centroid()
            {
                let c = world_coord(p.0);
                let normalized_name = strip_city_accents(source.tag("name"));
                if let Some(city) = self.cities.iter().find(|city| {
                    let q = city.world;
                    (c.x - q.x).hypot(c.y - q.y) <= 50_000.0 / EARTH_CIRCUMFERENCE
                        && ((!source.tag("wikidata").is_empty()
                            && city.wikidata == source.tag("wikidata"))
                            || city.names.contains(&source.tag("name").to_lowercase())
                            || city.names.contains(&source.tag("name:en").to_lowercase())
                            || normalized_name == city.name)
                }) {
                    let rank = if city.rank <= 5 {
                        city.rank + 1
                    } else {
                        city.rank
                    };
                    feature.attrs.insert("rank".into(), json!(rank));
                    feature.min_zoom = if rank == 1 {
                        2
                    } else if rank <= 8 {
                        (rank - 1).max(3) as u8
                    } else {
                        feature.min_zoom
                    };
                    feature.sort_key = place_order(
                        Some(rank),
                        source.tag("place"),
                        source.tag("population").parse().unwrap_or(0),
                        source.tag("name"),
                    );
                    feature.grid_limits_by_zoom.clear();
                    feature.grid_limit = 0;
                }
            }
        }
        let tail = out.split_off(start);
        for mut feature in tail {
            if let Some(key) = feature
                .attrs
                .remove("__agg_stop_key")
                .and_then(|v| v.as_str().map(str::to_string))
            {
                contribution.stops.push((key, feature));
            } else {
                out.push(feature);
            }
        }
    }

    fn supplement(&mut self, source: &Source) {
        let number = |key| parse_number(source.tag(key)).unwrap_or(0.0);
        match source.source_layer.as_str() {
            "ne_10m_admin_0_countries" => {
                self.countries.push(Region {
                    geometry: source.geometry.clone(),
                    name: source.tag("name").into(),
                    rank: ((number("scalerank") + number("labelrank")) / 2.0).ceil() as i64,
                });
                if source.tag("iso_a2") == "US" {
                    self.us = source
                        .geometry
                        .bounding_rect()
                        .map(|bounds| (source.geometry.clone(), bounds));
                }
            }
            "ne_10m_admin_1_states_provinces"
                if source.has("scalerank")
                    && source.has("labelrank")
                    && number("scalerank") <= 6.0
                    && number("labelrank") <= 7.0 =>
            {
                self.states.push(Region {
                    geometry: source.geometry.clone(),
                    name: source.tag("name").into(),
                    rank: ((number("scalerank") + number("labelrank") + number("datarank")) / 3.0)
                        .ceil() as i64,
                });
            }
            "ne_10m_populated_places" => {
                if let Some(point) = source.geometry.centroid() {
                    self.cities.push(City {
                        world: world_coord(point.0),
                        name: source.tag("name").into(),
                        names: ["name", "namealt", "meganame", "name_en", "nameascii"]
                            .iter()
                            .filter_map(|k| source.tags.get(*k))
                            .map(|s| s.to_lowercase())
                            .collect(),
                        wikidata: source.tag("wikidataid").into(),
                        rank: number("scalerank") as i64,
                    });
                }
            }
            _ => {}
        }
    }

    pub fn finish(&mut self, out: &mut Vec<Feature>) {
        for (_, mut group) in std::mem::take(&mut self.stops) {
            let points: Vec<_> = group
                .iter()
                .filter_map(|f| f.geometry.centroid())
                .map(|p| world_coord(p.0))
                .collect();
            let center = Coord {
                x: points.iter().map(|p| p.x).sum::<f64>() / points.len().max(1) as f64,
                y: points.iter().map(|p| p.y).sum::<f64>() / points.len().max(1) as f64,
            };
            let best = group
                .iter()
                .enumerate()
                .min_by(|(_, a), (_, b)| {
                    let order = |f: &Feature| {
                        f.attrs
                            .get("__agg_stop_order")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(9)
                    };
                    let distance = |f: &Feature| {
                        f.geometry
                            .centroid()
                            .map(|p| {
                                let p = world_coord(p.0);
                                (p.x - center.x).hypot(p.y - center.y)
                            })
                            .unwrap_or(f64::INFINITY)
                    };
                    order(a)
                        .cmp(&order(b))
                        .then_with(|| distance(a).total_cmp(&distance(b)))
                        .then_with(|| a.id.cmp(&b.id))
                })
                .map(|(index, _)| index);
            for (index, mut feature) in group.drain(..).enumerate() {
                feature.attrs.remove("__agg_stop_order");
                if Some(index) == best {
                    feature.attrs.insert("agg_stop".into(), json!(1));
                }
                out.push(feature);
            }
        }
    }
}

fn world_coord(c: Coord<f64>) -> Coord<f64> {
    Coord {
        x: (c.x + 180.0) / 360.0,
        y: (1.0 - c.y.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0,
    }
}

fn world_area(source: &Source) -> f64 {
    let geometry = source.geometry.map_coords(world_coord);
    match geometry {
        Geometry::Polygon(p) => p.unsigned_area(),
        Geometry::MultiPolygon(p) => p.unsigned_area(),
        _ => 0.0,
    }
}

pub(super) fn elevation_meters(source: &Source) -> Option<f64> {
    // For example OSM way 93759497 has ele="241,4"; the reference finds the
    // trailing "4", not 241.4.
    java_meters(source.tag("ele")).filter(|value| value.is_finite())
}

fn add_elevation(source: &Source, feature: &mut Feature) {
    if let Some(meters) = elevation_meters(source) {
        feature
            .attrs
            .insert("ele".into(), json!((meters + 0.5).floor() as i64));
        feature.attrs.insert(
            "ele_ft".into(),
            json!((meters * 3.2808399 + 0.5).floor() as i64),
        );
    }
}

fn string_attr(feature: &mut Feature, key: &str, value: &str) {
    if !value.is_empty() {
        feature.attrs.insert(key.into(), json!(value));
    }
}

fn quantize_label_source(source: &Source) -> bool {
    matches!(
        source.kind,
        super::types::SourceKind::Node
            | super::types::SourceKind::Way
            | super::types::SourceKind::Relation
    )
}

fn point_feature(source: &Source, layer: &str, minzoom: u8) -> Option<Feature> {
    if quantize_label_source(source) {
        let world = super::interior_point::osm_point_world(&source.geometry)?;
        Some(Feature::new_world_point(source, layer, world, minzoom, 14))
    } else {
        let point = source.point()?;
        Some(Feature::new(source, layer, point, minzoom, 14))
    }
}

pub(super) fn centroid_label_feature(source: &Source, layer: &str, minzoom: u8) -> Option<Feature> {
    if quantize_label_source(source) {
        let world =
            super::interior_point::osm_point_then_centroid_if_convex_world(&source.geometry)?;
        Some(Feature::new_world_point(source, layer, world, minzoom, 14))
    } else {
        let point = super::interior_point::point_then_centroid_if_convex(&source.geometry)?;
        Some(Feature::new(
            source,
            layer,
            Geometry::Point(point),
            minzoom,
            14,
        ))
    }
}

fn area_rank(area: f64) -> i64 {
    for (meters, rank) in [
        (1_000_000.0, 6),
        (15_000_000.0, 5),
        (40_000_000.0, 4),
        (160_000_000.0, 3),
        (640_000_000.0, 2),
    ] {
        if area <= meters / EARTH_CIRCUMFERENCE.powi(2) {
            return rank;
        }
    }
    1
}

/// Bounded packed precedence; logarithmic bins match the upstream SortKey ordering.
fn log_order(value: f64, start: f64, end: f64, levels: i64) -> i64 {
    if start > end {
        return log_order(start / value, end, start, levels);
    }
    let value = value.max(start).min(end);
    ((((value.ln() - start.ln()) / (end.ln() - start.ln())) * levels as f64).floor() as i64)
        .min(levels - 1)
}

fn place_order(rank: Option<i64>, place: &str, population: i64, name: &str) -> i64 {
    let kind = [
        "city",
        "town",
        "village",
        "hamlet",
        "suburb",
        "quarter",
        "neighbourhood",
        "isolated_dwelling",
    ]
    .iter()
    .position(|p| *p == place)
    .unwrap_or(8) as i64;
    // Planetiler reserves 22 sort-key bits. Upstream expressions 1 << (bits-13)-1
    // use Java shift precedence, producing 256 population bins.
    let pop = log_order(population as f64, 100_000_000.0, 1.0, 256);
    (((rank.unwrap_or(15).clamp(0, 15) * 16 + kind) * 256 + pop) * 32)
        + name.encode_utf16().count().min(31) as i64
        - (1 << 21)
}

pub fn process(source: &Source, out: &mut Vec<Feature>) {
    if !source.source_layer.is_empty() && source.source_layer.starts_with("ne_") {
        return;
    }
    place(source, out);
    peak(source, out);
    park(source, out);
    aerodrome(source, out);
    housenumber(source, out);
    poi(source, out);
}

fn place(source: &Source, out: &mut Vec<Feature>) {
    let kind = source.tag("place");
    if source.is_point() && source.has("name") {
        let mut feature = match kind {
            "continent" if !source.tag("name").is_empty() => {
                let Some(mut f) = point_feature(source, "place", 0) else {
                    return;
                };
                f.max_zoom = 3;
                f.attrs.insert("rank".into(), json!(1));
                f
            }
            "country" if !source.tag("name").is_empty() => {
                let iso = [
                    "country_code_iso3166_1_alpha_2",
                    "ISO3166-1:alpha2",
                    "ISO3166-1",
                ]
                .iter()
                .map(|key| source.tag(key))
                .find(|s| !s.is_empty());
                let Some(iso) = iso else {
                    return;
                };
                let Some(mut f) = point_feature(source, "place", 5) else {
                    return;
                };
                string_attr(&mut f, "iso_a2", iso);
                f.attrs.insert("rank".into(), json!(6));
                f.sort_key = 6;
                f
            }
            "island" => {
                let Some(mut f) = point_feature(source, "place", 12) else {
                    return;
                };
                f.attrs.insert("rank".into(), json!(7));
                f
            }
            "city" | "town" | "village" | "hamlet" | "borough" | "suburb" | "quarter"
            | "neighbourhood" | "isolated_dwelling" => {
                let z = match kind {
                    "city" | "town" => 7,
                    "village" => 8,
                    "hamlet" | "suburb" => 11,
                    _ => 14,
                };
                let Some(mut f) = point_feature(source, "place", z) else {
                    return;
                };
                f.sort_key = place_order(
                    None,
                    kind,
                    source.tag("population").parse().unwrap_or(0),
                    source.tag("name"),
                );
                f.grid_size = 128.0;
                f.grid_max_zoom = 12;
                f.grid_limits_by_zoom = vec![(8, 4), (9, 8), (10, 12), (12, 14)];
                f.rank = true;
                match source.tag("capital") {
                    "yes" | "2" => {
                        f.attrs.insert("capital".into(), json!(2));
                    }
                    "3" | "4" | "5" | "6" => {
                        f.attrs.insert(
                            "capital".into(),
                            json!(source.tag("capital").parse::<i64>().unwrap()),
                        );
                    }
                    _ => {}
                }
                f
            }
            _ => return,
        };
        feature.buffer_pixels = 256.0;
        string_attr(&mut feature, "class", kind);
        add_names(source, &mut feature);
        out.push(feature);
    } else if source.is_polygon()
        && ((kind == "island" && source.has("name"))
            || (source.tag("boundary") == "aboriginal_lands" && source.tag("type") == "boundary"))
    {
        let area = world_area(source);
        let rank = area_rank(area);
        let island = kind == "island";
        let z = if island {
            if rank <= 3 {
                8
            } else if rank == 4 {
                9
            } else {
                10
            }
        } else {
            if rank <= 4 { (rank + 5) as u8 } else { 10 }
        };
        if let Some(mut f) = point_feature(source, "place", z) {
            f.buffer_pixels = 256.0;
            string_attr(
                &mut f,
                "class",
                if island { "island" } else { "aboriginal_lands" },
            );
            f.attrs.insert("rank".into(), json!(rank));
            add_names(source, &mut f);
            if island {
                f.sort_key = log_order(area, 1.0, 4_f64.powi(-26), (1 << 22) - 1) - (1 << 21);
            }
            out.push(f);
        }
    }
}

fn peak(source: &Source, out: &mut Vec<Feature>) {
    let natural = source.tag("natural");
    if source.is_point() && matches!(natural, "peak" | "volcano" | "saddle") {
        let Some(mut f) = point_feature(source, "mountain_peak", 7) else {
            return;
        };
        add_elevation(source, &mut f);
        let elevation = elevation_meters(source);
        if source.tag("name").is_empty() && !elevation.is_some_and(|x| x.abs() < 10_000.0) {
            return;
        }
        add_names(source, &mut f);
        string_attr(&mut f, "class", natural);
        f.sort_key = -(elevation.unwrap_or(0.0) as i64
            + if source.tag("name").is_empty() {
                0
            } else {
                10_000
            }
            + if source.tag("wikipedia").is_empty() {
                0
            } else {
                10_000
            });
        f.buffer_pixels = 100.0;
        f.grid_size = 100.0;
        f.grid_max_zoom = 13;
        f.grid_limit = 5;
        f.rank = true;
        out.push(f);
    } else if source.is_line()
        && matches!(natural, "ridge" | "cliff" | "arete")
        && let Some(g) = source.line()
    {
        let mut f = Feature::new_original_geometry(source, "mountain_peak", g, 13, 14);
        let rank = 3
            - i64::from(!source.tag("name").is_empty())
            - i64::from(!source.tag("wikipedia").is_empty());
        f.attrs.insert("rank".into(), json!(rank));
        f.sort_key = rank;
        f.buffer_pixels = 100.0;
        string_attr(&mut f, "class", natural);
        add_names(source, &mut f);
        out.push(f);
    }
}

fn park(source: &Source, out: &mut Vec<Feature>) {
    if !source.is_polygon()
        || !(source.tag("leisure") == "nature_reserve"
            || matches!(source.tag("boundary"), "national_park" | "protected_area"))
    {
        return;
    }
    let class = if !source.tag("protection_title").is_empty() {
        source
            .tag("protection_title")
            .replace(' ', "_")
            .to_lowercase()
    } else if !source.tag("boundary").is_empty() {
        source.tag("boundary").into()
    } else {
        source.tag("leisure").into()
    };
    let mut shape = Feature::new(source, "park", source.geometry.clone(), 4, 14);
    shape.osm_outline_projection = matches!(
        source.kind,
        super::types::SourceKind::Way | super::types::SourceKind::Relation
    );
    shape.buffer_pixels = 4.0;
    shape.min_size = 2.0;
    string_attr(&mut shape, "class", &class);
    shape.attr_minzoom.insert("class".into(), 5);
    if source.has("name") {
        add_profile_names_inner(source, &mut shape, false);
        for key in shape.attrs.keys() {
            shape.attr_minzoom.insert(key.clone(), 5);
        }
        let area = world_area(source);
        let z = (20.0 - (area / (70_000.0 / EARTH_CIRCUMFERENCE.powi(2))).log2())
            .floor()
            .clamp(5.0, 14.0) as u8;
        if let Some(mut f) = point_feature(source, "park", z) {
            f.buffer_pixels = 256.0;
            f.grid_size = 100.0;
            f.grid_max_zoom = 14;
            f.rank = true;
            string_attr(&mut f, "class", &class);
            add_names(source, &mut f);
            f.sort_key = (i64::from(class != "national_park") * 2
                + i64::from(!source.has("wikipedia") && !source.has("wikidata")))
                * (1 << 19)
                + log_order(area, 1.0, 4_f64.powi(-26), 1 << 19)
                - (1 << 21);
            out.push(f);
        }
    }
    out.push(shape);
}

fn aerodrome(source: &Source, out: &mut Vec<Feature>) {
    if source.tag("aeroway") != "aerodrome" || !(source.is_point() || source.is_polygon()) {
        return;
    }
    // Generated schema matches aerodrome_type (underscore), not aerodrome:type.
    let a = source.tag("aerodrome");
    let t = source.tag("aerodrome_type");
    let class = if a == "international" || t == "international" {
        "international"
    } else if a == "public" || t.contains("public") || t == "civil" {
        "public"
    } else if a == "regional" || t == "regional" {
        "regional"
    } else if a == "military" || t.contains("military") || source.tag("military") == "airfield" {
        "military"
    } else if a == "private" || t == "private" {
        "private"
    } else {
        "other"
    };
    if let Some(mut f) = point_feature(
        source,
        "aerodrome_label",
        if class == "international" && !source.tag("iata").is_empty() {
            8
        } else {
            10
        },
    ) {
        if quantize_label_source(source) {
            if let Some(world) = super::interior_point::osm_centroid_world(&source.geometry) {
                f.geometry = Geometry::Point(super::interior_point::world_to_latlon(world));
                f.world_point = Some(world);
            }
        } else if let Some(point) = super::interior_point::centroid(&source.geometry) {
            f.geometry = Geometry::Point(point);
        }
        f.buffer_pixels = 64.0;
        string_attr(&mut f, "class", class);
        string_attr(&mut f, "iata", source.tag("iata"));
        string_attr(&mut f, "icao", source.tag("icao"));
        add_names(source, &mut f);
        add_elevation(source, &mut f);
        out.push(f);
    }
}

fn display_housenumber(raw: &str) -> String {
    if !raw.contains(';') {
        return raw.into();
    }
    let parts: Vec<_> = raw
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return raw.into();
    }
    if raw.chars().all(|c| c.is_ascii_digit() || c == ';') {
        if let Ok(values) = parts
            .iter()
            .map(|s| s.parse::<u64>().map(|n| n as i64))
            .collect::<Result<Vec<_>, _>>()
        {
            return format!(
                "{}–{}",
                values.iter().min().unwrap(),
                values.iter().max().unwrap()
            );
        }
        return raw.into();
    }
    format!("{}–{}", parts[0], parts[parts.len() - 1])
}

fn housenumber(source: &Source, out: &mut Vec<Feature>) {
    if !source.has("addr:housenumber") || !(source.is_point() || source.is_polygon()) {
        return;
    }
    if let Some(mut f) = centroid_label_feature(source, "housenumber", 14) {
        let number = display_housenumber(source.tag("addr:housenumber"));
        f.buffer_pixels = 8.0;
        f.attrs.insert("housenumber".into(), json!(number));
        f.attrs.insert(
            "__housenumber_partition".into(),
            json!(format!(
                "{}{}{}",
                source.tag("addr:street"),
                source.tag("addr:block_number"),
                number
            )),
        );
        f.attrs.insert(
            "__housenumber_has_name".into(),
            json!(!source.tag("name").is_empty()),
        );
        out.push(f);
    }
}

fn poi_class<'a>(subclass: &'a str, key: &str) -> &'a str {
    if key == "amenity" && subclass == "university" {
        return "college";
    }
    if subclass == "station" {
        return if key == "railway" {
            "railway"
        } else if key == "aerialway" {
            "aerialway"
        } else {
            subclass
        };
    }
    if matches!(subclass, "halt" | "tram_stop" | "subway") {
        return "railway";
    }
    POI_CLASSES
        .iter()
        .find(|(_, values)| values.contains(&subclass))
        .map_or(subclass, |(class, _)| *class)
}

fn class_rank(class: &str) -> i64 {
    match class {
        "hospital" => 20,
        "railway" => 40,
        "bus" => 50,
        "attraction" => 70,
        "harbor" => 75,
        "college" => 80,
        "school" => 85,
        "stadium" => 90,
        "zoo" => 95,
        "town_hall" => 100,
        "campsite" => 110,
        "cemetery" => 115,
        "park" => 120,
        "library" => 130,
        "police" => 135,
        "post" => 140,
        "golf" => 150,
        "shop" => 400,
        "grocery" => 500,
        "fast_food" => 600,
        "clothing_store" => 700,
        "bar" => 800,
        _ => 1000,
    }
}

fn poi(source: &Source, out: &mut Vec<Feature>) {
    let filters = if source.is_point() {
        POI_POINT
    } else if source.is_polygon() {
        POI_POLYGON
    } else {
        return;
    };
    let Some((key, _)) = filters
        .iter()
        .find(|(key, values)| values.contains(&source.tag(key)))
    else {
        return;
    };
    let original = source.tag(key);
    let raw = if original == "station" && source.tag("station") == "subway" {
        "subway"
    } else if original == "station" && source.tag("funicular") == "yes" {
        "halt"
    } else {
        original
    };
    let z = if matches!(raw, "university" | "college") {
        10
    } else if (original == "station" && *key == "railway")
        || matches!(original, "halt" | "ferry_terminal")
    {
        12
    } else {
        14
    };
    let Some(mut f) = centroid_label_feature(source, "poi", z) else {
        return;
    };
    let mut name = source.tag("name").to_string();
    if name.is_empty() && raw == "atm" {
        name = if !source.tag("operator").is_empty() {
            source.tag("operator")
        } else {
            source.tag("network")
        }
        .into();
    }
    if name.is_empty() && matches!(raw, "charging_station" | "parcel_locker") {
        name = if !source.tag("brand").is_empty() {
            source.tag("brand")
        } else {
            source.tag("operator")
        }
        .into();
        if !source.tag("ref").is_empty() {
            if !name.is_empty() {
                name.push(' ');
            }
            name.push_str(source.tag("ref"));
        }
    }
    let class = poi_class(raw, key);
    string_attr(&mut f, "class", class);
    string_attr(
        &mut f,
        "subclass",
        match raw {
            "information" => source.tag("information"),
            "place_of_worship" => source.tag("religion"),
            "pitch" => source.tag("sport"),
            _ => raw,
        },
    );
    if source.tag("name").is_empty() && !name.is_empty() {
        let mut named = source.clone();
        named.tags.insert("name".into(), name.clone());
        add_names(&named, &mut f);
    } else {
        add_names(source, &mut f);
    }
    for key in ["layer", "level"] {
        if let Some(n) = parse_profile_long(source.tag(key))
            && (key == "level" || n != 0)
        {
            f.attrs.insert(key.into(), json!(n));
        }
    }
    // Tables.OsmPoiPoint/Polygon call SourceFeature.getBoolean, backed by
    // Imposm3Parsers.bool: every nonempty value except 0/false/no is true.
    if !matches!(source.tag("indoor"), "" | "0" | "false" | "no") {
        f.attrs.insert("indoor".into(), json!(1));
    }
    f.sort_key = class_rank(class) + if name.is_empty() { 2000 } else { 0 };
    f.buffer_pixels = 64.0;
    f.grid_size = 64.0;
    f.grid_max_zoom = 14;
    f.rank = true;
    if matches!(raw, "university" | "college") {
        f.min_size_source_geometry = Some(source.geometry.clone());
        f.attrs.insert("__university_min_size".into(), json!(80));
    }
    if source.is_point()
        && source.has("uic_ref")
        && matches!(
            original,
            "subway" | "tram_stop" | "bus_station" | "bus_stop"
        )
    {
        // Poi.finish emits these through SimpleFeature.fromWorldGeometry with
        // the raw node ID, so the tile ID is node * 10 without the node
        // suffix 1.
        f.id = source.osm_id as u64 * 10;
        f.attrs.insert(
            "__agg_stop_key".into(),
            json!(format!(
                "{}{}{}{}",
                source.tag("uic_ref"),
                source.tag("name"),
                source.tag("network"),
                source.tag("operator")
            )),
        );
        f.attrs.insert(
            "__agg_stop_order".into(),
            json!(
                ["subway", "tram_stop", "bus_station", "bus_stop"]
                    .iter()
                    .position(|s| *s == original)
                    .unwrap()
            ),
        );
    }
    out.push(f);
}

const POI_POINT: &[(&str, &[&str])] = &[
    ("aerialway", &["station"]),
    (
        "amenity",
        &[
            "arts_centre",
            "atm",
            "bank",
            "bar",
            "bbq",
            "bicycle_parking",
            "bicycle_rental",
            "biergarten",
            "bus_station",
            "cafe",
            "charging_station",
            "cinema",
            "clinic",
            "college",
            "community_centre",
            "courthouse",
            "dentist",
            "doctors",
            "drinking_water",
            "fast_food",
            "ferry_terminal",
            "fire_station",
            "food_court",
            "fuel",
            "grave_yard",
            "hospital",
            "ice_cream",
            "kindergarten",
            "library",
            "marketplace",
            "motorcycle_parking",
            "nightclub",
            "nursing_home",
            "parking",
            "pharmacy",
            "place_of_worship",
            "police",
            "parcel_locker",
            "post_box",
            "post_office",
            "prison",
            "pub",
            "public_building",
            "recycling",
            "restaurant",
            "school",
            "shelter",
            "swimming_pool",
            "taxi",
            "telephone",
            "theatre",
            "toilets",
            "townhall",
            "university",
            "veterinary",
            "waste_basket",
        ],
    ),
    (
        "barrier",
        &[
            "bollard",
            "border_control",
            "cycle_barrier",
            "gate",
            "lift_gate",
            "sally_port",
            "stile",
            "toll_booth",
        ],
    ),
    ("building", &["dormitory"]),
    ("highway", &["bus_stop"]),
    ("historic", &["monument", "castle", "ruins"]),
    (
        "landuse",
        &[
            "basin",
            "brownfield",
            "cemetery",
            "reservoir",
            "winter_sports",
        ],
    ),
    (
        "leisure",
        &[
            "dog_park",
            "escape_game",
            "garden",
            "golf_course",
            "ice_rink",
            "hackerspace",
            "marina",
            "miniature_golf",
            "park",
            "pitch",
            "playground",
            "sports_centre",
            "stadium",
            "swimming_area",
            "swimming_pool",
            "water_park",
        ],
    ),
    (
        "office",
        &[
            "accountant",
            "advertising_agency",
            "architect",
            "association",
            "bail_bond_agent",
            "charity",
            "company",
            "construction_company",
            "consulting",
            "cooperative",
            "courier",
            "coworking",
            "diplomatic",
            "educational_institution",
            "employment_agency",
            "energy_supplier",
            "engineer",
            "estate_agent",
            "financial",
            "financial_advisor",
            "forestry",
            "foundation",
            "geodesist",
            "government",
            "graphic_design",
            "guide",
            "harbour_master",
            "health_insurance",
            "insurance",
            "interior_design",
            "it",
            "lawyer",
            "logistics",
            "marketing",
            "moving_company",
            "newspaper",
            "ngo",
            "notary",
            "physician",
            "political_party",
            "private_investigator",
            "property_management",
            "publisher",
            "quango",
            "religion",
            "research",
            "security",
            "surveyor",
            "tax_advisor",
            "taxi",
            "telecommunication",
            "therapist",
            "translator",
            "travel_agent",
            "tutoring",
            "union",
            "university",
            "water_utility",
            "web_design",
            "wedding_planner",
        ],
    ),
    (
        "railway",
        &[
            "halt",
            "station",
            "subway_entrance",
            "train_station_entrance",
            "tram_stop",
        ],
    ),
    (
        "shop",
        &[
            "accessories",
            "alcohol",
            "antiques",
            "art",
            "bag",
            "bakery",
            "beauty",
            "bed",
            "beverages",
            "bicycle",
            "books",
            "boutique",
            "butcher",
            "camera",
            "car",
            "car_repair",
            "car_parts",
            "carpet",
            "charity",
            "chemist",
            "chocolate",
            "clothes",
            "coffee",
            "computer",
            "confectionery",
            "convenience",
            "copyshop",
            "cosmetics",
            "deli",
            "delicatessen",
            "department_store",
            "doityourself",
            "dry_cleaning",
            "electronics",
            "erotic",
            "fabric",
            "florist",
            "frozen_food",
            "furniture",
            "garden_centre",
            "general",
            "gift",
            "greengrocer",
            "hairdresser",
            "hardware",
            "hearing_aids",
            "hifi",
            "ice_cream",
            "interior_decoration",
            "jewelry",
            "kiosk",
            "lamps",
            "laundry",
            "locksmith",
            "mall",
            "massage",
            "mobile_phone",
            "motorcycle",
            "music",
            "musical_instrument",
            "newsagent",
            "optician",
            "outdoor",
            "paint",
            "perfume",
            "perfumery",
            "pet",
            "photo",
            "second_hand",
            "shoes",
            "sports",
            "stationery",
            "supermarket",
            "tailor",
            "tattoo",
            "ticket",
            "tobacco",
            "toys",
            "travel_agency",
            "video",
            "video_games",
            "watches",
            "weapons",
            "wholesale",
            "wine",
        ],
    ),
    (
        "sport",
        &[
            "american_football",
            "archery",
            "athletics",
            "australian_football",
            "badminton",
            "baseball",
            "basketball",
            "beachvolleyball",
            "billiards",
            "bmx",
            "boules",
            "bowls",
            "boxing",
            "canadian_football",
            "canoe",
            "chess",
            "climbing",
            "climbing_adventure",
            "cricket",
            "cricket_nets",
            "croquet",
            "curling",
            "cycling",
            "disc_golf",
            "diving",
            "dog_racing",
            "equestrian",
            "fatsal",
            "field_hockey",
            "free_flying",
            "gaelic_games",
            "golf",
            "gymnastics",
            "handball",
            "hockey",
            "horse_racing",
            "horseshoes",
            "ice_hockey",
            "ice_stock",
            "judo",
            "karting",
            "korfball",
            "long_jump",
            "model_aerodrome",
            "motocross",
            "motor",
            "multi",
            "netball",
            "orienteering",
            "paddle_tennis",
            "paintball",
            "paragliding",
            "pelota",
            "racquet",
            "rc_car",
            "rowing",
            "rugby",
            "rugby_league",
            "rugby_union",
            "running",
            "sailing",
            "scuba_diving",
            "shooting",
            "shooting_range",
            "skateboard",
            "skating",
            "skiing",
            "soccer",
            "surfing",
            "swimming",
            "table_soccer",
            "table_tennis",
            "team_handball",
            "tennis",
            "toboggan",
            "volleyball",
            "water_ski",
            "yoga",
        ],
    ),
    (
        "tourism",
        &[
            "alpine_hut",
            "aquarium",
            "artwork",
            "attraction",
            "bed_and_breakfast",
            "camp_site",
            "caravan_site",
            "chalet",
            "gallery",
            "guest_house",
            "hostel",
            "hotel",
            "information",
            "motel",
            "museum",
            "picnic_site",
            "theme_park",
            "viewpoint",
            "zoo",
        ],
    ),
    ("waterway", &["dock"]),
];

const POI_POLYGON: &[(&str, &[&str])] = &[
    ("aerialway", &["station"]),
    (
        "amenity",
        &[
            "arts_centre",
            "atm",
            "bank",
            "bar",
            "bbq",
            "bicycle_parking",
            "bicycle_rental",
            "biergarten",
            "bus_station",
            "cafe",
            "charging_station",
            "cinema",
            "clinic",
            "college",
            "community_centre",
            "courthouse",
            "dentist",
            "doctors",
            "drinking_water",
            "fast_food",
            "ferry_terminal",
            "fire_station",
            "food_court",
            "fuel",
            "grave_yard",
            "hospital",
            "ice_cream",
            "kindergarten",
            "library",
            "marketplace",
            "motorcycle_parking",
            "nightclub",
            "nursing_home",
            "parking",
            "pharmacy",
            "place_of_worship",
            "police",
            "parcel_locker",
            "post_box",
            "post_office",
            "prison",
            "pub",
            "public_building",
            "recycling",
            "restaurant",
            "school",
            "shelter",
            "swimming_pool",
            "taxi",
            "telephone",
            "theatre",
            "toilets",
            "townhall",
            "university",
            "veterinary",
            "waste_basket",
        ],
    ),
    (
        "barrier",
        &[
            "bollard",
            "border_control",
            "cycle_barrier",
            "gate",
            "lift_gate",
            "sally_port",
            "stile",
            "toll_booth",
        ],
    ),
    ("building", &["dormitory"]),
    ("highway", &["bus_stop"]),
    ("historic", &["monument", "castle", "ruins"]),
    (
        "landuse",
        &[
            "basin",
            "brownfield",
            "cemetery",
            "reservoir",
            "winter_sports",
        ],
    ),
    (
        "leisure",
        &[
            "dog_park",
            "escape_game",
            "garden",
            "golf_course",
            "ice_rink",
            "hackerspace",
            "marina",
            "miniature_golf",
            "park",
            "pitch",
            "playground",
            "sports_centre",
            "stadium",
            "swimming_area",
            "swimming_pool",
            "water_park",
        ],
    ),
    (
        "office",
        &[
            "accountant",
            "advertising_agency",
            "architect",
            "association",
            "bail_bond_agent",
            "charity",
            "company",
            "construction_company",
            "consulting",
            "cooperative",
            "courier",
            "coworking",
            "diplomatic",
            "educational_institution",
            "employment_agency",
            "energy_supplier",
            "engineer",
            "estate_agent",
            "financial",
            "financial_advisor",
            "forestry",
            "foundation",
            "geodesist",
            "government",
            "graphic_design",
            "guide",
            "harbour_master",
            "health_insurance",
            "insurance",
            "interior_design",
            "it",
            "lawyer",
            "logistics",
            "marketing",
            "moving_company",
            "newspaper",
            "ngo",
            "notary",
            "physician",
            "political_party",
            "private_investigator",
            "property_management",
            "publisher",
            "quango",
            "religion",
            "research",
            "security",
            "surveyor",
            "tax_advisor",
            "taxi",
            "telecommunication",
            "therapist",
            "translator",
            "travel_agent",
            "tutoring",
            "union",
            "university",
            "water_utility",
            "web_design",
            "wedding_planner",
        ],
    ),
    (
        "railway",
        &[
            "halt",
            "station",
            "subway_entrance",
            "train_station_entrance",
            "tram_stop",
        ],
    ),
    (
        "shop",
        &[
            "accessories",
            "alcohol",
            "antiques",
            "art",
            "bag",
            "bakery",
            "beauty",
            "bed",
            "beverages",
            "bicycle",
            "books",
            "boutique",
            "butcher",
            "camera",
            "car",
            "car_repair",
            "car_parts",
            "carpet",
            "charity",
            "chemist",
            "chocolate",
            "clothes",
            "coffee",
            "computer",
            "confectionery",
            "convenience",
            "copyshop",
            "cosmetics",
            "deli",
            "delicatessen",
            "department_store",
            "doityourself",
            "dry_cleaning",
            "electronics",
            "erotic",
            "fabric",
            "florist",
            "frozen_food",
            "furniture",
            "garden_centre",
            "general",
            "gift",
            "greengrocer",
            "hairdresser",
            "hardware",
            "hearing_aids",
            "hifi",
            "ice_cream",
            "interior_decoration",
            "jewelry",
            "kiosk",
            "lamps",
            "laundry",
            "locksmith",
            "mall",
            "massage",
            "mobile_phone",
            "motorcycle",
            "music",
            "musical_instrument",
            "newsagent",
            "optician",
            "outdoor",
            "paint",
            "perfume",
            "perfumery",
            "pet",
            "photo",
            "second_hand",
            "shoes",
            "sports",
            "stationery",
            "supermarket",
            "tailor",
            "tattoo",
            "ticket",
            "tobacco",
            "toys",
            "travel_agency",
            "video",
            "video_games",
            "watches",
            "weapons",
            "wholesale",
            "wine",
        ],
    ),
    (
        "sport",
        &[
            "american_football",
            "archery",
            "athletics",
            "australian_football",
            "badminton",
            "baseball",
            "basketball",
            "beachvolleyball",
            "billiards",
            "bmx",
            "boules",
            "bowls",
            "boxing",
            "canadian_football",
            "canoe",
            "chess",
            "climbing",
            "climbing_adventure",
            "cricket",
            "cricket_nets",
            "croquet",
            "curling",
            "cycling",
            "disc_golf",
            "diving",
            "dog_racing",
            "equestrian",
            "fatsal",
            "field_hockey",
            "free_flying",
            "gaelic_games",
            "golf",
            "gymnastics",
            "handball",
            "hockey",
            "horse_racing",
            "horseshoes",
            "ice_hockey",
            "ice_stock",
            "judo",
            "karting",
            "korfball",
            "long_jump",
            "model_aerodrome",
            "motocross",
            "motor",
            "multi",
            "netball",
            "orienteering",
            "paddle_tennis",
            "paintball",
            "paragliding",
            "pelota",
            "racquet",
            "rc_car",
            "rowing",
            "rugby",
            "rugby_league",
            "rugby_union",
            "running",
            "sailing",
            "scuba_diving",
            "shooting",
            "shooting_range",
            "skateboard",
            "skating",
            "skiing",
            "soccer",
            "surfing",
            "swimming",
            "table_soccer",
            "table_tennis",
            "team_handball",
            "tennis",
            "toboggan",
            "volleyball",
            "water_ski",
            "yoga",
        ],
    ),
    (
        "tourism",
        &[
            "alpine_hut",
            "aquarium",
            "artwork",
            "attraction",
            "bed_and_breakfast",
            "camp_site",
            "caravan_site",
            "chalet",
            "gallery",
            "guest_house",
            "hostel",
            "hotel",
            "information",
            "motel",
            "museum",
            "picnic_site",
            "theme_park",
            "viewpoint",
            "zoo",
        ],
    ),
    ("waterway", &["dock"]),
];

const POI_CLASSES: &[(&str, &[&str])] = &[
    (
        "shop",
        &[
            "accessories",
            "antiques",
            "beauty",
            "bed",
            "boutique",
            "camera",
            "carpet",
            "charity",
            "chemist",
            "chocolate",
            "coffee",
            "computer",
            "convenience",
            "confectionery",
            "copyshop",
            "cosmetics",
            "garden_centre",
            "doityourself",
            "erotic",
            "electronics",
            "fabric",
            "florist",
            "frozen_food",
            "furniture",
            "video_games",
            "video",
            "general",
            "gift",
            "hardware",
            "hearing_aids",
            "hifi",
            "interior_decoration",
            "jewelry",
            "kiosk",
            "locksmith",
            "lamps",
            "mall",
            "massage",
            "motorcycle",
            "mobile_phone",
            "newsagent",
            "optician",
            "outdoor",
            "paint",
            "perfumery",
            "perfume",
            "pet",
            "photo",
            "second_hand",
            "shoes",
            "sports",
            "stationery",
            "tailor",
            "tattoo",
            "ticket",
            "tobacco",
            "toys",
            "travel_agency",
            "watches",
            "weapons",
            "wholesale",
        ],
    ),
    (
        "office",
        &[
            "accountant",
            "advertising_agency",
            "architect",
            "association",
            "bail_bond_agent",
            "charity",
            "company",
            "construction_company",
            "consulting",
            "cooperative",
            "courier",
            "coworking",
            "diplomatic",
            "educational_institution",
            "employment_agency",
            "energy_supplier",
            "engineer",
            "estate_agent",
            "financial",
            "financial_advisor",
            "forestry",
            "foundation",
            "geodesist",
            "government",
            "graphic_design",
            "guide",
            "harbour_master",
            "health_insurance",
            "insurance",
            "interior_design",
            "it",
            "lawyer",
            "logistics",
            "marketing",
            "moving_company",
            "newspaper",
            "ngo",
            "notary",
            "physician",
            "political_party",
            "private_investigator",
            "property_management",
            "publisher",
            "quango",
            "religion",
            "research",
            "security",
            "surveyor",
            "tax_advisor",
            "taxi",
            "telecommunication",
            "therapist",
            "translator",
            "travel_agent",
            "tutoring",
            "union",
            "university",
            "water_utility",
            "web_design",
            "wedding_planner",
        ],
    ),
    (
        "town_hall",
        &[
            "townhall",
            "public_building",
            "courthouse",
            "community_centre",
        ],
    ),
    ("golf", &["golf", "golf_course", "miniature_golf"]),
    ("fast_food", &["fast_food", "food_court"]),
    ("park", &["park", "bbq"]),
    ("bus", &["bus_stop", "bus_station"]),
    ("entrance", &["subway_entrance", "train_station_entrance"]),
    ("campsite", &["camp_site", "caravan_site"]),
    ("laundry", &["laundry", "dry_cleaning"]),
    (
        "grocery",
        &[
            "supermarket",
            "deli",
            "delicatessen",
            "department_store",
            "greengrocer",
            "marketplace",
        ],
    ),
    ("library", &["books", "library"]),
    ("college", &["university", "college"]),
    (
        "lodging",
        &[
            "hotel",
            "motel",
            "bed_and_breakfast",
            "guest_house",
            "hostel",
            "chalet",
            "alpine_hut",
            "dormitory",
        ],
    ),
    ("ice_cream", &["ice_cream"]),
    ("post", &["post_box", "post_office", "parcel_locker"]),
    ("cafe", &["cafe"]),
    ("school", &["school", "kindergarten"]),
    ("alcohol_shop", &["alcohol", "beverages", "wine"]),
    ("bar", &["bar", "nightclub"]),
    ("harbor", &["marina", "dock"]),
    ("car", &["car", "car_repair", "car_parts", "taxi"]),
    ("hospital", &["hospital", "nursing_home", "clinic"]),
    ("cemetery", &["grave_yard", "cemetery"]),
    ("attraction", &["attraction", "viewpoint"]),
    ("beer", &["biergarten", "pub"]),
    ("music", &["music", "musical_instrument"]),
    ("stadium", &["american_football", "stadium", "soccer"]),
    ("art_gallery", &["art", "artwork", "gallery", "arts_centre"]),
    ("clothing_store", &["bag", "clothes"]),
    ("swimming", &["swimming_area", "swimming"]),
    ("castle", &["castle", "ruins"]),
    ("atm", &["atm"]),
    ("fuel", &["fuel", "charging_station"]),
];

#[cfg(test)]
mod tests {
    #[test]
    fn park_outline_projection_flag_excludes_supplements_and_labels() {
        for kind in [
            SourceKind::Way,
            SourceKind::Relation,
            SourceKind::Supplement,
        ] {
            let mut s = source(&[("boundary", "national_park"), ("name", "Example")]);
            s.kind = kind;
            s.geometry = Geometry::Polygon(geo_types::Polygon::new(
                geo_types::LineString::from(vec![
                    (19., 50.),
                    (19.1, 50.),
                    (19.1, 50.1),
                    (19., 50.),
                ]),
                vec![],
            ));
            let mut out = vec![];
            park(&s, &mut out);
            let outline = out
                .iter()
                .find(|f| matches!(f.geometry, Geometry::Polygon(_)))
                .unwrap();
            assert_eq!(
                outline.osm_outline_projection,
                kind != SourceKind::Supplement
            );
            assert!(
                out.iter()
                    .filter(|f| matches!(f.geometry, Geometry::Point(_)))
                    .all(|f| !f.osm_outline_projection)
            );
        }
    }

    #[test]
    fn quantization_only_applies_to_original_osm_sources() {
        let mut s = source(&[("amenity", "school")]);
        s.geometry = Geometry::Polygon(geo_types::Polygon::new(
            geo_types::LineString::from(vec![
                (19.0000001, 50.0000001),
                (19.0003, 50.0000001),
                (19.0003, 50.0002),
                (19.0000001, 50.0000001),
            ]),
            vec![],
        ));
        s.kind = SourceKind::Supplement;
        assert!(!quantize_label_source(&s));
        let original =
            super::super::interior_point::point_then_centroid_if_convex(&s.geometry).unwrap();
        assert_eq!(
            centroid_label_feature(&s, "poi", 14).unwrap().geometry,
            Geometry::Point(original)
        );
        for kind in [SourceKind::Node, SourceKind::Way, SourceKind::Relation] {
            s.kind = kind;
            assert!(quantize_label_source(&s));
        }
    }

    #[test]
    fn city_accents_follow_commons_lang_320_policy() {
        assert_eq!(strip_city_accents("Łódź Đđ Ɨɨ Ʉʉ Ŧŧ"), "Lodz Dd Ii Uu Tt");
        assert_eq!(strip_city_accents("é é ﬁ ² ¼"), "e e fi 2 1⁄4");
        assert_eq!(strip_city_accents("A᪰ ø ß"), "A᪰ ø ß");
        assert_eq!(strip_city_accents(""), "");
    }
    #[test]
    fn natural_earth_city_accent_fallback_changes_rank_not_display_name() {
        let mut state = State::default();
        state.cities.push(City {
            world: world_coord(Coord { x: 19., y: 50. }),
            name: "Lodz".into(),
            names: vec![],
            wikidata: "Q-reference".into(),
            rank: 3,
        });
        let mut output = vec![];
        state.process(&source(&[("place", "city"), ("name", "Łódź")]), &mut output);
        let place = output.iter().find(|f| f.layer == "place").unwrap();
        assert_eq!(place.attrs["rank"], json!(4));
        assert_eq!(place.min_zoom, 3);
        assert_eq!(place.attrs["name"], json!("Łódź"));
        output.clear();
        state.process(&source(&[("place", "city"), ("name", "łódź")]), &mut output);
        let place = output.iter().find(|f| f.layer == "place").unwrap();
        assert!(!place.attrs.contains_key("rank"));
        assert_eq!(place.min_zoom, 7);
    }

    use super::super::types::SourceKind;
    use super::*;
    fn source(tags: &[(&str, &str)]) -> Source {
        Source {
            id: 1,
            osm_id: 1,
            kind: SourceKind::Node,
            source_layer: String::new(),
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            geometry: Geometry::Point(geo_types::Point::new(19.0, 50.0)),
            relations: vec![],
        }
    }
    fn feature(tags: &[(&str, &str)], layer: &str) -> Feature {
        let mut out = vec![];
        process(&source(tags), &mut out);
        out.into_iter()
            .find(|f| f.layer == layer)
            .expect("expected source classification")
    }

    // Cases translated from pinned HousenumberTest.testDisplayHousenumber.
    #[test]
    fn housenumber_ranges_match_upstream() {
        for (raw, want) in [
            ("1;1a;2;2/b;20;3;", "1–3"),
            ("1;2;20;3", "1–20"),
            (";", ";"),
            (";;", ";;"),
            ("2712;935803935803", "2712–935803935803"),
            ("1; 2;20;3", "1–3"),
        ] {
            assert_eq!(display_housenumber(raw), want);
        }
        let f = feature(
            &[
                ("addr:housenumber", "765/6"),
                ("addr:block_number", "X"),
                ("addr:street", "street"),
                ("name", "name"),
            ],
            "housenumber",
        );
        assert_eq!(f.attrs["__housenumber_partition"], json!("streetX765/6"));
        assert_eq!(f.attrs["__housenumber_has_name"], json!(true));
    }

    #[test]
    fn aerodrome_class_and_elevation_match_upstream() {
        let f = feature(
            &[
                ("aeroway", "aerodrome"),
                ("aerodrome", "international"),
                ("iata", "123"),
                ("ele", "328' 1\""),
            ],
            "aerodrome_label",
        );
        assert_eq!(f.min_zoom, 8);
        assert_eq!(f.attrs["ele"], json!(100));
        assert_eq!(f.attrs["ele_ft"], json!(328));
        for (value, class) in [
            ("public airport", "public"),
            ("civil", "public"),
            ("military airport", "military"),
            ("private", "private"),
        ] {
            let f = feature(
                &[("aeroway", "aerodrome"), ("aerodrome_type", value)],
                "aerodrome_label",
            );
            assert_eq!(f.attrs["class"], json!(class));
            assert_eq!(f.min_zoom, 10);
        }
    }

    #[test]
    fn elevation_parser_preserves_reference_search_and_unit_semantics() {
        // Actual baseline mismatch at z8/141/86, MVT id 937594972 (Krakow airport).
        let f = feature(
            &[("aeroway", "aerodrome"), ("ele", "241,4")],
            "aerodrome_label",
        );
        assert_eq!(f.attrs["ele"], json!(4));
        assert_eq!(f.attrs["ele_ft"], json!(13));
        for (raw, expected) in [
            ("1.2 km", 1200.0),
            ("1 MI", 1609.344),
            ("1 nautical mile", 1852.0),
            ("12 in", 0.3048),
            ("bad prefix 42m", 42.0),
        ] {
            assert!((elevation_meters(&source(&[("ele", raw)])).unwrap() - expected).abs() < 1e-9);
        }
        assert_eq!(elevation_meters(&source(&[("ele", "241..4")])), None);
    }

    #[test]
    fn poi_station_and_office_collision_match_upstream() {
        let f = feature(
            &[
                ("railway", "station"),
                ("funicular", "yes"),
                ("name", "station"),
            ],
            "poi",
        );
        assert_eq!(f.attrs["subclass"], json!("halt"));
        assert_eq!(f.attrs["class"], json!("railway"));
        assert_eq!(f.min_zoom, 12);
        assert_eq!(
            feature(&[("office", "university")], "poi").attrs["class"],
            json!("office")
        );
        assert_eq!(
            feature(&[("amenity", "university")], "poi").attrs["class"],
            json!("college")
        );
    }

    #[test]
    fn poi_generated_filter_rejects_unknown_shop() {
        let mut out = vec![];
        process(&source(&[("shop", "not_an_omt_shop")]), &mut out);
        assert!(out.is_empty());
        let f = feature(
            &[
                ("shop", "supermarket"),
                ("layer", "0"),
                ("level", "-1"),
                ("indoor", "yes"),
            ],
            "poi",
        );
        assert_eq!(f.attrs["class"], json!("grocery"));
        assert!(!f.attrs.contains_key("layer"));
        assert_eq!(f.attrs["level"], json!(-1));
        assert_eq!(f.attrs["indoor"], json!(1));
    }

    #[test]
    fn poi_actual_osm_indoor_and_level_tags_follow_pinned_parsers() {
        // malopolskie.osm.pbf (SHA-256 4c3a6b117c6d1e79f7fbaae4efc8d8565866a3ea1c075488fb04746acec1c971):
        // way 1385129263 is a deli with indoor=shop; OMT getBoolean treats every
        // nonempty value except 0/false/no as true.
        let deli = feature(
            &[("shop", "deli"), ("indoor", "shop"), ("level", "0")],
            "poi",
        );
        assert_eq!(deli.attrs["indoor"], json!(1));
        assert_eq!(deli.attrs["level"], json!(0));

        // node 2331362477 is a restaurant with level=0;1. Pinned
        // Parse.parseLongOrNull accepts the numeric prefix and emits 0.
        let restaurant = feature(&[("amenity", "restaurant"), ("level", "0;1")], "poi");
        assert_eq!(restaurant.attrs["level"], json!(0));

        // node 3040858953 is a restaurant with level=-0.5; NumberFormat's
        // parsed value truncates toward zero when converted to long.
        let half_level = feature(&[("amenity", "restaurant"), ("level", "-0.5")], "poi");
        assert_eq!(half_level.attrs["level"], json!(0));

        // Imposm3Parsers.bool has exactly four false inputs; all other
        // nonempty values, including the actual indoor=shop tag, are true.
        for raw in ["", "0", "false", "no"] {
            let poi = feature(&[("shop", "deli"), ("indoor", raw)], "poi");
            assert!(!poi.attrs.contains_key("indoor"), "indoor={raw:?}");
        }
    }

    #[test]
    fn poi_fallback_name_has_language_aliases() {
        let f = feature(&[("amenity", "atm"), ("operator", "Example Bank")], "poi");
        assert_eq!(f.attrs["name"], json!("Example Bank"));
        assert_eq!(f.attrs["name_en"], json!("Example Bank"));
        assert_eq!(f.attrs["name_int"], json!("Example Bank"));
    }

    #[test]
    fn agg_stop_prefers_tram_then_nearest_and_id() {
        let mut state = State::default();
        let mut out = vec![];
        for (id, key, value, x) in [
            (3, "highway", "bus_stop", 19.0),
            (2, "railway", "tram_stop", 19.2),
            (1, "railway", "tram_stop", 19.0),
        ] {
            let mut s = source(&[(key, value), ("name", "station"), ("uic_ref", "1")]);
            s.osm_id = id;
            s.id = id as u64 * 10 + 1;
            s.geometry = Geometry::Point(geo_types::Point::new(x, 50.0));
            state.process(&s, &mut out);
        }
        assert!(out.is_empty());
        state.finish(&mut out);
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.iter()
                .find(|f| f.attrs.contains_key("agg_stop"))
                .unwrap()
                .id,
            10
        );
        // Berlin bus stops: Planetiler's agg_stop IDs end in 0.
        let mut ids: Vec<u64> = out.iter().map(|f| f.id).collect();
        ids.sort_unstable();
        assert_eq!(ids, [10, 20, 30]);
        assert!(
            out.iter()
                .all(|f| !f.attrs.contains_key("__agg_stop_order"))
        );
    }

    #[test]
    fn place_zoom_fallbacks_and_capital() {
        for (kind, z) in [
            ("city", 7),
            ("village", 8),
            ("hamlet", 11),
            ("borough", 14),
            ("quarter", 14),
            ("island", 12),
        ] {
            let f = feature(
                &[("place", kind), ("name", "Town"), ("capital", "yes")],
                "place",
            );
            assert_eq!(f.min_zoom, z);
        }
        let f = feature(&[("place", "continent"), ("name", "Europe")], "place");
        assert_eq!((f.min_zoom, f.max_zoom), (0, 3));
        assert_eq!(f.attrs["rank"], json!(1));
        let f = feature(
            &[
                ("place", "country"),
                ("name", "Poland"),
                ("ISO3166-1:alpha2", "PL"),
            ],
            "place",
        );
        assert_eq!(f.attrs["rank"], json!(6));
        assert_eq!(f.min_zoom, 5);
    }

    #[test]
    fn peak_selection_and_grid_policy() {
        let f = feature(&[("natural", "peak"), ("ele", "9999.9")], "mountain_peak");
        assert_eq!(f.attrs["ele"], json!(10000));
        assert_eq!(f.sort_key, -9999);
        assert_eq!(f.grid_limit, 5);
        assert_eq!(f.grid_max_zoom, 13);
        let mut out = vec![];
        process(&source(&[("natural", "peak"), ("ele", "10000")]), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn names_keep_latin_accents_and_clean_nonlatin() {
        let s = source(&[
            ("name", "Łódź"),
            ("name:pl", "Łódź"),
            ("name:unknown", "hidden"),
        ]);
        let mut f = Feature::new(&s, "poi", s.geometry.clone(), 14, 14);
        add_profile_names(&s, &mut f);
        assert_eq!(f.attrs["name:latin"], json!("Łódź"));
        assert!(!f.attrs.contains_key("name:nonlatin"));
        assert!(!f.attrs.contains_key("name:unknown"));
        let s = source(&[("name", "東京 (Tokyo)"), ("name:en", "Tokyo")]);
        let mut f = Feature::new(&s, "poi", s.geometry.clone(), 14, 14);
        add_profile_names(&s, &mut f);
        assert_eq!(f.attrs["name:nonlatin"], json!("東京"));
        assert_eq!(f.attrs["name:latin"], json!("Tokyo"));
        assert_eq!(f.attrs["name_de"], json!("東京 (Tokyo)"));
    }

    #[test]
    fn latin_fallback_follows_java_hash_map_order() {
        // JDK 21 iterates these seven tags as name:sk, name, name:it,
        // name:fr, name:pl, place, name:cs, so Planetiler picks name:sk where
        // key order would pick name:cs.
        let s = source(&[
            ("name", "Краків"),
            ("name:cs", "Krakov"),
            ("name:fr", "Cracovie"),
            ("name:it", "Cracovia"),
            ("name:pl", "Kraków"),
            ("name:sk", "Krakova"),
            ("place", "city"),
        ]);
        let mut f = Feature::new(&s, "place", s.geometry.clone(), 14, 14);
        add_profile_names(&s, &mut f);
        assert_eq!(f.attrs["name:latin"], json!("Krakova"));
        assert_eq!(f.attrs["name_int"], json!("Krakova"));
    }

    #[test]
    fn compiled_script_transliteration_matches_pinned_any_latin_corpus() {
        // Expected outputs from Planetiler 0.10.2 JAR SHA-256
        // f310bd0413e2e4512b27f4046d418664e8e1d3bf31603c2a70e23de06c167e4d.
        // The first two names are the remaining POI attribute discrepancies
        // in the quality43 archives (two adjacent tiles per source feature).
        for (name, expected) in [
            ("ДП \"Документ\"", Some("DP \"Dokument\"")),
            (
                "MU Restaurant Krakow 무대륙",
                Some("MU Restaurant Krakow mudaelyug"),
            ),
            ("Київ", Some("Kií̈v")),
            ("Москва", Some("Moskva")),
            ("България", Some("Bʺlgariâ")),
            ("한글", Some("hangeul")),
            ("서울", Some("seoul")),
            ("대한민국", Some("daehanmingug")),
            ("Αθήνα", Some("Athḗna")),
            ("ДП 무대륙", Some("DP mudaelyug")),
            ("東京", Some("dōng jīng")),
            ("مرحبا", Some("mrḥbạ")),
            ("हिन्दी", Some("hindī")),
            ("བོད", None),
            ("ДП བོད", None),
            ("Latin \"name\" — Łódź", Some("Latin \"name\" — Łódź")),
            // Berlin POIs: Common-script letters that Any-Latin keeps.
            ("Thobenʼs Backwaren", Some("Thobenʼs Backwaren")),
            ("Café µ", Some("Café µ")),
        ] {
            assert_eq!(transliterate_name(name).as_deref(), expected, "{name}");
        }
    }

    #[test]
    fn unchanged_transliteration_is_still_name_latin() {
        // Berlin POI "Café µ": Planetiler writes name:latin and name_int from
        // Any-Latin's unchanged result, and name:nonlatin "µ".
        let cafe = source(&[("name", "Café µ")]);
        let mut poi = Feature::new(&cafe, "poi", cafe.geometry.clone(), 14, 14);
        add_profile_names(&cafe, &mut poi);
        assert_eq!(poi.attrs["name:latin"], json!("Café µ"));
        assert_eq!(poi.attrs["name_int"], json!("Café µ"));
        assert_eq!(poi.attrs["name:nonlatin"], json!("µ"));
    }

    #[test]
    fn transliteration_fallback_keeps_existing_latin_name_precedence() {
        let first = source(&[("name", "ДП \"Документ\"")]);
        let mut poi = Feature::new(&first, "poi", first.geometry.clone(), 14, 14);
        add_profile_names(&first, &mut poi);
        assert_eq!(poi.attrs["name:latin"], json!("DP \"Dokument\""));
        assert_eq!(poi.attrs["name_int"], json!("DP \"Dokument\""));
        assert_eq!(poi.attrs["name:nonlatin"], json!("ДП \"Документ\""));

        let second = source(&[("name", "MU Restaurant Krakow 무대륙")]);
        let mut poi = Feature::new(&second, "poi", second.geometry.clone(), 14, 14);
        add_profile_names(&second, &mut poi);
        assert_eq!(
            poi.attrs["name:latin"],
            json!("MU Restaurant Krakow mudaelyug")
        );
        assert_eq!(
            poi.attrs["name_int"],
            json!("MU Restaurant Krakow mudaelyug")
        );

        let with_en = source(&[("name", "ДП \"Документ\""), ("name:en", "Document")]);
        let mut poi = Feature::new(&with_en, "poi", with_en.geometry.clone(), 14, 14);
        add_profile_names(&with_en, &mut poi);
        assert_eq!(poi.attrs["name:latin"], json!("Document"));

        let unsupported = source(&[("name", "བོད")]);
        let mut poi = Feature::new(&unsupported, "poi", unsupported.geometry.clone(), 14, 14);
        add_profile_names(&unsupported, &mut poi);
        assert!(!poi.attrs.contains_key("name:latin"));
    }

    #[test]
    fn natural_earth_country_rank_joins_before_osm() {
        let mut state = State::default();
        let mut out = vec![];
        let mut region = source(&[("name", "Poland"), ("scalerank", "0"), ("labelrank", "2")]);
        region.source_layer = "ne_10m_admin_0_countries".into();
        region.geometry =
            geo_types::Rect::new(Coord { x: 18.0, y: 49.0 }, Coord { x: 20.0, y: 51.0 })
                .to_polygon()
                .into();
        state.process(&region, &mut out);
        state.process(
            &source(&[
                ("place", "country"),
                ("name", "Polska"),
                ("ISO3166-1:alpha2", "PL"),
            ]),
            &mut out,
        );
        assert_eq!(out[0].attrs["rank"], json!(1));
        assert_eq!(out[0].min_zoom, 0);
    }

    #[test]
    fn park_has_polygon_and_label_with_distinct_zoom_attributes() {
        let mut s = source(&[
            ("boundary", "national_park"),
            ("name", "National Park"),
            ("name:pl", "Park Narodowy"),
        ]);
        s.geometry = geo_types::Rect::new(Coord { x: 19.0, y: 50.0 }, Coord { x: 19.1, y: 50.1 })
            .to_polygon()
            .into();
        let mut out = vec![];
        process(&s, &mut out);
        let shape = out
            .iter()
            .find(|f| f.layer == "park" && matches!(f.geometry, Geometry::Polygon(_)))
            .unwrap();
        assert_eq!(shape.min_zoom, 4);
        assert_eq!(shape.attr_minzoom["class"], 5);
        assert_eq!(shape.attr_minzoom["name"], 5);
        assert!(!shape.attrs.contains_key("name:pl"));
        let point = out
            .iter()
            .find(|f| f.layer == "park" && matches!(f.geometry, Geometry::Point(_)))
            .unwrap();
        assert_eq!(point.grid_size, 100.0);
        assert_eq!(point.attrs["name:pl"], json!("Park Narodowy"));
        assert!((5..=14).contains(&point.min_zoom));
    }
}
