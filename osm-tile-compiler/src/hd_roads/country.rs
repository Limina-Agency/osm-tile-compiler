//! How roads look by country: which side traffic keeps to, and how what is
//! painted where it stops, crosses and keeps its lanes looks. A table by ISO 3166
//! code, with a default for what it does not hold, and where an extract's own
//! boundaries say which country a place is in.
//!
//! The country is the extract's own: `boundary=administrative` relations with an
//! `ISO3166-1:alpha2` or `ISO3166-1` code, and those of their parts with an
//! `ISO3166-2` one (`PL-12`, `DE-BE`), whose rings close in it. A region's
//! extract seldom holds its country's whole outline, but mostly its own. A place
//! in none of them has the default's look.

use super::furniture;
use super::network::Point;
use osm_pbf_ingest::{NormalizedRelation, RelationMemberType};
use rustc_hash::FxHashMap;

/// The side of the road traffic keeps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    Right,
    Left,
}

/// What a crossing with signals is marked with, where its marking is not tagged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Signalled {
    Zebra,
    /// Two lines across the road.
    Lines,
    /// Two rows of studs, or of short bars: drawn as the dots' rows.
    Studs,
}

/// What is painted where traffic gives way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GiveWay {
    /// A line of bars.
    Bars,
    /// Two lines of bars.
    Double,
    /// A row of triangles pointing at who gives way.
    Triangles,
}

/// The colour of a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Colour {
    White,
    Yellow,
}

/// A country's look. A bare `width` is metres everywhere, as OpenStreetMap's
/// documentation says, so the table has no units: feet are read where they are
/// tagged as feet, in any country (`profile::metres`).
#[derive(Debug, PartialEq)]
pub(super) struct Rules {
    /// ISO 3166-1 alpha-2; empty for the default.
    pub code: &'static str,
    pub side: Side,
    pub signalled: Signalled,
    pub give_way: GiveWay,
    /// Whether the line traffic stops at at signals is dashed.
    pub signal_dashed: bool,
    /// Metres across a line traffic stops at.
    pub stop: f64,
    /// Metres of a lane's dashed line, and of the gap after each dash.
    pub dash: [f64; 2],
    /// The carriageway's edges; and of a one-way carriageway the one on its
    /// traffic's inner side (the left where it keeps right).
    pub edge: Colour,
    pub inner: Colour,
    /// The line between the two directions.
    pub centre: Colour,
}

impl Rules {
    pub(super) fn keeps_left(&self) -> bool {
        self.side == Side::Left
    }

    /// Metres of a dashed line's paint, and of its period.
    pub(super) fn pattern(&self) -> [f64; 2] {
        [self.dash[0], self.dash[0] + self.dash[1]]
    }

    /// Its dashes as a marking's `dash`, if they are not the default's.
    pub(super) fn dash_name(&self) -> Option<String> {
        (self.dash != DEFAULT.dash).then(|| format!("{}-{}", self.dash[0], self.dash[1]))
    }
}

/// Where the table says nothing: what was drawn everywhere before it, Poland's
/// lanes' dashes (P-1b) and the Vienna Convention's usual zebra, bars and solid
/// line.
pub(super) const DEFAULT: Rules = Rules {
    code: "",
    side: Side::Right,
    signalled: Signalled::Zebra,
    give_way: GiveWay::Bars,
    signal_dashed: false,
    stop: 0.5,
    dash: [2.0, 4.0],
    edge: Colour::White,
    inner: Colour::White,
    centre: Colour::White,
};

/// Countries that keep left and of which nothing else is in the table.
pub(super) const KEEPS_LEFT: Rules = Rules {
    code: "left",
    side: Side::Left,
    ..DEFAULT
};

