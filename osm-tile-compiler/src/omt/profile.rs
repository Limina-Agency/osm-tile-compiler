//! The seam between a schema and the engine (roadmap lane E4).
//!
//! The engine reads sources (OSM objects and supplemental records), hands
//! each one to a [`Profile`], renders the [`Feature`]s it makes into tile
//! records, sorts them and writes the archive. A profile decides which layers
//! exist and what goes into them; it knows nothing about tiles, sorting or
//! archives. The OpenMapTiles port is one profile ([`super::OmtProfile`]),
//! the configurable schemas of [`crate::schema`] are another.
//!
//! Supplemental sources are evaluated serially ([`Profile::process`]), since
//! they may fill state that OSM sources read. OSM sources are evaluated on the
//! render workers against the then read-only profile ([`Profile::evaluate`]);
//! what a source adds to that state is returned as a [`Contribution`] and
//! replayed in source order ([`Profile::apply`]) once every OSM source is
//! read. [`Profile::finish`] then makes the features that needed every
//! source.
use super::types::{Feature, Source};

/// What one OSM source adds to a profile's state.
pub(crate) trait Contribution: Default + Send + 'static {
    /// Whether there is nothing to replay; empty contributions are dropped.
    fn is_empty(&self) -> bool;
}

/// A contribution for profiles that keep no state across sources.
#[derive(Default)]
pub(crate) struct NoContribution;

impl Contribution for NoContribution {
    fn is_empty(&self) -> bool {
        true
    }
}

/// A schema: turns sources into features.
pub(crate) trait Profile: Send + Sync + 'static {
    type Contribution: Contribution;

    /// Evaluates a supplemental source serially, applying its effect on the
    /// profile's state at once.
    fn process(&mut self, source: &Source, features: &mut Vec<Feature>);

    /// Evaluates an OSM source on a render worker.
    fn evaluate(
        &self,
        source: &Source,
        features: &mut Vec<Feature>,
        contribution: &mut Self::Contribution,
    );

    /// Replays one source's contribution; called in source order.
    fn apply(&mut self, contribution: Self::Contribution);

    /// The features that need every source, made once all are read.
    fn finish(&mut self, deferred: &mut Vec<Feature>);
}

/// Which of a profile's features the archive keeps, and what of them.
pub(crate) trait FeatureFilter: Send + Sync + 'static {
    /// Whether to write `feature`, after removing what the request leaves
    /// out of it (such as name languages).
    fn keep(&self, feature: &mut Feature) -> bool;
}
