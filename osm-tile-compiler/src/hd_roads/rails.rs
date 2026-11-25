//! Railway tracks: what they lie on, their sleepers and their two rails at
//! their gauge. A tram in a street has its rails alone.

use super::geometry;
use super::network::{Point, Way, distance};
use super::profile::{self, Class, Confidence, Level, Line, Placed, Profile};
use super::structures;
use rustc_hash::{FxHashMap, FxHashSet};

/// A track as its tags have it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Track {
    pub class: Class,
    pub level: Level,
    /// Metres between its rails, and whether that is tagged.
    pub gauge: f64,
    pub tagged: bool,
    /// Whether the tags say that it lies in a road or a square.
    pub embedded: bool,
}

/// Metres between rails that are believed.
const GAUGES: std::ops::RangeInclusive<f64> = 0.38..=1.7;

/// The track a way with these tags is, if trains run on it.
pub(super) fn track<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Track> {
    let (class, usual) = match get("railway")? {
        "rail" | "subway" => (Class::Rail, 1.435),
        "narrow_gauge" => (Class::Rail, 1.0),
        "tram" | "light_rail" => (Class::Tram, 1.435),
        _ => return None,
    };
    let is = |key: &str| get(key).is_some_and(|v| v != "no");
    let gone = [
        "disused",
        "abandoned",
        "razed",
        "demolished",
        "construction",
        "proposed",
    ];
    if gone.into_iter().any(is) {
        return None;
    }
    // millimetres, the first of those a track of more has
    let gauge = get("gauge")
        .and_then(|v| v.split(';').next()?.trim().parse::<f64>().ok())
        .map(|mm| mm / 1000.0)
        .filter(|g| GAUGES.contains(g));
    let street = get("railway") == Some("tram");
    Some(Track {
        class,
        level: profile::level(&get),
        gauge: gauge.unwrap_or(usual),
        tagged: gauge.is_some(),
        embedded: is("embedded") || is("embedded_rails") || (street && is("highway")),
    })
}

impl Track {
    /// Metres of its deck across: a train's, a tram's, and less for a narrow gauge.
    fn deck(&self) -> f64 {
        match self.class {
            Class::Tram => 3.6,
            _ if self.gauge < NARROW => 3.4,
            _ => 5.0,
        }
    }

    pub(super) fn profile(&self) -> Profile {
        Profile {
            class: self.class,
            level: self.level,
            paved: false,
            confidence: if self.tagged {
                Confidence::Full
            } else {
                Confidence::Assumed
            },
            width: self.deck(),
            measured: self.tagged,
            centre: 0.0,
            lines: vec![
                (-self.gauge / 2.0, Line::Rail),
                (self.gauge / 2.0, Line::Rail),
            ],
            lanes: None,
            parking: [0.0; 2],
            sides: Default::default(),
            paving: None,
            roundabout: false,
            placed: Placed::Unsaid,
            speed: 0.0,
            named: [0; 2],
            rules: &super::country::DEFAULT,
        }
    }
}

/// Metres between rails below which a track is of a narrow gauge.
const NARROW: f64 = 1.2;

/// What a track lies on that is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Bed {
    Ballast,
    /// A tram's own.
    Trackbed,
}

impl Bed {
    pub(super) fn kind(self) -> &'static str {
        match self {
            Self::Ballast => "ballast",
            Self::Trackbed => "trackbed",
        }
    }

    /// Metres across, under one track of this gauge.
    pub(super) fn width(self, gauge: f64) -> f64 {
        let narrow = if gauge < NARROW { 0.8 } else { 0.0 };
        match self {
            Self::Ballast => 3.2 - narrow,
            Self::Trackbed => 2.8 - narrow,
        }
    }

    /// Metres between two tracks' middles that lie on one bed.
    fn joined(self) -> f64 {
        match self {
            Self::Ballast => 5.2,
            Self::Trackbed => 4.4,
        }
    }
}

/// A stretch of a track on one thing.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Strip {
    pub way: u32,
    pub points: Vec<Point>,
    pub gauge: f64,
    pub tagged: bool,
    pub level: Level,
    /// Nothing in a street.
    pub bed: Option<Bed>,
}