/// By code. Each says where it is from and how sure it is; what is not sure is
/// the default's.
const TABLE: &[Rules] = &[
    // Austria: Bodenmarkierungsverordnung (BGBl. 848/1995). Only the side is
    // sure; the rest is the default's.
    Rules {
        code: "AT",
        ..DEFAULT
    },
    // Czechia: vyhláška 294/2015 Sb. and TP 133. Not checked: the default.
    Rules {
        code: "CZ",
        ..DEFAULT
    },
    // Germany: StVO Anlage 2 (Wartelinie, Zeichen 341; Haltlinie, Zeichen 294) and
    // the RMS (Richtlinien für die Markierung von Straßen): Leitlinien in towns 3 m
    // with 6 m between, sure; a Fußgängerfurt at signals is two lines, fairly sure
    // (they are often broken, drawn whole).
    Rules {
        code: "DE",
        signalled: Signalled::Lines,
        dash: [3.0, 6.0],
        ..DEFAULT
    },
    // Spain: Norma 8.2-IC Marcas viales (1987). Ceda el paso is a line of bars
    // (M-4.2), sure; dashes not sure: the default.
    Rules {
        code: "ES",
        ..DEFAULT
    },
    // Finland: tieliikennelaki 729/2018 and Väylävirasto's marking guide: the
    // line between directions is yellow, sure.
    Rules {
        code: "FI",
        centre: Colour::Yellow,
        ..DEFAULT
    },
    // France: Instruction interministérielle sur la signalisation routière, 7e
    // partie: lanes' lines T1, 3 m with 10 m between, fairly sure; cédez-le-passage a
    // line of bars, sure.
    Rules {
        code: "FR",
        dash: [3.0, 10.0],
        ..DEFAULT
    },
    // Great Britain: Traffic Signs Regulations and General Directions 2016. Keeps
    // left, sure; give way two broken lines (diagram 1003), sure; a crossing at
    // signals two rows of studs (diagram 1055.1), sure; a stop line at signals
    // 0.3 m (diagram 1001), fairly sure. Dashes vary by speed: the default.
    Rules {
        code: "GB",
        side: Side::Left,
        signalled: Signalled::Studs,
        give_way: GiveWay::Double,
        stop: 0.3,
        ..DEFAULT
    },
    // Ireland: Traffic Signs Manual (2019), chapter 7. Keeps left, sure; edges
    // yellow, fairly sure; a crossing at signals two rows of studs, fairly sure.
    Rules {
        code: "IE",
        side: Side::Left,
        signalled: Signalled::Studs,
        edge: Colour::Yellow,
        ..DEFAULT
    },
    // Italy: Regolamento del Codice della Strada (DPR 495/1992), art. 138 and 144:
    // dare precedenza a row of white triangles, fairly sure; lines in towns 3 m with
    // 4.5 m between, fairly sure.
    Rules {
        code: "IT",
        give_way: GiveWay::Triangles,
        dash: [3.0, 4.5],
        ..DEFAULT
    },
    // The Netherlands: RVV 1990 and CROW's Richtlijnen bebakening en markering:
    // haaientanden, sure; no zebra at signals but two broken lines, fairly sure;
    // lines 3 m with 9 m between, fairly sure.
    Rules {
        code: "NL",
        signalled: Signalled::Studs,
        give_way: GiveWay::Triangles,
        dash: [3.0, 9.0],
        ..DEFAULT
    },
    // Norway: Statens vegvesen, håndbok N302 Vegoppmerking: the line between
    // directions is yellow, edges white, sure.
    Rules {
        code: "NO",
        centre: Colour::Yellow,
        ..DEFAULT
    },
    // Poland: rozporządzenie Ministra Infrastruktury z 3 lipca 2003, załącznik nr 2
    // (znaki poziome): give way P-13, triangles, sure; at signals P-14, bars, fairly
    // sure; a zebra at signals (P-10), lines P-1b, 2 m with 4 m between, and stop
    // lines (P-12) 0.5 m, fairly sure.
    Rules {
        code: "PL",
        give_way: GiveWay::Triangles,
        signal_dashed: true,
        ..DEFAULT
    },
    // Slovakia: vyhláška 30/2020 Z. z. Not checked: the default.
    Rules {
        code: "SK",
        ..DEFAULT
    },
    // The United States: MUTCD (2009), part 3. Lines 10 ft with 30 ft between, the
    // line between directions yellow and so a one-way carriageway's left edge, yield
    // lines triangles, crosswalks two lines (many are ladders, tagged so), sure.
    Rules {
        code: "US",
        signalled: Signalled::Lines,
        give_way: GiveWay::Triangles,
        dash: [3.05, 9.15],
        inner: Colour::Yellow,
        centre: Colour::Yellow,
        ..DEFAULT
    },
];

