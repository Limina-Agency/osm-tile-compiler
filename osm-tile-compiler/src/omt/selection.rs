//! Which layers and `name:<lang>` attributes a compile writes.
//!
//! The profile still evaluates every source in full, since layers read one
//! another's state (water names come from water polygons, road names from the
//! road graph, park labels from park outlines). The selection applies to each
//! feature only once it is evaluated, before it is rendered: a feature
//! belongs to one layer and the tile encoder post-processes each layer on its
//! own, so a selected layer comes out as it would in a full build.
use super::{LANGUAGES, LAYERS, sar, types::Feature};
use std::collections::BTreeSet;

/// How many layers a compile can write: the OpenMapTiles layers, then the
/// opt-in `sar` layer.
const KNOWN: usize = LAYERS.len() + 1;

/// The layer at a position of the selection: `LAYERS`, then `sar`.
fn known(index: usize) -> &'static str {
    LAYERS.get(index).copied().unwrap_or(sar::LAYER)
}

#[derive(Debug, Clone)]
pub(super) struct Selection {
    /// The selected layers, `LAYERS` then `sar`.
    layers: [bool; KNOWN],
    /// `name:<lang>` attributes left out; empty when every language is kept.
    dropped_names: BTreeSet<String>,
}

/// Every OpenMapTiles layer, without `sar`.
fn default_layers() -> [bool; KNOWN] {
    let mut layers = [true; KNOWN];
    layers[LAYERS.len()] = false;
    layers
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            layers: default_layers(),
            dropped_names: BTreeSet::new(),
        }
    }
}

impl Selection {
    /// `layers` lists the layers to write, `sar` among them when it is
    /// written [default: every OpenMapTiles layer, without `sar`].
    pub fn new(layers: Option<&[String]>, languages: Option<&[String]>) -> Result<Self, String> {
        let layers = match layers {
            None => default_layers(),
            Some(names) => {
                let mut selected = [false; KNOWN];
                for name in names {
                    selected[layer_index(name)?] = true;
                }
                if !selected.contains(&true) {
                    return Err("no layer is left to write".into());
                }
                selected
            }
        };
        let dropped_names = match languages {
            None => BTreeSet::new(),
            Some(languages) => {
                let kept = resolve_languages(languages)?;
                LANGUAGES
                    .iter()
                    .filter(|language| !kept.contains(language))
                    .map(|language| format!("name:{language}"))
                    .collect()
            }
        };
        Ok(Self {
            layers,
            dropped_names,
        })
    }

    /// The layers written, in `LAYERS` order, then `sar`.
    pub fn layers(&self) -> Vec<&'static str> {
        (0..KNOWN)
            .filter(|index| self.layers[*index])
            .map(known)
            .collect()
    }

    pub fn has_layer(&self, layer: &str) -> bool {
        match layer_index(layer) {
            Ok(index) => self.layers[index],
            // As before `sar`, a full build passes on any other name, so a
            // feature that cannot be encoded still fails the compile.
            Err(_) => self.layers == default_layers(),
        }
    }

    /// Removes the `name:<lang>` attributes of languages left out. Nothing in
    /// the profile reads them back, so this equals never adding them.
    pub fn strip_names(&self, feature: &mut Feature) {
        if self.dropped_names.is_empty() {
            return;
        }
        let keep = |key: &String| !key.starts_with("name:") || !self.dropped_names.contains(key);
        feature.attrs.retain(|key, _| keep(key));
        feature.attr_minzoom.retain(|key, _| keep(key));
    }
}

impl super::profile::FeatureFilter for Selection {
    fn keep(&self, feature: &mut Feature) -> bool {
        if !self.has_layer(&feature.layer) {
            return false;
        }
        self.strip_names(feature);
        true
    }
}

fn layer_index(name: &str) -> Result<usize, String> {
    (0..KNOWN)
        .find(|index| known(*index) == name)
        .ok_or_else(|| {
            format!(
                "unknown layer '{name}'; the layers are {} (and {}, written only when asked for)",
                LAYERS.join(", "),
                sar::LAYER
            )
        })
}