impl Strip {
    /// Metres from its line that it reaches, what joins it to others too.
    pub(super) fn reach(&self) -> f64 {
        self.bed.map_or(self.gauge / 2.0, |bed| bed.joined() / 2.0)
    }
}

/// Segments of ways by cell: the way, and the segment's place in it.
struct Segments<'a> {
    ways: &'a [Way],
    at: FxHashMap<(i64, i64), Vec<(u32, u32)>>,
}

/// Where a point lies beside a segment.
struct Beside {
    way: u32,
    /// The segment's direction.
    along: Point,
    /// Metres right of it.
    right: f64,
}

impl<'a> Segments<'a> {
    /// Those of the ways `of` accepts that come within a cell `wanted` has.
    fn new(
        ways: &'a [Way],
        wanted: Option<&FxHashSet<(i64, i64)>>,
        of: impl Fn(&Way) -> bool,
    ) -> Self {
        let mut at: FxHashMap<_, Vec<_>> = FxHashMap::default();
        for (index, way) in ways.iter().enumerate().filter(|(_, w)| of(w)) {
            for (segment, cell) in structures::cells(&way.points) {
                if wanted.is_none_or(|w| w.contains(&cell)) {
                    at.entry(cell)
                        .or_default()
                        .push((index as u32, segment as u32));
                }
            }
        }
        Self { ways, at }
    }

    /// The segments that `p` lies beside: those of its cell and the cells about
    /// it, of which a segment may be in several.
    fn beside(&self, p: Point) -> impl Iterator<Item = Beside> + '_ {
        let cell = structures::cell(p);
        (-1..=1)
            .flat_map(move |x| (-1..=1).map(move |y| (cell.0 + x, cell.1 + y)))
            .filter_map(|cell| self.at.get(&cell))
            .flatten()
            .filter_map(move |&(index, k)| {
                let way = &self.ways[index as usize];
                let (a, b) = (way.points[k as usize], way.points[k as usize + 1]);
                let length = distance(a, b);
                if length == 0.0 {
                    return None;
                }
                let along = [(b[0] - a[0]) / length, (b[1] - a[1]) / length];
                let q = [p[0] - a[0], p[1] - a[1]];
                let t = q[0] * along[0] + q[1] * along[1];
                // a little past its ends, for the outside of a bend
                let past = 0.5 * way.scale;
                (-past..=length + past).contains(&t).then(|| Beside {
                    way: index,
                    along,
                    right: (q[1] * along[0] - q[0] * along[1]) / way.scale,
                })
            })
    }
}

/// The sine of the angle up to which a track runs along a road, and along
/// another track.
const ALONG: f64 = 0.4;
const PARALLEL: f64 = 0.2;
/// Metres beyond a road's edge a track is still in it: where a bed of its own
/// would lie in the road.
const BESIDE: f64 = 1.25;
/// Metres at which a track is asked whether it is in a road, and the least that
/// it is in one, or on its own between two.
const STEP: f64 = 4.0;
const SHORTEST: f64 = 16.0;
/// Metres at most between places at which a track is asked what is beside it.
const FILL_STEP: f64 = 15.0;

fn direction(a: Point, b: Point) -> Point {
    let length = distance(a, b).max(f64::MIN_POSITIVE);
    [(b[0] - a[0]) / length, (b[1] - a[1]) / length]
}

/// The points `t` of the way along from `a` to `b`.
fn mix(a: Point, b: Point, t: f64) -> Point {
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
}

/// The stretches of a track (metres along it, and whether in a road), by
/// whether each of its steps is in one.
fn stretches(steps: &[(f64, bool)]) -> Vec<([f64; 2], bool)> {
    let mut out: Vec<([f64; 2], bool)> = Vec::new();
    let mut at = 0.0;
    let add =
        |out: &mut Vec<([f64; 2], bool)>, [from, to]: [f64; 2], road: bool| match out.last_mut() {
            Some(last) if last.1 == road => last.0[1] = to,
            _ => out.push(([from, to], road)),
        };
    for &(length, road) in steps {
        add(&mut out, [at, at + length], road);
        at += length;
    }
    // what is short is as what is about it: first between two stretches in a road
    for short in [false, true] {
        let all = out.len() > 1;
        let mut next = Vec::new();
        for (span, road) in out {
            let flip = all && road == short && span[1] - span[0] < SHORTEST;
            add(&mut next, span, road != flip);
        }
        out = next;
    }
    out
}