/// Others that keep left, of which the table holds nothing else.
const LEFT: [&str; 22] = [
    "AU", "BD", "BW", "CY", "GG", "HK", "ID", "IM", "IN", "JE", "JM", "JP", "KE", "LK", "MO", "MT",
    "MY", "NZ", "PK", "SG", "TH", "ZA",
];

/// The look of the country with this code.
pub(super) fn rules(code: &str) -> &'static Rules {
    if let Some(rules) = TABLE.iter().find(|r| r.code == code) {
        return rules;
    }
    if LEFT.contains(&code) {
        return &KEEPS_LEFT;
    }
    &DEFAULT
}

/// The country code a relation's tags give, if it is a boundary that says one,
/// and its `admin_level`.
fn code<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<(String, u8)> {
    if get("boundary") != Some("administrative") {
        return None;
    }
    let level = get("admin_level").and_then(|l| l.trim().parse::<u8>().ok())?;
    let two = |v: &str| {
        let v = v.trim();
        (v.len() == 2 && v.bytes().all(|b| b.is_ascii_uppercase())).then(|| v.to_owned())
    };
    let country = match level {
        2 => get("ISO3166-1:alpha2")
            .and_then(two)
            .or_else(|| get("ISO3166-1").and_then(two)),
        _ => None,
    };
    let code = country.or_else(|| get("ISO3166-2")?.split_once('-').and_then(|c| two(c.0)))?;
    Some((code, level))
}

/// A boundary relation that says a country: its code, `admin_level`, id, and
/// member ways (all its rings are taken together, so holes are holes).
pub(super) struct Boundary {
    pub code: String,
    pub level: u8,
    pub id: i64,
    pub ways: Vec<i64>,
}

pub(super) fn boundary(relation: &NormalizedRelation) -> Option<Boundary> {
    let (code, level) = code(|key| relation.tags.get(key))?;
    let ways = (relation.members.iter())
        .filter(|m| m.member_type == RelationMemberType::Way)
        .map(|m| m.member_id)
        .collect();
    Some(Boundary {
        code,
        level,
        id: relation.relation_id,
        ways,
    })
}

/// World units a side of a cell: a z13 tile's.
const CELL: f64 = 32768.0;

type Cell = (i64, i64);

fn cell_of(p: Point) -> Cell {
    ((p[0] / CELL).floor() as i64, (p[1] / CELL).floor() as i64)
}

fn centre_of(c: Cell) -> Point {
    [(c.0 as f64 + 0.5) * CELL, (c.1 as f64 + 0.5) * CELL]
}

/// One country's outline, in world units, rings taken even-odd.
struct Outline {
    rules: &'static Rules,
    /// Its cells' first column and row, and how many columns.
    from: Cell,
    columns: usize,
    rows: usize,
    /// By cell, from the first row, whether its middle is within.
    within: Vec<bool>,
    edges: Vec<[Point; 2]>,
}

/// Which country a place is in, by the extract's own boundaries.
#[derive(Default)]
pub(super) struct Countries {
    /// The most sure first: countries before their parts, then by id.
    outlines: Vec<Outline>,
    /// By cell, the edges that reach it: outline and edge.
    near: FxHashMap<Cell, Vec<(u32, u32)>>,
}

/// Whether `a`-`b` crosses `c`-`d`, a point on the line counted on one side alone.
fn crosses(a: Point, b: Point, c: Point, d: Point) -> bool {
    let side = |p: Point, q: Point, r: Point| {
        (q[0] - p[0]) * (r[1] - p[1]) - (q[1] - p[1]) * (r[0] - p[0]) > 0.0
    };
    side(a, b, c) != side(a, b, d) && side(c, d, a) != side(c, d, b)
}

