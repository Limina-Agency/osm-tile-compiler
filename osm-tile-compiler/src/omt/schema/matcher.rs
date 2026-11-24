//! A schema's structured expressions: boolean tag tests (`include_when`,
//! `exclude_when`, `if`) and value expressions (`value`, `min_zoom`, match
//! blocks, `coalesce`, `tag_value`), with Planetiler's semantics.
use super::expr::{self, Expr, Scope};
use super::value::{DataType, Val, convert};
use super::yaml::Node;

/// A boolean expression over a feature's tags.
#[derive(Debug, Clone)]
pub(crate) enum Cond {
    True,
    False,
    And(Vec<Cond>),
    Or(Vec<Cond>),
    Not(Box<Cond>),
    /// The tag has a value (not empty).
    Field(String),
    /// The tag (or a script's value) is one of some values.
    Any(MatchAny),
    Script(Expr),
}

#[derive(Debug, Clone)]
pub(crate) struct MatchAny {
    /// The tag; `None` when the key is a script, whose value is matched.
    key: Option<String>,
    script: Option<Expr>,
    exact: Vec<String>,
    patterns: Vec<Wildcard>,
    /// Whether a missing or empty value matches (the values hold `''`).
    when_missing: bool,
}

/// A value with `%` wildcards: `motorway%`, `%_link`.
#[derive(Debug, Clone)]
struct Wildcard {
    /// The literal parts between wildcards.
    parts: Vec<String>,
    leading: bool,
    trailing: bool,
}

impl Wildcard {
    fn parse(value: &str) -> Option<Self> {
        let mut parts = vec![String::new()];
        let mut chars = value.chars().peekable();
        let mut wild = false;
        while let Some(c) = chars.next() {
            if c == '\\' && chars.peek() == Some(&'%') {
                chars.next();
                parts.last_mut().expect("nonempty").push('%');
            } else if c == '%' {
                wild = true;
                parts.push(String::new());
            } else {
                parts.last_mut().expect("nonempty").push(c);
            }
        }
        if !wild {
            return None;
        }
        let leading = parts.first().is_some_and(String::is_empty);
        let trailing = parts.last().is_some_and(String::is_empty);
        parts.retain(|p| !p.is_empty());
        Some(Self {
            parts,
            leading,
            trailing,
        })
    }

    /// `^part1.*part2.*part3$`, with `.*` at the ends when the value starts
    /// or ends with `%`.
    fn matches(&self, text: &str) -> bool {
        let mut rest = text;
        let last = self.parts.len();
        for (i, part) in self.parts.iter().enumerate() {
            if i == 0 && !self.leading {
                let Some(r) = rest.strip_prefix(part.as_str()) else {
                    return false;
                };
                rest = r;
                if last == 1 && !self.trailing {
                    return rest.is_empty();
                }
                continue;
            }
            if i + 1 == last && !self.trailing {
                return rest.len() >= part.len() && rest.ends_with(part.as_str());
            }
            match rest.find(part.as_str()) {
                Some(at) => rest = &rest[at + part.len()..],
                None => return false,
            }
        }
        true
    }
}

/// What a test can read about a feature. The keys that made a test match
/// are recorded for `match_key` and `match_value`.
pub(crate) type MatchKeys = Vec<Option<String>>;

fn is_keyword(text: &str, word: &str) -> bool {
    text.trim().eq_ignore_ascii_case(word)
}

/// Whether `node` is a `${ ... }` script, and its text.
pub(crate) fn script(node: &Node) -> Option<&str> {
    let text = node.as_str()?.trim();
    text.strip_prefix("${")?.strip_suffix('}')
}

/// `\${...}` and `\__any__` stand for the text without the backslash.
fn unescape(node: &Node) -> Node {
    match node {
        Node::Str(s) => {
            let trimmed = s.trim_start_matches([' ', '\t']);
            let lead = s.len() - trimmed.len();
            for word in ["\\${", "\\__any__", "\\__all__"] {
                if trimmed
                    .get(..word.len())
                    .is_some_and(|start| start.eq_ignore_ascii_case(word))
                {
                    return Node::Str(format!("{}{}", &s[..lead], &trimmed[1..]));
                }
            }
            node.clone()
        }
        _ => node.clone(),
    }
}