/// The tracks, cut where they come into a street or leave it.
pub(super) struct Tracks<'a> {
    pub strips: Vec<Strip>,
    /// The tracks on the ground.
    ground: Segments<'a>,
}

/// Whether a track from `a` to `b` lies in a road of `roads`, or within `beyond`
/// metres of its edge.
fn in_road(roads: &Segments, a: Point, b: Point, level: Level, beyond: f64) -> bool {
    let along = direction(a, b);
    roads.beside(mix(a, b, 0.5)).any(|found| {
        let p = &roads.ways[found.way as usize].profile;
        (along[0] * found.along[1] - along[1] * found.along[0]).abs() < ALONG
            && (found.right - p.centre).abs() < p.width / 2.0 + beyond
            && p.level == level
    })
}

/// Metres and whether in a road, of each step along a way's line.
fn steps(way: &Way, roads: &Segments, beyond: f64) -> Vec<(f64, bool)> {
    let mut out = Vec::new();
    for p in way.points.windows(2) {
        let length = distance(p[0], p[1]) / way.scale;
        let parts = (length / STEP).ceil().max(1.0) as u32;
        for k in 0..parts {
            let at = |k: u32| mix(p[0], p[1], f64::from(k) / f64::from(parts));
            let road = in_road(roads, at(k), at(k + 1), way.profile.level, beyond);
            out.push((length / f64::from(parts), road));
        }
    }
    out
}

/// The motor roads about the trams of `tracks`.
fn about_trams<'a, 't>(roads: &'a [Way], tracks: impl Iterator<Item = &'t Way>) -> Segments<'a> {
    let mut cells: FxHashSet<(i64, i64)> = FxHashSet::default();
    let trams = tracks.filter(|w| w.profile.class == Class::Tram);
    for (_, (x, y)) in trams.flat_map(|w| structures::cells(&w.points)) {
        cells.extend((-1..=1).flat_map(|i| (-1..=1).map(move |j| (x + i, y + j))));
    }
    Segments::new(roads, Some(&cells), |w| w.profile.class.is_motor())
}

/// Metres beyond a road's edge, less than nothing, at which both rails are on it.
fn both(gauge: f64) -> f64 {
    0.1 - gauge / 2.0
}

/// Whether each of `tracks` is a tram's on a bridge of the roads it runs along,
/// and no bridge of its own. Trams in roads are paved.
pub(super) fn on_roads_bridges(roads: &[Way], tracks: &mut [(Way, Track)]) -> Vec<bool> {
    let bridge = |w: &Way| matches!(w.profile.level, Level::Bridge(_));
    let roads = about_trams(roads, tracks.iter().map(|t| &t.0));
    let mostly = |way: &Way, beyond: f64| {
        let steps = steps(way, &roads, beyond);
        let all: f64 = steps.iter().map(|s| s.0).sum();
        steps.iter().filter(|s| s.1).map(|s| s.0).sum::<f64>() > all / 2.0
    };
    let tram = |way: &Way| way.profile.class == Class::Tram;
    tracks
        .iter_mut()
        .map(|(way, track)| {
            way.profile.paved = tram(way) && mostly(way, BESIDE);
            way.profile.paved && bridge(way) && mostly(way, both(track.gauge))
        })
        .collect()
}