/// The layers to write, in `LAYERS` order then `sar`, given either the layers
/// to write or the layers to leave out; `None` when neither is given and
/// `sar` is not asked for (every OpenMapTiles layer). `sar` asks for the
/// opt-in `sar` layer beside the others; naming `sar` among the layers to
/// write asks for it too, and naming it among those to leave out drops it.
/// Every name must be one of [`LAYERS`] or `sar`, and at least one layer must
/// remain.
pub fn select_layers(
    only: Option<&[String]>,
    excluded: Option<&[String]>,
    sar: bool,
) -> Result<Option<Vec<String>>, String> {
    let selected = match (only, excluded) {
        (None, None) if !sar => return Ok(None),
        (Some(_), Some(_)) => {
            return Err("give either the layers to write or those to leave out, not both".into());
        }
        (Some(names), None) => {
            let mut selected = [false; KNOWN];
            for name in names {
                selected[layer_index(name)?] = true;
            }
            selected[LAYERS.len()] |= sar;
            selected
        }
        (None, excluded) => {
            let mut selected = default_layers();
            selected[LAYERS.len()] = sar;
            for name in excluded.unwrap_or_default() {
                selected[layer_index(name)?] = false;
            }
            selected
        }
    };
    if !selected.contains(&true) {
        return Err("no layer is left to write".into());
    }
    Ok(Some(
        (0..KNOWN)
            .filter(|index| selected[*index])
            .map(|index| known(index).to_string())
            .collect(),
    ))
}