fn parse_script(text: &str) -> Result<Expr, String> {
    expr::parse(text).map_err(|e| format!("invalid script '${{{text}}}': {e}"))
}

impl Cond {
    /// Parses a boolean expression; maps and lists are any-of.
    pub fn parse(node: &Node) -> Result<Self, String> {
        Self::parse_with(node, false)
    }

    fn parse_with(node: &Node, all: bool) -> Result<Self, String> {
        let collect = |items: Vec<Cond>| {
            if all {
                Cond::And(items)
            } else {
                Cond::Or(items)
            }
        };
        let cond = match node {
            Node::Null => Cond::False,
            Node::Str(s) if is_keyword(s, "__any__") => Cond::True,
            _ if script(node).is_some() => {
                Cond::Script(parse_script(script(node).expect("checked"))?)
            }
            Node::Map(entries) => collect(
                entries
                    .iter()
                    .map(|(key, value)| Self::criterion(key, value))
                    .collect::<Result<_, _>>()?,
            ),
            Node::List(items) => collect(
                items
                    .iter()
                    .map(|item| Self::parse_with(item, false))
                    .collect::<Result<_, _>>()?,
            ),
            other => {
                return Err(format!(
                    "a tag test must be a map, a list or a script, not {} ({})",
                    other.kind(),
                    other.text()
                ));
            }
        };
        Ok(cond.simplify())
    }

    fn criterion(key: &str, value: &Node) -> Result<Self, String> {
        let list_or_map = matches!(value, Node::List(_) | Node::Map(_));
        if is_keyword(key, "__any__") && list_or_map {
            return Self::parse_with(value, false);
        }
        if is_keyword(key, "__all__") && list_or_map {
            return Self::parse_with(value, true);
        }
        if is_keyword(key, "__not__") {
            return Ok(Cond::Not(Box::new(Self::parse_with(value, false)?)));
        }
        let any_value = |v: &Node| !v.is_null() && is_keyword(&v.text(), "__any__");
        let is_any = match value {
            Node::Null => true,
            Node::List(items) => items.iter().any(any_value),
            other => any_value(other),
        };
        let values: Vec<Node> = match value {
            Node::List(items) => items.iter().map(unescape).collect(),
            Node::Null => Vec::new(),
            other => vec![unescape(other)],
        };
        let key_node = Node::Str(key.into());
        if let Some(text) = script(&key_node) {
            let script = parse_script(text)?;
            let values = if is_any { Vec::new() } else { values };
            return Ok(Cond::Any(MatchAny::new(None, Some(script), &values)));
        }
        let field = unescape(&key_node).text();
        Ok(if is_any {
            Cond::Field(field)
        } else {
            Cond::Any(MatchAny::new(Some(field), None, &values))
        })
    }

    /// Removes the trivial nesting the parser makes.
    fn simplify(self) -> Self {
        match self {
            Cond::And(items) => {
                let mut items = items
                    .into_iter()
                    .map(Cond::simplify)
                    .filter(|c| !matches!(c, Cond::True))
                    .collect::<Vec<_>>();
                if items.iter().any(|c| matches!(c, Cond::False)) {
                    return Cond::False;
                }
                match items.len() {
                    0 => Cond::True,
                    1 => items.pop().expect("one"),
                    _ => Cond::And(items),
                }
            }
            Cond::Or(items) => {
                let mut items = items
                    .into_iter()
                    .map(Cond::simplify)
                    .filter(|c| !matches!(c, Cond::False))
                    .collect::<Vec<_>>();
                match items.len() {
                    0 => Cond::False,
                    1 => items.pop().expect("one"),
                    _ => Cond::Or(items),
                }
            }
            Cond::Not(inner) => match inner.simplify() {
                Cond::True => Cond::False,
                Cond::False => Cond::True,
                other => Cond::Not(Box::new(other)),
            },
            other => other,
        }
    }