/// The tracks `ways[first..]`, whose tags are `tracks` and whose last `carried`
/// are on roads' bridges, among the roads before them.
pub(super) fn build<'a>(
    ways: &'a [Way],
    first: usize,
    tracks: &[Track],
    carried: usize,
) -> Tracks<'a> {
    let roads = about_trams(&ways[..first], ways[first..].iter());
    let own = ways.len() - carried;
    let mut strips = Vec::new();
    for (i, track) in tracks.iter().enumerate() {
        let index = first + i;
        let way = &ways[index];
        let bed = match track.class {
            Class::Tram => Bed::Trackbed,
            _ => Bed::Ballast,
        };
        let mut add = |points: Vec<Point>, street: bool| {
            strips.push(Strip {
                way: index as u32,
                points,
                gauge: track.gauge,
                tagged: track.tagged,
                level: track.level,
                bed: (!street).then_some(bed),
            });
        };
        match track.level {
            Level::Tunnel(_) => {}
            _ if track.embedded || index >= own => add(way.points.clone(), true),
            Level::Ground if track.class == Class::Tram => {
                let stretches = stretches(&steps(way, &roads, BESIDE));
                let Some(end) = stretches.last().map(|s| s.0[1]) else {
                    continue;
                };
                for ([from, to], street) in stretches {
                    let cut = [from * way.scale, (end - to) * way.scale];
                    if let Some(points) = geometry::trim(&way.points, cut) {
                        add(points, street);
                    }
                }
            }
            _ => add(way.points.clone(), false),
        }
    }
    Tracks {
        strips,
        ground: Segments::new(ways, None, |w| {
            w.profile.class.is_rail() && w.profile.level == Level::Ground
        }),
    }
}

impl Tracks<'_> {
    /// The ground between a strip's `run` and the tracks beside it that lie on
    /// the same bed, as rings.
    pub(super) fn between(&self, strip: &Strip, run: &[Point]) -> Vec<Vec<Point>> {
        let Some(bed) = strip.bed.filter(|_| strip.level == Level::Ground) else {
            return Vec::new();
        };
        let ways = self.ground.ways;
        let way = &ways[strip.way as usize];
        // metres to the nearest track left and right
        let nearest = |p: Point, along: Point| -> [Option<f64>; 2] {
            let mut out = [None; 2];
            for found in self.ground.beside(p) {
                let turned = along[0] * found.along[1] - along[1] * found.along[0];
                let same = ways[found.way as usize].profile.class == way.profile.class;
                if found.way == strip.way || !same || turned.abs() > PARALLEL {
                    continue;
                }
                let against = along[0] * found.along[0] + along[1] * found.along[1] < 0.0;
                // the point is right of that track: the track is left of it
                let right = if against { found.right } else { -found.right };
                let side: &mut Option<f64> = &mut out[usize::from(right > 0.0)];
                if right.abs() <= bed.joined() && side.is_none_or(|d| right.abs() < d) {
                    *side = Some(right.abs());
                }
            }
            out
        };
        let mut out = Vec::new();
        for p in run.windows(2) {
            let along = direction(p[0], p[1]);
            let right = [-along[1], along[0]];
            let length = distance(p[0], p[1]) / way.scale;
            let parts = (length / FILL_STEP).ceil().max(1.0) as u32;
            let at = |k: u32| mix(p[0], p[1], f64::from(k) / f64::from(parts));
            let mut before = nearest(p[0], along);
            for k in 0..parts {
                let (a, b) = (at(k), at(k + 1));
                let after = nearest(b, along);
                for (side, sign) in [(0, -1.0), (1, 1.0)] {
                    // tapered to its own bed where they part
                    let own = bed.width(strip.gauge);
                    let [from, to] = [before[side], after[side]].map(|d| d.unwrap_or(own));
                    if from.max(to) < own + 0.05 {
                        continue;
                    }
                    // a little past halfway, where the other's comes to
                    let out_by = |p: Point, d: f64| {
                        let by = sign * (d / 2.0 + 0.05) * way.scale;
                        [p[0] + right[0] * by, p[1] + right[1] * by]
                    };
                    let ring = [a, b, out_by(b, to), out_by(a, from)];
                    out.extend(super::furniture::ring(&ring));
                }
                before = after;
            }
        }
        out
    }

    /// What is painted of the tracks on roads' bridges, and on the roads that
    /// lead up to them: rings, on a piece of a group.
    pub(super) fn painted(
        &self,
        ways: &[Way],
        index: &structures::Index,
    ) -> Vec<((u32, u32), Vec<Point>)> {
        let mut out = Vec::new();
        for strip in self.strips.iter().filter(|s| s.bed.is_none()) {
            let way = &ways[strip.way as usize];
            for p in strip.points.windows(2) {
                let along = direction(p[0], p[1]);
                let right = [-along[1] * way.scale, along[0] * way.scale];
                let parts = (distance(p[0], p[1]) / (STEP * way.scale)).ceil().max(1.0) as u32;
                let at = |k: u32| mix(p[0], p[1], f64::from(k) / f64::from(parts));
                for k in 0..parts {
                    let (a, b) = (at(k), at(k + 1));
                    let beyond = both(strip.gauge);
                    let Some(on) = index.carrying(ways, a, b, strip.level, beyond) else {
                        continue;
                    };
                    for side in [-strip.gauge / 2.0, strip.gauge / 2.0] {
                        let across = |p: Point, by: f64| {
                            [p[0] + right[0] * (side + by), p[1] + right[1] * (side + by)]
                        };
                        let half = RAIL / 2.0;
                        let ring = [
                            across(a, -half),
                            across(b, -half),
                            across(b, half),
                            across(a, half),
                        ];
                        out.push((on, ring.to_vec()));
                    }
                }
            }
        }
        out
    }
}

