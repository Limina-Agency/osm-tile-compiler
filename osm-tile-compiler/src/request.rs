//! The typed parts of a [`BasemapRequest`](crate::BasemapRequest): the zooms,
//! the layers, and the area to compile.

use crate::{CompileArea, CompileBounds, CompileError};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

/// The zooms a compile writes, from [`min`](Self::min) to [`max`](Self::max)
/// inclusive. A value always holds `min <= max <= ZoomRange::MAX_ZOOM`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ZoomRange {
    min: u8,
    max: u8,
}

impl ZoomRange {
    /// The highest zoom a compile writes. Tiles at it carry every detail;
    /// viewers overzoom them.
    pub const MAX_ZOOM: u8 = 14;

    /// Zooms 0 to 14, the OpenMapTiles profile's full range.
    pub const ALL: Self = Self {
        min: 0,
        max: Self::MAX_ZOOM,
    };

    /// The zooms from `min` to `max`; [`CompileError::InvalidRequest`] unless
    /// `min <= max <= MAX_ZOOM`.
    pub fn new(min: u8, max: u8) -> Result<Self, CompileError> {
        if min > max || max > Self::MAX_ZOOM {
            return Err(CompileError::InvalidRequest(format!(
                "zooms must satisfy 0 <= min <= max <= {}, got {min} and {max}",
                Self::MAX_ZOOM
            )));
        }
        Ok(Self { min, max })
    }

    /// The lowest zoom written.
    pub const fn min(self) -> u8 {
        self.min
    }

    /// The highest zoom written.
    pub const fn max(self) -> u8 {
        self.max
    }

    /// Whether `zoom` is written.
    pub const fn contains(self, zoom: u8) -> bool {
        self.min <= zoom && zoom <= self.max
    }
}

impl Default for ZoomRange {
    /// [`ZoomRange::ALL`].
    fn default() -> Self {
        Self::ALL
    }
}

/// The name of a vector tile layer, such as `water` or `transportation`.
///
/// Any name can be held; a compile checks it against the layers its profile
/// writes ([`OMT_LAYERS`](crate::OMT_LAYERS) for OpenMapTiles) and refuses
/// an unknown one with [`CompileError::InvalidRequest`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LayerName(String);

impl LayerName {
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for LayerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for LayerName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<&str> for LayerName {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl From<String> for LayerName {
    fn from(name: String) -> Self {
        Self(name)
    }
}

impl PartialEq<str> for LayerName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for LayerName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// Which layers a compile writes. Each layer written is identical to the
/// same layer of a full build; the archive's metadata lists only those.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum LayerSelection {
    /// Every layer of the profile.
    #[default]
    All,
    /// Only these layers.
    Only(Vec<LayerName>),
    /// Every layer but these.
    Except(Vec<LayerName>),
}

impl LayerSelection {
    /// [`LayerSelection::Only`] these layers.
    pub fn only<T: Into<LayerName>>(layers: impl IntoIterator<Item = T>) -> Self {
        Self::Only(layers.into_iter().map(Into::into).collect())
    }

    /// Every layer [`except`](LayerSelection::Except) these.
    pub fn except<T: Into<LayerName>>(layers: impl IntoIterator<Item = T>) -> Self {
        Self::Except(layers.into_iter().map(Into::into).collect())
    }

    /// The layers written, in the profile's order, when the opt-in
    /// [`SAR_LAYER`](crate::SAR_LAYER) is not asked for otherwise
    /// ([`BasemapRequest::sar`](crate::BasemapRequest::sar)). Every name must
    /// be one of [`OMT_LAYERS`](crate::OMT_LAYERS) or `sar`, and at least one
    /// layer must remain; otherwise [`CompileError::InvalidRequest`].
    /// [`LayerSelection::Only`] naming `sar` writes it.
    pub fn resolve(&self) -> Result<Vec<LayerName>, CompileError> {
        Ok(
            match self.names(false).map_err(CompileError::InvalidRequest)? {
                Some(names) => names.into_iter().map(LayerName::from).collect(),
                None => crate::omt::LAYERS
                    .into_iter()
                    .map(LayerName::from)
                    .collect(),
            },
        )
    }