    /// Evaluates the test, adding the keys that matched to `keys`.
    pub fn eval(&self, scope: &dyn Scope, keys: &mut MatchKeys) -> bool {
        match self {
            Cond::True => true,
            Cond::False => false,
            Cond::And(items) => {
                for item in items {
                    if !item.eval(scope, keys) {
                        keys.clear();
                        return false;
                    }
                }
                true
            }
            Cond::Or(items) => items.iter().any(|item| item.eval(scope, keys)),
            Cond::Not(inner) => !inner.eval(scope, &mut Vec::new()),
            Cond::Field(key) => {
                let present = scope.tag(key).is_some_and(|v| !v.is_empty());
                if present {
                    keys.push(Some(key.clone()));
                }
                present
            }
            Cond::Any(any) => any.eval(scope, keys),
            Cond::Script(script) => expr::eval(script, scope) == Val::Bool(true),
        }
    }

    /// Keys one of which a feature must have for the test to match, or
    /// `None` when it can match without any.
    pub fn required_keys(&self) -> Option<Vec<String>> {
        match self {
            Cond::False => Some(Vec::new()),
            Cond::Field(key) => Some(vec![key.clone()]),
            Cond::Any(MatchAny {
                key: Some(key),
                when_missing: false,
                ..
            }) => Some(vec![key.clone()]),
            Cond::Or(items) => {
                let mut all = Vec::new();
                for item in items {
                    all.extend(item.required_keys()?);
                }
                Some(all)
            }
            Cond::And(items) => items
                .iter()
                .filter_map(Cond::required_keys)
                .min_by_key(Vec::len),
            _ => None,
        }
    }
}

impl MatchAny {
    fn new(key: Option<String>, script: Option<Expr>, values: &[Node]) -> Self {
        let mut exact = Vec::new();
        let mut patterns = Vec::new();
        for value in values.iter().filter(|v| !v.is_null()) {
            let text = value.text();
            match Wildcard::parse(&text) {
                Some(wildcard) => patterns.push(wildcard),
                None => exact.push(text.replace("\\%", "%")),
            }
        }
        let when_missing = values
            .iter()
            .any(|v| v.is_null() || matches!(v, Node::Str(s) if s.is_empty()));
        Self {
            key,
            script,
            exact,
            patterns,
            when_missing,
        }
    }

    fn eval(&self, scope: &dyn Scope, keys: &mut MatchKeys) -> bool {
        let value = match (&self.key, &self.script) {
            (Some(key), _) => scope
                .tag(key)
                .map(|v| Val::Str(v.into()))
                .unwrap_or(Val::Null),
            (None, Some(script)) => expr::eval(script, scope),
            (None, None) => Val::Null,
        };
        self.eval_value(&value, keys)
    }

    fn eval_value(&self, value: &Val, keys: &mut MatchKeys) -> bool {
        match value {
            Val::Null => self.when_missing,
            Val::Str(s) if s.is_empty() => self.when_missing,
            Val::List(items) if items.is_empty() => self.when_missing,
            Val::List(items) => items.iter().any(|item| self.eval_value(item, keys)),
            Val::Map(_) => false,
            other => {
                let text = match other {
                    Val::Str(s) => std::borrow::Cow::Borrowed(s.as_str()),
                    v => std::borrow::Cow::Owned(v.text()),
                };
                let hit = self.exact.iter().any(|e| *e == *text)
                    || (self.key.is_none() && self.exact.is_empty() && self.patterns.is_empty())
                    || self.patterns.iter().any(|p| p.matches(&text));
                if hit {
                    keys.push(self.key.clone());
                }
                hit
            }
        }
    }
}

/// What a value expression should produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Output {
    Any,
    Integer,
    Double,
    String,
}