impl Countries {
    /// From `boundaries` and their rings, as points: those that close.
    pub(super) fn new(mut found: Vec<(&Boundary, Vec<Vec<Point>>)>) -> Self {
        found.sort_by_key(|(b, _)| (b.level != 2, b.id));
        let mut out = Self::default();
        for (boundary, rings) in found {
            let edges: Vec<[Point; 2]> = (rings.iter())
                .flat_map(|ring| ring.windows(2).map(|p| [p[0], p[1]]))
                .filter(|e| e[0] != e[1])
                .collect();
            if edges.is_empty() {
                continue;
            }
            let (mut low, mut high) = ([f64::MAX; 2], [f64::MIN; 2]);
            for p in edges.iter().flatten() {
                low = [low[0].min(p[0]), low[1].min(p[1])];
                high = [high[0].max(p[0]), high[1].max(p[1])];
            }
            let (from, to) = (cell_of(low), cell_of(high));
            let columns = (to.0 - from.0 + 1) as usize;
            let rows = (to.1 - from.1 + 1) as usize;
            // each row's middle line, and where the edges cross it
            let mut crossed: Vec<Vec<f64>> = vec![Vec::new(); rows];
            for &[a, b] in &edges {
                let (top, bottom) = (a[1].min(b[1]), a[1].max(b[1]));
                let first = ((top / CELL - 0.5).ceil() as i64).max(from.1);
                for row in first..=to.1 {
                    let y = (row as f64 + 0.5) * CELL;
                    if y >= bottom {
                        break;
                    }
                    if (a[1] > y) != (b[1] > y) {
                        let t = (y - a[1]) / (b[1] - a[1]);
                        crossed[(row - from.1) as usize].push(a[0] + t * (b[0] - a[0]));
                    }
                }
            }
            let mut within = vec![false; rows * columns];
            for (row, xs) in crossed.iter_mut().enumerate() {
                xs.sort_by(f64::total_cmp);
                for column in 0..columns {
                    let x = centre_of((from.0 + column as i64, 0))[0];
                    let left = xs.partition_point(|v| *v < x);
                    within[row * columns + column] = left % 2 == 1;
                }
            }
            let index = out.outlines.len() as u32;
            for (e, &[a, b]) in edges.iter().enumerate() {
                let (ca, cb) = (cell_of(a), cell_of(b));
                for x in ca.0.min(cb.0)..=ca.0.max(cb.0) {
                    for y in ca.1.min(cb.1)..=ca.1.max(cb.1) {
                        out.near.entry((x, y)).or_default().push((index, e as u32));
                    }
                }
            }
            out.outlines.push(Outline {
                rules: rules(&boundary.code),
                from,
                columns,
                rows,
                within,
                edges,
            });
        }
        out
    }

    /// The look of the country `p` is in: the first outline that holds it.
    pub(super) fn at(&self, p: Point) -> &'static Rules {
        let cell = cell_of(p);
        let centre = centre_of(cell);
        let near = self.near.get(&cell).map_or(&[][..], |v| &v[..]);
        for (i, outline) in self.outlines.iter().enumerate() {
            let (column, row) = (cell.0 - outline.from.0, cell.1 - outline.from.1);
            if column < 0
                || row < 0
                || column as usize >= outline.columns
                || row as usize >= outline.rows
            {
                continue;
            }
            let mut inside = outline.within[row as usize * outline.columns + column as usize];
            // from the cell's middle to the place, an edge between is in the cell
            for &(o, e) in near {
                if o == i as u32 {
                    let [a, b] = outline.edges[e as usize];
                    if crosses(centre, p, a, b) {
                        inside = !inside;
                    }
                }
            }
            if inside {
                return outline.rules;
            }
        }
        &DEFAULT
    }
}

