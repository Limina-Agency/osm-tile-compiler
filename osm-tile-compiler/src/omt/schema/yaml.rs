//! Reads a schema's YAML (or JSON, which YAML 1.2 includes) into a tree that
//! keeps the order of every map, which the schema's semantics depend on: the
//! first matching override wins, and the first key that matched is
//! `match_key`.
use super::value::Val;
use std::collections::HashMap;
use yaml_rust2::Yaml;
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser, Tag};
use yaml_rust2::scanner::{Marker, TScalarStyle};

/// A parsed YAML node.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(String),
    List(Vec<Node>),
    /// Entries in the order the file gives them.
    Map(Vec<(String, Node)>),
}

/// Documents larger than this are refused: a schema is a few hundred KiB at
/// most, and the limit keeps a hostile one from exhausting a phone's memory.
pub(crate) const MAX_SCHEMA_BYTES: usize = 8 * 1024 * 1024;
/// Aliases expand into copies, so a few nested ones can describe a huge
/// tree ("billion laughs"); the expanded tree may have at most this many
/// nodes.
const MAX_NODES: usize = 2_000_000;
const MAX_DEPTH: usize = 128;

/// Parses one YAML document.
pub(crate) fn parse(text: &str) -> Result<Node, String> {
    if text.len() > MAX_SCHEMA_BYTES {
        return Err(format!(
            "the schema is {} bytes; at most {MAX_SCHEMA_BYTES} are read",
            text.len()
        ));
    }
    let mut builder = Builder::default();
    Parser::new_from_str(text)
        .load(&mut builder, true)
        .map_err(|e| format!("invalid YAML: {e}"))?;
    if let Some(error) = builder.error {
        return Err(error);
    }
    match builder.documents.len() {
        0 => Ok(Node::Null),
        1 => Ok(builder.documents.pop().unwrap_or(Node::Null)),
        _ => Err("the schema must be a single YAML document".into()),
    }
}

/// Builds nodes from the parser's events. Unlike `yaml_rust2::YamlLoader`
/// it counts what each alias would copy before copying it, so an alias bomb
/// fails at once instead of filling memory.
#[derive(Default)]
struct Builder {
    documents: Vec<Node>,
    /// Open lists and maps, with their anchor and, for a map, the key read.
    stack: Vec<(Node, usize, Option<String>)>,
    /// Anchored nodes and how many nodes each holds.
    anchors: HashMap<usize, (Node, usize)>,
    /// Nodes made so far, aliases counted at their full size.
    nodes: usize,
    error: Option<String>,
}

impl Builder {
    fn fail(&mut self, message: &str) {
        if self.error.is_none() {
            self.error = Some(message.into());
        }
    }

    fn insert(&mut self, node: Node, anchor: usize, size: usize) {
        if anchor > 0 {
            self.anchors.insert(anchor, (node.clone(), size));
        }
        match self.stack.last_mut() {
            None => self.documents.push(node),
            Some((Node::List(items), ..)) => items.push(node),
            Some((Node::Map(entries), _, key)) => match key.take() {
                None => match node_key(&node) {
                    Some(k) => *key = Some(k),
                    None => self.fail("map keys must be scalars"),
                },
                Some(k) => entries.push((k, node)),
            },
            Some(_) => self.fail("unexpected YAML structure"),
        }
    }
}

fn node_key(node: &Node) -> Option<String> {
    match node {
        Node::Str(s) => Some(s.clone()),
        Node::Int(i) => Some(i.to_string()),
        Node::Double(d) => Some(d.to_string()),
        Node::Bool(b) => Some(b.to_string()),
        Node::Null => Some("null".into()),
        Node::List(_) | Node::Map(_) => None,
    }
}

/// How many nodes `node` holds, itself included.
fn size(node: &Node) -> usize {
    match node {
        Node::List(items) => 1 + items.iter().map(size).sum::<usize>(),
        Node::Map(entries) => 1 + entries.iter().map(|(_, v)| size(v)).sum::<usize>(),
        _ => 1,
    }
}