impl Output {
    fn convert(self, value: Val) -> Val {
        let kind = match self {
            Output::Any => return value,
            Output::Integer => DataType::Integer,
            Output::Double => DataType::Double,
            Output::String => DataType::String,
        };
        convert(&value, kind).unwrap_or(Val::Null)
    }
}

/// An expression that computes a value.
#[derive(Debug, Clone)]
pub(crate) enum ValueExpr {
    Const(Val),
    Script(Expr, Output),
    /// `tag_value:` the tag whose key the inner expression gives.
    Tag(Box<ValueExpr>, Output),
    /// `arg_value:` an argument of the schema.
    Arg(Box<ValueExpr>, Output),
    Coalesce(Vec<ValueExpr>),
    /// The value of the first case whose test matches, else the fallback.
    Match(Vec<(ValueExpr, Cond)>, Box<ValueExpr>),
    Cast(Box<ValueExpr>, DataType, Output),
}

impl ValueExpr {
    pub fn parse(node: &Node, output: Output) -> Result<Self, String> {
        Ok(match node {
            Node::Null => ValueExpr::Const(Val::Null),
            _ if script(node).is_some() => {
                ValueExpr::Script(parse_script(script(node).expect("checked"))?, output)
            }
            Node::List(_) => Self::parse_match(node, true, output)?,
            Node::Map(entries) => {
                let keys = entries.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>();
                if let Some(kind) = node.get("type").filter(|t| !t.is_null()) {
                    let name = kind.text();
                    let data_type = DataType::from_name(&name).ok_or_else(|| {
                        format!(
                            "unknown type '{name}'; the types are boolean, string, direction, \
                             long, integer and double"
                        )
                    })?;
                    let rest = Node::Map(
                        entries
                            .iter()
                            .filter(|(k, _)| k != "type")
                            .cloned()
                            .collect(),
                    );
                    return Ok(ValueExpr::Cast(
                        Box::new(Self::parse(&rest, Output::Any)?),
                        data_type,
                        output,
                    ));
                }
                match keys.as_slice() {
                    ["coalesce"] if matches!(node.get("coalesce"), Some(Node::List(_))) => {
                        let Some(Node::List(items)) = node.get("coalesce") else {
                            unreachable!()
                        };
                        ValueExpr::Coalesce(
                            items
                                .iter()
                                .map(|item| Self::parse(item, output))
                                .collect::<Result<_, _>>()?,
                        )
                    }
                    ["match"] => Self::parse_match(node.get("match").expect("key"), true, output)?,
                    ["default_value", "overrides"] | ["overrides", "default_value"] => {
                        let ValueExpr::Match(cases, _) =
                            Self::parse_match(node.get("overrides").expect("key"), false, output)?
                        else {
                            unreachable!()
                        };
                        let fallback =
                            Self::parse(node.get("default_value").expect("key"), output)?;
                        ValueExpr::Match(cases, Box::new(fallback))
                    }
                    ["tag_value"] => ValueExpr::Tag(
                        Box::new(Self::parse(
                            node.get("tag_value").expect("key"),
                            Output::String,
                        )?),
                        output,
                    ),
                    ["arg_value"] => ValueExpr::Arg(
                        Box::new(Self::parse(
                            node.get("arg_value").expect("key"),
                            Output::String,
                        )?),
                        output,
                    ),
                    ["value"] => Self::parse(node.get("value").expect("key"), output)?,
                    [] => ValueExpr::Const(Val::Null),
                    _ => Self::parse_match(node, true, output)
                        .map_err(|e| format!("could not read {}: {e}", node.text()))?,
                }
            }
            scalar => ValueExpr::Const(output.convert(unescape(scalar).to_val())),
        })
    }