/// The countries of `boundaries`, whose ways' nodes are in `outlined` and are
/// located by `locate`.
pub(super) fn countries(
    boundaries: &[Boundary],
    outlined: &FxHashMap<i64, Vec<i64>>,
    mut locate: impl FnMut(&[i64]) -> Option<Vec<Point>>,
) -> Countries {
    let found = boundaries
        .iter()
        .map(|b| {
            let lines = b.ways.iter().filter_map(|w| outlined.get(w).cloned());
            let rings = furniture::joined(lines.collect());
            let rings: Vec<Vec<Point>> = rings.iter().filter_map(|r| locate(r)).collect();
            (b, rings)
        })
        .filter(|(_, rings)| !rings.is_empty())
        .collect();
    Countries::new(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(
        code: &str,
        level: u8,
        id: i64,
        [x, y, side]: [f64; 3],
    ) -> (Boundary, Vec<Vec<Point>>) {
        let boundary = Boundary {
            code: code.into(),
            level,
            id,
            ways: Vec::new(),
        };
        let ring = vec![
            [x, y],
            [x + side, y],
            [x + side, y + side],
            [x, y + side],
            [x, y],
        ];
        (boundary, vec![ring])
    }

    #[test]
    fn the_table_holds_its_countries_and_the_default_the_rest() {
        assert_eq!(rules("GB").side, Side::Left);
        assert_eq!(rules("GB").give_way, GiveWay::Double);
        assert_eq!(rules("PL").give_way, GiveWay::Triangles);
        assert!(rules("PL").signal_dashed);
        assert_eq!(rules("DE").signalled, Signalled::Lines);
        assert_eq!(rules("DE").pattern(), [3.0, 9.0]);
        assert_eq!(rules("US").centre, Colour::Yellow);
        assert_eq!(rules("IE").edge, Colour::Yellow);
        assert_eq!(rules("JP").side, Side::Left);
        assert_eq!(rules("JP").give_way, GiveWay::Bars);
        assert_eq!(rules("XX"), &DEFAULT);
        assert_eq!(rules("DE").dash_name().as_deref(), Some("3-6"));
        assert_eq!(rules("IT").dash_name().as_deref(), Some("3-4.5"));
        assert_eq!(rules("PL").dash_name(), None);
        for (i, r) in TABLE.iter().enumerate() {
            assert!(TABLE[i + 1..].iter().all(|o| o.code != r.code) && !LEFT.contains(&r.code));
            assert!(r.stop > 0.0 && r.dash[0] > 0.0 && r.dash[1] > 0.0);
        }
    }

    #[test]
    fn a_boundary_says_its_country_or_its_parts() {
        let read =
            |tags: &[(&str, &'static str)]| code(|k| tags.iter().find(|t| t.0 == k).map(|t| t.1));
        let country = [
            ("boundary", "administrative"),
            ("admin_level", "2"),
            ("ISO3166-1", "PL"),
        ];
        assert_eq!(read(&country), Some(("PL".into(), 2)));
        let state = [
            ("boundary", "administrative"),
            ("admin_level", "4"),
            ("ISO3166-2", "DE-BE"),
        ];
        assert_eq!(read(&state), Some(("DE".into(), 4)));
        // a country's code on another level, and other boundaries, say nothing
        let odd = [
            ("boundary", "administrative"),
            ("admin_level", "4"),
            ("ISO3166-1", "PL"),
        ];
        assert_eq!(read(&odd), None);
        let postal = [
            ("boundary", "postal_code"),
            ("admin_level", "2"),
            ("ISO3166-1", "PL"),
        ];
        assert_eq!(read(&postal), None);
        let lower = [
            ("boundary", "administrative"),
            ("admin_level", "2"),
            ("ISO3166-1", "pl"),
        ];
        assert_eq!(read(&lower), None);
    }

    #[test]
    fn a_place_is_in_the_first_outline_that_holds_it() {
        // a state over two cells' corner, and a country beside it with a hole
        let state = square("DE", 4, 9, [CELL * 0.6, CELL * 0.6, CELL]);
        let (gb, mut rings) = square("GB", 2, 7, [CELL * 3.0, 0.0, CELL * 2.0]);
        rings.push(vec![
            [CELL * 3.3, CELL * 0.3],
            [CELL * 3.3, CELL * 1.2],
            [CELL * 4.3, CELL * 1.2],
            [CELL * 4.3, CELL * 0.3],
            [CELL * 3.3, CELL * 0.3],
        ]);
        let countries = Countries::new(vec![(&state.0, state.1.clone()), (&gb, rings)]);
        assert_eq!(countries.at([CELL * 0.7, CELL * 0.7]).code, "DE");
        assert_eq!(countries.at([CELL * 1.5, CELL * 1.5]).code, "DE");
        assert_eq!(countries.at([CELL * 1.59, CELL * 0.61]).code, "DE");
        assert_eq!(countries.at([CELL * 0.59, CELL * 1.0]), &DEFAULT);
        assert_eq!(countries.at([CELL * 1.61, CELL * 1.0]), &DEFAULT);
        assert_eq!(countries.at([CELL * 3.2, CELL * 1.0]).code, "GB");
        assert_eq!(countries.at([CELL * 4.0, CELL * 1.0]), &DEFAULT);
        assert_eq!(countries.at([CELL * 4.0, CELL * 1.7]).code, "GB");
        assert_eq!(countries.at([CELL * 9.0, CELL * 9.0]), &DEFAULT);
        assert_eq!(Countries::new(Vec::new()).at([0.0, 0.0]), &DEFAULT);
    }
}