/// Metres across a rail that is painted on a bridge, as a style draws one.
const RAIL: f64 = 0.12;

#[cfg(test)]
mod tests {
    use super::super::profile::profile;
    use super::*;

    fn read(tags: &[(&str, &'static str)]) -> Option<Track> {
        track(|k| tags.iter().find(|t| t.0 == k).map(|t| t.1))
    }

    fn road(id: i64, tags: &[(&str, &'static str)], points: &[Point]) -> Way {
        Way {
            id,
            nodes: (0..points.len() as i64).map(|k| id * 100 + k).collect(),
            points: points.to_vec(),
            profile: profile(|k| tags.iter().find(|t| t.0 == k).map(|t| t.1)).unwrap(),
            scale: 1.0,
        }
    }

    fn rail(id: i64, tags: &[(&str, &'static str)], points: &[Point]) -> (Way, Track) {
        let track = read(tags).unwrap();
        let way = Way {
            id,
            nodes: (0..points.len() as i64).map(|k| id * 100 + k).collect(),
            points: points.to_vec(),
            profile: track.profile(),
            scale: 1.0,
        };
        (way, track)
    }

    fn strips(roads: Vec<Way>, rails: Vec<(Way, Track)>) -> Vec<Strip> {
        let first = roads.len();
        let (mut ways, mut tracks) = (roads, Vec::new());
        for (way, track) in rails {
            ways.push(way);
            tracks.push(track);
        }
        build(&ways, first, &tracks, 0).strips
    }

    const TRAM: &[(&str, &str)] = &[("railway", "tram"), ("gauge", "1435")];
    const RAILS: &[(&str, &str)] = &[("railway", "rail")];

    #[test]
    fn a_gauge_is_the_tags_or_the_kinds() {
        let gauge = |tags: &[(&str, &'static str)]| read(tags).map(|t| (t.gauge, t.tagged));
        assert_eq!(gauge(RAILS), Some((1.435, false)));
        assert_eq!(gauge(TRAM), Some((1.435, true)));
        let narrow = ("railway", "narrow_gauge");
        assert_eq!(gauge(&[narrow]), Some((1.0, false)));
        assert_eq!(gauge(&[narrow, ("gauge", "750")]), Some((0.75, true)));
        assert_eq!(
            gauge(&[("railway", "rail"), ("gauge", "1520;1435")]),
            Some((1.52, true))
        );
        for wrong in ["14350", "standard", "-1", "NaN", ""] {
            let found = gauge(&[("railway", "rail"), ("gauge", wrong)]);
            assert_eq!(found, Some((1.435, false)), "{wrong}");
        }
        for gone in ["disused", "abandoned", "razed", "construction", "proposed"] {
            assert_eq!(gauge(&[("railway", gone)]), None);
            assert_eq!(gauge(&[("railway", "rail"), (gone, "yes")]), None);
        }
        assert!(read(&[("railway", "rail"), ("disused", "no")]).is_some());
        assert_eq!(read(&[("railway", "platform")]), None);
    }

    #[test]
    fn a_tram_has_a_bed_of_its_own_out_of_a_street() {
        let street = road(
            1,
            &[("highway", "tertiary"), ("lanes", "2")],
            &[[0.0, 0.0], [100.0, 0.0]],
        );
        // crossed at right angles: in no street
        let across = road(
            2,
            &[("highway", "tertiary")],
            &[[200.0, -50.0], [200.0, 50.0]],
        );
        let tram = rail(3, TRAM, &[[-60.0, 1.6], [300.0, 1.6]]);
        let found = strips(vec![street, across], vec![tram]);
        let seen: Vec<_> = found.iter().map(|s| s.bed).collect();
        assert_eq!(seen, [Some(Bed::Trackbed), None, Some(Bed::Trackbed)]);
        assert!((found[1].points[0][0]).abs() < 4.0, "{found:?}");
        assert!((found[1].points[1][0] - 100.0).abs() < 4.0, "{found:?}");
        // cut, not parted
        assert_eq!(found[0].points.last(), found[1].points.first());
    }

    #[test]
    fn a_track_in_a_street_is_the_tags_or_the_roads_and_a_railways_is_not() {
        let street = || {
            road(
                1,
                &[("highway", "tertiary"), ("lanes", "2")],
                &[[0.0, 0.0], [100.0, 0.0]],
            )
        };
        let line = [[0.0, 1.0], [100.0, 1.0]];
        let train = strips(vec![street()], vec![rail(2, RAILS, &line)]);
        assert_eq!(train[0].bed, Some(Bed::Ballast));
        let tagged = [("railway", "tram"), ("embedded_rails", "yes")];
        let tram = strips(Vec::new(), vec![rail(2, &tagged, &line)]);
        assert_eq!(tram[0].bed, None);
        let tunnel = [("railway", "rail"), ("tunnel", "yes")];
        assert!(strips(Vec::new(), vec![rail(2, &tunnel, &line)]).is_empty());
    }

    #[test]
    fn short_gaps_in_a_street_are_none() {
        let steps = |flags: &[(f64, bool)]| stretches(flags);
        assert_eq!(
            steps(&[(40.0, true), (8.0, false), (40.0, true)]),
            [([0.0, 88.0], true)]
        );
        assert_eq!(
            steps(&[(40.0, false), (8.0, true), (40.0, false)]),
            [([0.0, 88.0], false)]
        );
        assert_eq!(steps(&[(8.0, true)]), [([0.0, 8.0], true)]);
        assert_eq!(steps(&[]), []);
    }

    #[test]
    fn tracks_side_by_side_lie_on_one_bed() {
        let area = |apart: f64| {
            let rails = [
                rail(1, RAILS, &[[0.0, 0.0], [100.0, 0.0]]),
                // against the first
                rail(2, RAILS, &[[100.0, apart], [0.0, apart]]),
            ];
            let (ways, tags): (Vec<Way>, Vec<Track>) = rails.into_iter().unzip();
            let tracks = build(&ways, 0, &tags, 0);
            let mut shapes = Vec::new();
            for strip in &tracks.strips {
                let bed = strip.bed.unwrap();
                shapes.extend(geometry::ribbon(&strip.points, bed.width(strip.gauge)));
                let between = tracks.between(strip, &strip.points);
                shapes.extend(between.into_iter().map(|ring| vec![ring]));
            }
            geometry::union(&shapes).len()
        };
        assert_eq!(area(4.5), 1);
        // a platform's width apart
        assert_eq!(area(9.0), 2);
    }

    #[test]
    fn a_tram_on_a_roads_bridge_is_no_bridge_of_its_own() {
        let bridge = road(
            1,
            &[("highway", "primary"), ("lanes", "4"), ("bridge", "yes")],
            &[[0.0, 0.0], [100.0, 0.0]],
        );
        let on = |layer: &'static str, y: f64| {
            let tags = [("railway", "tram"), ("bridge", "yes"), ("layer", layer)];
            let mut tram = [rail(2, &tags, &[[0.0, y], [100.0, y]])];
            let on = on_roads_bridges(std::slice::from_ref(&bridge), &mut tram)[0];
            (on, tram[0].0.profile.paved)
        };
        assert_eq!(on("1", 1.6), (true, true));
        // one that its rails would hang over the edge of
        assert_eq!(on("1", 7.0), (false, true));
        assert_eq!(
            (on("2", 1.6), on("1", 12.0)),
            ((false, false), (false, false))
        );
    }
}