    /// The layers written, as the profile takes them: `None` for every
    /// OpenMapTiles layer. `sar` adds the opt-in `sar` layer unless the
    /// selection leaves it out.
    pub(crate) fn names(&self, sar: bool) -> Result<Option<Vec<String>>, String> {
        let strings =
            |names: &[LayerName]| names.iter().map(|n| n.0.clone()).collect::<Vec<String>>();
        match self {
            Self::All => crate::omt::select_layers(None, None, sar),
            Self::Only(names) => crate::omt::select_layers(Some(&strings(names)), None, sar),
            Self::Except(names) => crate::omt::select_layers(None, Some(&strings(names)), sar),
        }
    }
}

/// The part of the extract a compile covers, and how much of the extract
/// it reads for it.
///
/// The default is the whole extract: the PBF header's box, read in full.
#[derive(Debug, Clone, Default, PartialEq)]
#[non_exhaustive]
pub struct Area {
    /// The box to compile [default: the PBF header's]; one whose west is
    /// greater than its east crosses the antimeridian.
    pub bounds: Option<CompileBounds>,
    /// A polygon limiting the compile to the tiles it covers, within
    /// `bounds`. Like Planetiler's `--polygon`, it decides which tiles are
    /// written; features in those tiles keep their full geometry.
    pub polygon: Option<AreaPolygon>,
    /// How much of the extract to read [default: all of it].
    pub read: ReadScope,
}

impl Area {
    /// The whole extract, read in full: the default.
    pub fn whole_extract() -> Self {
        Self::default()
    }

    /// The tiles within `bounds`.
    pub fn bounds(bounds: CompileBounds) -> Self {
        Self {
            bounds: Some(bounds),
            ..Self::default()
        }
    }

    /// The tiles a polygon covers.
    pub fn polygon(polygon: impl Into<AreaPolygon>) -> Self {
        Self {
            polygon: Some(polygon.into()),
            ..Self::default()
        }
    }

    /// The same area, limited to `bounds` as well.
    pub fn within(mut self, bounds: CompileBounds) -> Self {
        self.bounds = Some(bounds);
        self
    }

    /// The same area, read as `read` says.
    pub fn reading(mut self, read: ReadScope) -> Self {
        self.read = read;
        self
    }

    /// Whether the area is narrower than the extract: it has bounds or a
    /// polygon.
    pub fn is_limited(&self) -> bool {
        self.bounds.is_some() || self.polygon.is_some()
    }
}

/// The polygon of an [`Area`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum AreaPolygon {
    /// A GeoJSON or Osmosis `.poly` file, read when the compile starts (see
    /// [`CompileArea::read`]).
    File(PathBuf),
    /// A polygon already in memory.
    Shape(CompileArea),
}

impl AreaPolygon {
    /// The polygon, read from its file if it has one.
    pub fn load(&self) -> Result<CompileArea, CompileError> {
        match self {
            Self::File(path) => CompileArea::read(path),
            Self::Shape(area) => Ok(area.clone()),
        }
    }
}

impl From<PathBuf> for AreaPolygon {
    fn from(path: PathBuf) -> Self {
        Self::File(path)
    }
}

impl From<&std::path::Path> for AreaPolygon {
    fn from(path: &std::path::Path) -> Self {
        Self::File(path.to_path_buf())
    }
}

impl From<CompileArea> for AreaPolygon {
    fn from(area: CompileArea) -> Self {
        Self::Shape(area)
    }
}

/// How much of the extract a compile reads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReadScope {
    /// Every OSM object in the extract.
    #[default]
    WholeExtract,
    /// Only the OSM objects in and near the area (its bounds, within its
    /// polygon's box), so a small area compiles from a country extract in
    /// seconds. The tiles at the highest zoom keep the content a full read
    /// gives them; lower zooms show only what lies near the area. It needs
    /// an area with bounds or a polygon.
    NearArea,
}

impl ReadScope {
    /// A stable identifier: `whole_extract` or `near_area`.
    pub fn id(self) -> &'static str {
        match self {
            Self::WholeExtract => "whole_extract",
            Self::NearArea => "near_area",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_ranges_are_checked() {
        assert_eq!(ZoomRange::default(), ZoomRange::new(0, 14).unwrap());
        let range = ZoomRange::new(3, 9).unwrap();
        assert_eq!((range.min(), range.max()), (3, 9));
        assert!(range.contains(3) && range.contains(9) && !range.contains(10));
        assert!(ZoomRange::new(5, 4).is_err());
        assert!(ZoomRange::new(0, 15).is_err());
        assert_eq!(ZoomRange::new(14, 14).unwrap().min(), 14);
    }

    #[test]
    fn layer_selections_resolve_in_profile_order() {
        assert_eq!(LayerSelection::All.resolve().unwrap().len(), 16);
        assert_eq!(
            LayerSelection::only(["poi", "water"]).resolve().unwrap(),
            [LayerName::from("water"), LayerName::from("poi")]
        );
        let except = LayerSelection::except(["building"]).resolve().unwrap();
        assert_eq!(except.len(), 15);
        assert!(!except.iter().any(|layer| layer == "building"));
        let unknown = LayerSelection::only(["roads"]).resolve().unwrap_err();
        assert_eq!(unknown.kind(), "invalid_request");
        assert!(
            LayerSelection::only(Vec::<LayerName>::new())
                .resolve()
                .is_err()
        );
    }
}