/// The languages whose `name:<lang>` attributes a `languages` list keeps, in
/// `LANGUAGES` order. It reads like Planetiler's `--languages` with the
/// profile's languages as the default set: a code, optionally prefixed with
/// `name:`; `default` for every language of the profile; `-code` to leave a
/// language out even if listed. As in Planetiler, a list of exclusions alone
/// keeps nothing; `default,-ru` keeps all but Russian. Unlike Planetiler,
/// codes outside the profile's languages, and `*` (every `name:*` tag), are
/// refused: the profile only ever writes its own languages.
pub fn resolve_languages(languages: &[String]) -> Result<Vec<&'static str>, String> {
    let mut included = BTreeSet::new();
    let mut excluded = BTreeSet::new();
    for entry in languages {
        if entry == "default" {
            included.extend(LANGUAGES.iter().copied());
            continue;
        }
        if entry == "*" {
            return Err(
                "the language '*' is not supported: list codes or 'default' (the profile's \
                 languages)"
                    .into(),
            );
        }
        let (set, code) = match entry.strip_prefix('-') {
            Some(code) => (&mut excluded, code),
            None => (&mut included, entry.as_str()),
        };
        let code = code.strip_prefix("name:").unwrap_or(code);
        let Some(known) = LANGUAGES.iter().find(|known| **known == code) else {
            return Err(format!(
                "unknown language '{entry}'; the languages are {} (or 'default' for all, \
                 '-code' to leave one out)",
                LANGUAGES.join(", ")
            ));
        };
        set.insert(*known);
    }
    Ok(LANGUAGES
        .iter()
        .copied()
        .filter(|language| included.contains(language) && !excluded.contains(language))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn languages_follow_planetiler_list_syntax() {
        let resolve = |list: &[&str]| resolve_languages(&strings(list));
        assert_eq!(resolve(&["pl", "en", "de"]).unwrap(), ["de", "en", "pl"]);
        assert_eq!(resolve(&["name:fr", "fr"]).unwrap(), ["fr"]);
        assert_eq!(resolve(&["default"]).unwrap(), LANGUAGES);
        let all_but_russian = resolve(&["default", "-ru"]).unwrap();
        assert_eq!(all_but_russian.len(), LANGUAGES.len() - 1);
        assert!(!all_but_russian.contains(&"ru"));
        // Exclusions win wherever they stand, and alone keep nothing.
        assert!(resolve(&["-en", "en"]).unwrap().is_empty());
        assert!(resolve(&["-name:en"]).unwrap().is_empty());
        assert!(resolve(&[]).unwrap().is_empty());
        for bad in ["xx", "EN", "-xx", "*", "name:", ""] {
            assert!(resolve(&[bad]).is_err(), "{bad}");
        }
        assert!(resolve(&["xx"]).unwrap_err().contains("zh-Hant"));
    }

    #[test]
    fn layers_are_checked_against_the_schema() {
        let select = |only: Option<&[&str]>, excluded: Option<&[&str]>| {
            select_layers(
                only.map(strings).as_deref(),
                excluded.map(strings).as_deref(),
                false,
            )
        };
        assert_eq!(select(None, None).unwrap(), None);
        assert_eq!(
            select(Some(&["poi", "water", "poi"]), None)
                .unwrap()
                .unwrap(),
            ["water", "poi"]
        );
        let without = select(None, Some(&["housenumber", "poi"]))
            .unwrap()
            .unwrap();
        assert_eq!(without.len(), LAYERS.len() - 2);
        assert!(!without.iter().any(|layer| layer == "poi"));
        assert!(select(Some(&["poi"]), Some(&["water"])).is_err());
        assert!(select(None, Some(&LAYERS)).is_err());
        assert!(select(Some(&[]), None).is_err());
        let error = select(Some(&["roads"]), None).unwrap_err();
        assert!(error.contains("'roads'") && error.contains("aerodrome_label"));

        assert!(Selection::new(Some(&strings(&["roads"])), None).is_err());
        assert!(Selection::new(Some(&[]), None).is_err());
        let selection = Selection::new(Some(&strings(&["poi", "water"])), None).unwrap();
        assert_eq!(selection.layers(), ["water", "poi"]);
        assert!(selection.has_layer("water") && !selection.has_layer("place"));
        assert_eq!(Selection::default().layers(), LAYERS);
        assert!(!Selection::default().has_layer("sar"));
    }

    #[test]
    fn sar_is_written_only_when_asked_for() {
        let select = |only: Option<&[&str]>, excluded: Option<&[&str]>, sar| {
            select_layers(
                only.map(strings).as_deref(),
                excluded.map(strings).as_deref(),
                sar,
            )
        };
        let mut all = LAYERS.to_vec();
        all.push("sar");
        assert_eq!(select(None, None, true).unwrap().unwrap(), all);
        assert_eq!(
            select(Some(&["sar", "water"]), None, false)
                .unwrap()
                .unwrap(),
            ["water", "sar"]
        );
        assert_eq!(
            select(Some(&["water"]), None, true).unwrap().unwrap(),
            ["water", "sar"]
        );
        let without_poi = select(None, Some(&["poi"]), true).unwrap().unwrap();
        assert_eq!(without_poi.len(), LAYERS.len());
        assert_eq!(without_poi.last().unwrap(), "sar");
        // Leaving out a layer that is not written anyway is no error.
        let plain = select(None, Some(&["sar"]), false).unwrap().unwrap();
        assert_eq!(plain, LAYERS);
        assert_eq!(select(None, Some(&["sar"]), true).unwrap().unwrap(), LAYERS);
        assert!(select(Some(&["sar"]), Some(&["sar"]), true).is_err());

        let selection = Selection::new(Some(&strings(&["sar"])), None).unwrap();
        assert_eq!(selection.layers(), ["sar"]);
        assert!(selection.has_layer("sar") && !selection.has_layer("poi"));
        let selection = Selection::new(
            Some(&all.iter().map(|l| l.to_string()).collect::<Vec<_>>()),
            None,
        )
        .unwrap();
        assert_eq!(selection.layers(), all);
    }

    #[test]
    fn only_the_unselected_translations_are_stripped() {
        let source = super::super::types::Source {
            id: 1,
            osm_id: 1,
            kind: super::super::types::SourceKind::Node,
            source_layer: "osm".into(),
            tags: Default::default(),
            geometry: geo_types::Geometry::Point(geo_types::Point::new(0., 0.)),
            relations: vec![],
        };
        let mut feature = Feature::new(&source, "place", source.geometry.clone(), 0, 14);
        for key in [
            "name",
            "name_en",
            "name_de",
            "name_int",
            "name:latin",
            "name:nonlatin",
            "name:en",
            "name:de",
            "name:pl",
            "name:la",
        ] {
            feature.attrs.insert(key.into(), json!(key));
            feature.attr_minzoom.insert(key.into(), 5);
        }
        let selection = Selection::new(None, Some(&strings(&["pl"]))).unwrap();
        selection.strip_names(&mut feature);
        let expected = [
            "name",
            "name:latin",
            "name:nonlatin",
            "name:pl",
            "name_de",
            "name_en",
            "name_int",
        ];
        assert!(feature.attrs.keys().eq(expected));
        assert!(feature.attr_minzoom.keys().eq(expected));
    }
}