    fn parse_match(node: &Node, allow_else: bool, output: Output) -> Result<Self, String> {
        let mut cases = Vec::new();
        let mut fallback = ValueExpr::Const(Val::Null);
        match node {
            Node::List(items) => {
                for item in items {
                    let Node::Map(entries) = item else { continue };
                    let mut keys = entries.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>();
                    keys.sort_unstable();
                    if keys == ["if", "value"] {
                        cases.push((
                            Self::parse(item.get("value").expect("key"), output)?,
                            Cond::parse(item.get("if").expect("key"))?,
                        ));
                    } else if allow_else && keys == ["else"] {
                        fallback = Self::parse(item.get("else").expect("key"), output)?;
                        break;
                    } else {
                        return Err(format!(
                            "a match case needs 'if' and 'value'{}, not {}",
                            if allow_else { " (or 'else')" } else { "" },
                            item.text()
                        ));
                    }
                }
            }
            Node::Map(entries) => {
                for (key, value) in entries {
                    let text = value.text();
                    let bare = text.trim_matches('_');
                    if matches!(bare, "default_value" | "otherwise" | "default") {
                        fallback = Self::parse(&Node::Str(key.clone()), output)?;
                    } else {
                        cases.push((
                            Self::parse(&Node::Str(key.clone()), output)?,
                            Cond::parse(value)?,
                        ));
                    }
                }
            }
            other => {
                return Err(format!(
                    "a match block must be a list or a map, not {}",
                    other.kind()
                ));
            }
        }
        Ok(ValueExpr::Match(cases, Box::new(fallback)))
    }

    pub fn eval(&self, scope: &dyn Scope) -> Val {
        match self {
            ValueExpr::Const(v) => v.clone(),
            ValueExpr::Script(script, output) => output.convert(expr::eval(script, scope)),
            ValueExpr::Tag(key, output) => match key.eval(scope) {
                Val::Str(key) => {
                    output.convert(scope.tag(&key).map_or(Val::Null, |v| Val::Str(v.into())))
                }
                _ => Val::Null,
            },
            ValueExpr::Arg(key, output) => match key.eval(scope) {
                Val::Str(key) => {
                    let args = scope.variable("args");
                    let value = match args {
                        Some(Val::Map(map)) => map.get(&key).cloned().unwrap_or(Val::Null),
                        _ => Val::Null,
                    };
                    output.convert(value)
                }
                _ => Val::Null,
            },
            ValueExpr::Coalesce(items) => items
                .iter()
                .map(|item| item.eval(scope))
                .find(|v| *v != Val::Null)
                .unwrap_or(Val::Null),
            ValueExpr::Match(cases, fallback) => {
                for (value, test) in cases {
                    if test.eval(scope, &mut Vec::new()) {
                        return value.eval(scope);
                    }
                }
                fallback.eval(scope)
            }
            ValueExpr::Cast(inner, data_type, output) => {
                let value = inner.eval(scope);
                output.convert(convert(&value, *data_type).unwrap_or(Val::Null))
            }
        }
    }