impl MarkedEventReceiver for Builder {
    fn on_event(&mut self, event: Event, _: Marker) {
        if self.error.is_some() {
            return;
        }
        match event {
            Event::Scalar(value, style, anchor, tag) => {
                self.nodes += 1;
                let node = scalar(value, style, tag.as_ref());
                self.insert(node, anchor, 1);
            }
            Event::SequenceStart(anchor, _) | Event::MappingStart(anchor, _) => {
                if self.stack.len() >= MAX_DEPTH {
                    return self.fail("the schema nests too deeply");
                }
                self.nodes += 1;
                let node = if matches!(event, Event::SequenceStart(..)) {
                    Node::List(Vec::new())
                } else {
                    Node::Map(Vec::new())
                };
                self.stack.push((node, anchor, None));
            }
            Event::SequenceEnd | Event::MappingEnd => {
                let Some((node, anchor, key)) = self.stack.pop() else {
                    return self.fail("unexpected end of a YAML list or map");
                };
                if key.is_some() {
                    return self.fail("a map key has no value");
                }
                let n = if anchor > 0 { size(&node) } else { 0 };
                self.insert(node, anchor, n);
            }
            Event::Alias(anchor) => {
                let Some((node, n)) = self.anchors.get(&anchor).cloned() else {
                    return self.fail("unknown YAML alias");
                };
                self.nodes = self.nodes.saturating_add(n);
                if self.nodes > MAX_NODES {
                    return self.fail("the schema expands to too many values (through aliases)");
                }
                self.insert(node, 0, n);
            }
            Event::Nothing
            | Event::StreamStart
            | Event::StreamEnd
            | Event::DocumentStart
            | Event::DocumentEnd => {}
        }
        if self.nodes > MAX_NODES {
            self.fail("the schema expands to too many values (through aliases)");
        }
    }
}

/// A scalar as YAML 1.2's core schema reads it: plain scalars may be null,
/// booleans or numbers; quoted ones are strings.
fn scalar(value: String, style: TScalarStyle, tag: Option<&Tag>) -> Node {
    if style != TScalarStyle::Plain {
        return Node::Str(value);
    }
    if let Some(tag) = tag
        && tag.handle == "tag:yaml.org,2002:"
        && tag.suffix == "str"
    {
        return Node::Str(value);
    }
    match Yaml::from_str(&value) {
        Yaml::Null => Node::Null,
        Yaml::Boolean(b) => Node::Bool(b),
        Yaml::Integer(i) => Node::Int(i),
        real @ Yaml::Real(_) => real.as_f64().map_or(Node::Str(value), Node::Double),
        _ => Node::Str(value),
    }
}

impl Node {
    pub fn get(&self, key: &str) -> Option<&Node> {
        match self {
            Node::Map(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Node::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Node::Null)
    }

    /// The node as a constant value.
    pub fn to_val(&self) -> Val {
        match self {
            Node::Null => Val::Null,
            Node::Bool(b) => Val::Bool(*b),
            Node::Int(i) => Val::Int(*i),
            Node::Double(d) => Val::Double(*d),
            Node::Str(s) => Val::Str(s.clone()),
            Node::List(items) => Val::List(items.iter().map(Node::to_val).collect()),
            Node::Map(entries) => Val::Map(
                entries
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_val()))
                    .collect(),
            ),
        }
    }

    /// The node as Java's `Objects.toString` writes it.
    pub fn text(&self) -> String {
        self.to_val().text()
    }

    /// What kind of node this is, for error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Node::Null => "null",
            Node::Bool(_) => "a boolean",
            Node::Int(_) | Node::Double(_) => "a number",
            Node::Str(_) => "a string",
            Node::List(_) => "a list",
            Node::Map(_) => "a map",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_map_order_and_resolves_aliases() {
        let node =
            parse("a: &x [1, yes, true, -1, 2e6, '']\nz: 1\nb: *x\n10: ten\nempty:\n").unwrap();
        let Node::Map(entries) = &node else { panic!() };
        let keys = entries.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>();
        assert_eq!(keys, ["a", "z", "b", "10", "empty"]);
        // YAML 1.2: `yes` is a string, `true` a boolean.
        let expected = Node::List(vec![
            Node::Int(1),
            Node::Str("yes".into()),
            Node::Bool(true),
            Node::Int(-1),
            Node::Double(2e6),
            Node::Str(String::new()),
        ]);
        assert_eq!(node.get("a"), Some(&expected));
        assert_eq!(node.get("b"), Some(&expected));
        assert_eq!(node.get("empty"), Some(&Node::Null));
    }

    #[test]
    fn refuses_alias_bombs_and_bad_documents() {
        let mut bomb = String::from("a: &a [x, x, x, x, x, x, x, x, x, x]\n");
        for (i, name) in ["b", "c", "d", "e", "f", "g", "h"].iter().enumerate() {
            let previous = if i == 0 {
                "a".to_string()
            } else {
                ["b", "c", "d", "e", "f", "g"][i - 1].to_string()
            };
            bomb.push_str(&format!(
                "{name}: &{name} [*{previous}, *{previous}, *{previous}, *{previous}, *{previous}, *{previous}, *{previous}, *{previous}, *{previous}, *{previous}]\n"
            ));
        }
        assert!(parse(&bomb).unwrap_err().contains("too many"));
        assert!(parse("a: [1, 2").is_err());
        assert!(parse("a: 1\n---\nb: 2\n").is_err());
    }
}