    /// The value, when it is the same for every feature.
    pub fn constant(&self) -> Option<&Val> {
        match self {
            ValueExpr::Const(v) => Some(v),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    struct Tags(Vec<(&'static str, &'static str)>);

    impl Scope for Tags {
        fn tag(&self, key: &str) -> Option<&str> {
            self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
        }
        fn tags(&self) -> BTreeMap<String, Val> {
            BTreeMap::new()
        }
        fn feature(&self, _: &str) -> Option<Val> {
            None
        }
        fn feature_call(&self, _: &str, _: &[Val]) -> Option<Val> {
            None
        }
        fn variable(&self, _: &str) -> Option<Val> {
            None
        }
    }

    fn test(yaml: &str, tags: Vec<(&'static str, &'static str)>) -> (bool, MatchKeys) {
        let cond = Cond::parse(&super::super::yaml::parse(yaml).unwrap()).unwrap();
        let mut keys = Vec::new();
        let hit = cond.eval(&Tags(tags), &mut keys);
        (hit, keys)
    }

    #[test]
    fn tag_tests_follow_planetilers_boolean_expressions() {
        let key = |k: &str| vec![Some(k.to_string())];
        // A map is any-of, in order; the first key that matched is recorded.
        let yaml = "amenity:\nlanduse: [forest, '%land']\nnatural: wood";
        assert_eq!(
            test(yaml, vec![("natural", "wood")]),
            (true, key("natural"))
        );
        assert_eq!(
            test(yaml, vec![("landuse", "forest"), ("natural", "wood")]),
            (true, key("landuse"))
        );
        assert_eq!(
            test(yaml, vec![("landuse", "farmland")]),
            (true, key("landuse"))
        );
        assert!(!test(yaml, vec![("landuse", "x")]).0);
        // `''` matches a missing tag, without recording it.
        assert_eq!(test("name: ''", vec![]), (true, vec![]));
        assert!(!test("name: ''", vec![("name", "x")]).0);
        // __all__, __not__ and wildcards.
        let yaml = "__all__:\n  place: [city, town]\n  __not__:\n    capital: [yes, '4']";
        assert!(test(yaml, vec![("place", "city")]).0);
        assert!(!test(yaml, vec![("place", "city"), ("capital", "yes")]).0);
        assert!(test("highway: motorway%", vec![("highway", "motorway_link")]).0);
        assert!(!test("highway: motorway%", vec![("highway", "trunk")]).0);
        assert!(test("highway: '%_link'", vec![("highway", "trunk_link")]).0);
        assert!(test("highway: 'a%b%c'", vec![("highway", "axbyc")]).0);
        assert!(!test("highway: 'a%b%c'", vec![("highway", "axbyd")]).0);
        assert!(test("highway: 'a\\%'", vec![("highway", "a%")]).0);
        // Numbers and booleans compare as their text.
        assert!(test("admin_level: [2, 4]", vec![("admin_level", "4")]).0);
        assert!(test("oneway: [yes, 1, true, -1]", vec![("oneway", "true")]).0);
        // A failed __all__ forgets the keys it had matched.
        assert_eq!(
            test("__all__: [{a: x}, {b: y}]", vec![("a", "x")]),
            (false, vec![])
        );
        assert!(test("building: __any__", vec![("building", "yes")]).0);
        assert!(!test("building: __any__", vec![("building", "")]).0);
    }

    #[test]
    fn value_expressions_pick_the_first_match() {
        let parse = |yaml: &str, output| {
            ValueExpr::parse(&super::super::yaml::parse(yaml).unwrap(), output).unwrap()
        };
        let zoom = parse(
            "default_value: 13\noverrides:\n  5: {highway: motorway%}\n  6: {highway: trunk%}",
            Output::Integer,
        );
        assert_eq!(
            zoom.eval(&Tags(vec![("highway", "trunk_link")])),
            Val::Int(6)
        );
        assert_eq!(zoom.eval(&Tags(vec![])), Val::Int(13));
        let population = parse(
            "match:\n- value: 100000\n  if: {place: city}\n- else: 0",
            Output::Any,
        );
        assert_eq!(
            population.eval(&Tags(vec![("place", "city")])),
            Val::Int(100000)
        );
        assert_eq!(population.eval(&Tags(vec![])), Val::Int(0));
        let kind = parse(
            "coalesce:\n- tag_value: aeroway\n- tag_value: amenity\n- aerialway_station",
            Output::Any,
        );
        assert_eq!(
            kind.eval(&Tags(vec![("amenity", "x")])),
            Val::Str("x".into())
        );
        assert_eq!(
            kind.eval(&Tags(vec![])),
            Val::Str("aerialway_station".into())
        );
        let map_form = parse(
            "farmland: {subclass: [farm]}\nwater: otherwise",
            Output::Any,
        );
        assert_eq!(
            map_form.eval(&Tags(vec![("subclass", "farm")])),
            Val::Str("farmland".into())
        );
        assert_eq!(map_form.eval(&Tags(vec![])), Val::Str("water".into()));
        let cast = parse("tag_value: oneway\ntype: direction", Output::Any);
        assert_eq!(cast.eval(&Tags(vec![("oneway", "-1")])), Val::Int(-1));
    }
}
