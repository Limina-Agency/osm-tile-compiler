//! Inline script expressions, `${ ... }`: the part of the Common Expression
//! Language (CEL) that Planetiler's configurable schemas use.
//!
//! The language has no loops, assignments or recursion: the only iteration is
//! the `map`, `filter`, `exists` and `all` macros over a list the expression
//! already holds, so every expression ends, in time proportional to its size
//! and its input. Nothing in it reaches files, the network or the clock, so a
//! schema from anyone is safe to run, on a phone as on a server.
//!
//! An expression that fails while running (a missing tag, a wrong type)
//! evaluates to null rather than stopping the compile, as Planetiler's do.
use super::value::{Val, convert, number};
use std::collections::BTreeMap;
use std::fmt;

/// A parsed expression.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Lit(Val),
    Ident(String),
    Member(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    /// A call: `target.name(args)` or `name(args)`.
    Call(Option<Box<Expr>>, String, Vec<Expr>),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    List(Vec<Expr>),
    Map(Vec<(Expr, Expr)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BinOp {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

/// What an expression can read about the feature it runs on.
pub(crate) trait Scope {
    /// A tag of the feature, or `None` when it has none of that key.
    fn tag(&self, key: &str) -> Option<&str>;
    /// The feature's tags, for expressions that need all of them.
    fn tags(&self) -> BTreeMap<String, Val>;
    /// A property of the feature, such as `id` or `source` (`feature.<name>`).
    fn feature(&self, name: &str) -> Option<Val>;
    /// A method of the feature, such as `area("m2")`.
    fn feature_call(&self, name: &str, args: &[Val]) -> Option<Val>;
    /// A variable: `match_key`, `match_value`, `value` or `args`.
    fn variable(&self, name: &str) -> Option<Val>;
}

/// Why a script did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParseError {
    pub message: String,
    pub offset: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at character {}", self.message, self.offset + 1)
    }
}

/// The deepest an expression may nest, which keeps parsing and evaluation
/// off the end of a small (phone) thread stack.
const MAX_DEPTH: usize = 64;

/// Parses a script, the text between `${` and `}`.
pub(crate) fn parse(text: &str) -> Result<Expr, ParseError> {
    let tokens = lex(text)?;
    let mut parser = Parser {
        tokens,
        position: 0,
        depth: 0,
        end: text.len(),
    };
    let expr = parser.expression()?;
    match parser.peek() {
        Token::End => Ok(expr),
        _ => Err(parser.error("unexpected text after the expression")),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Int(i64),
    Double(f64),
    Str(String),
    Ident(String),
    Punct(&'static str),
    End,
}

const PUNCTUATION: [&str; 25] = [
    "&&", "||", "==", "!=", "<=", ">=", "<", ">", "!", "+", "-", "*", "/", "%", "?", ":", ".", ",",
    "(", ")", "[", "]", "{", "}", "=",
];

fn lex(text: &str) -> Result<Vec<(Token, usize)>, ParseError> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    let error = |message: &str, offset| ParseError {
        message: message.into(),
        offset,
    };
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        if c.is_ascii_digit() || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            while i < bytes.len() && bytes[i].is_ascii_digit() {
                i += 1;
            }
            let mut double = false;
            if i < bytes.len()
                && bytes[i] == b'.'
                && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)
            {
                double = true;
                i += 1;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
            }
            if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
                let mut j = i + 1;
                if j < bytes.len() && matches!(bytes[j], b'+' | b'-') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j].is_ascii_digit() {
                    double = true;
                    i = j;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let literal = &text[start..i];
            // CEL's unsigned suffix; the value is the same here.
            if i < bytes.len() && matches!(bytes[i], b'u' | b'U') && !double {
                i += 1;
            }
            let token = if double {
                Token::Double(
                    literal
                        .parse()
                        .map_err(|_| error("invalid number", start))?,
                )
            } else {
                match literal.parse::<i64>() {
                    Ok(value) => Token::Int(value),
                    Err(_) => Token::Double(
                        literal
                            .parse()
                            .map_err(|_| error("invalid number", start))?,
                    ),
                }
            };
            tokens.push((token, start));
            continue;
        }
        if c == b'"' || c == b'\'' {
            let quote = c;
            i += 1;
            let mut value = String::new();
            loop {
                let Some(&b) = bytes.get(i) else {
                    return Err(error("unterminated string", start));
                };
                if b == quote {
                    i += 1;
                    break;
                }
                if b == b'\\' {
                    let escaped = *bytes
                        .get(i + 1)
                        .ok_or_else(|| error("unterminated string", start))?;
                    value.push(match escaped {
                        b'n' => '\n',
                        b't' => '\t',
                        b'r' => '\r',
                        b'\\' => '\\',
                        b'"' => '"',
                        b'\'' => '\'',
                        _ => return Err(error("unknown escape in string", i)),
                    });
                    i += 2;
                    continue;
                }
                // Copy one UTF-8 character.
                let width = text[i..].chars().next().map_or(1, char::len_utf8);
                value.push_str(&text[i..i + width]);
                i += width;
            }
            tokens.push((Token::Str(value), start));
            continue;
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            tokens.push((Token::Ident(text[start..i].into()), start));
            continue;
        }
        let rest = &text[i..];
        match PUNCTUATION.iter().find(|p| rest.starts_with(**p)) {
            Some(p) => {
                i += p.len();
                tokens.push((Token::Punct(p), start));
            }
            None => return Err(error("unexpected character", start)),
        }
    }
    tokens.push((Token::End, text.len()));
    Ok(tokens)
}

struct Parser {
    tokens: Vec<(Token, usize)>,
    position: usize,
    depth: usize,
    end: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.position].0
    }

    fn offset(&self) -> usize {
        self.tokens.get(self.position).map_or(self.end, |t| t.1)
    }

    fn error(&self, message: &str) -> ParseError {
        ParseError {
            message: message.into(),
            offset: self.offset(),
        }
    }

    fn next(&mut self) -> Token {
        let token = self.tokens[self.position].0.clone();
        if self.position + 1 < self.tokens.len() {
            self.position += 1;
        }
        token
    }

    fn eat(&mut self, punct: &str) -> bool {
        if matches!(self.peek(), Token::Punct(p) if *p == punct) {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, punct: &str) -> Result<(), ParseError> {
        if self.eat(punct) {
            Ok(())
        } else {
            Err(self.error(&format!("expected '{punct}'")))
        }
    }

    fn nested<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, ParseError>,
    ) -> Result<T, ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("expression nests too deeply"));
        }
        let result = f(self);
        self.depth -= 1;
        result
    }

    fn expression(&mut self) -> Result<Expr, ParseError> {
        self.nested(|p| {
            let condition = p.or()?;
            if p.eat("?") {
                let then = p.or()?;
                p.expect(":")?;
                let otherwise = p.expression()?;
                Ok(Expr::Cond(
                    Box::new(condition),
                    Box::new(then),
                    Box::new(otherwise),
                ))
            } else {
                Ok(condition)
            }
        })
    }

    fn or(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.and()?;
        while self.eat("||") {
            let right = self.and()?;
            left = Expr::Binary(BinOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.relation()?;
        while self.eat("&&") {
            let right = self.relation()?;
            left = Expr::Binary(BinOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn relation(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.sum()?;
        loop {
            let op = match self.peek() {
                Token::Punct("==") => BinOp::Eq,
                Token::Punct("!=") => BinOp::Ne,
                Token::Punct("<") => BinOp::Lt,
                Token::Punct("<=") => BinOp::Le,
                Token::Punct(">") => BinOp::Gt,
                Token::Punct(">=") => BinOp::Ge,
                Token::Ident(word) if word == "in" => BinOp::In,
                _ => return Ok(left),
            };
            self.next();
            let right = self.sum()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    fn sum(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.product()?;
        loop {
            let op = match self.peek() {
                Token::Punct("+") => BinOp::Add,
                Token::Punct("-") => BinOp::Sub,
                _ => return Ok(left),
            };
            self.next();
            let right = self.product()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    fn product(&mut self) -> Result<Expr, ParseError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Token::Punct("*") => BinOp::Mul,
                Token::Punct("/") => BinOp::Div,
                Token::Punct("%") => BinOp::Rem,
                _ => return Ok(left),
            };
            self.next();
            let right = self.unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        self.nested(|p| {
            if p.eat("!") {
                Ok(Expr::Unary(UnOp::Not, Box::new(p.unary()?)))
            } else if p.eat("-") {
                Ok(match p.unary()? {
                    Expr::Lit(Val::Int(v)) => Expr::Lit(Val::Int(-v)),
                    Expr::Lit(Val::Double(v)) => Expr::Lit(Val::Double(-v)),
                    other => Expr::Unary(UnOp::Neg, Box::new(other)),
                })
            } else {
                p.postfix()
            }
        })
    }

    fn postfix(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.primary()?;
        loop {
            if self.eat(".") {
                let Token::Ident(name) = self.next() else {
                    return Err(self.error("expected a name after '.'"));
                };
                if self.eat("(") {
                    let args = self.arguments(")")?;
                    expr = Expr::Call(Some(Box::new(expr)), name, args);
                } else {
                    expr = Expr::Member(Box::new(expr), name);
                }
            } else if self.eat("[") {
                let index = self.expression()?;
                self.expect("]")?;
                expr = Expr::Index(Box::new(expr), Box::new(index));
            } else {
                return Ok(expr);
            }
        }
    }

    fn arguments(&mut self, close: &str) -> Result<Vec<Expr>, ParseError> {
        let mut args = Vec::new();
        if self.eat(close) {
            return Ok(args);
        }
        loop {
            args.push(self.expression()?);
            if self.eat(close) {
                return Ok(args);
            }
            self.expect(",")?;
            // A trailing comma.
            if self.eat(close) {
                return Ok(args);
            }
        }
    }

    fn primary(&mut self) -> Result<Expr, ParseError> {
        let offset = self.offset();
        match self.next() {
            Token::Int(v) => Ok(Expr::Lit(Val::Int(v))),
            Token::Double(v) => Ok(Expr::Lit(Val::Double(v))),
            Token::Str(v) => Ok(Expr::Lit(Val::Str(v))),
            Token::Ident(name) => match name.as_str() {
                "true" => Ok(Expr::Lit(Val::Bool(true))),
                "false" => Ok(Expr::Lit(Val::Bool(false))),
                "null" => Ok(Expr::Lit(Val::Null)),
                _ if self.eat("(") => {
                    let args = self.arguments(")")?;
                    Ok(Expr::Call(None, name, args))
                }
                _ => Ok(Expr::Ident(name)),
            },
            Token::Punct("(") => {
                let expr = self.expression()?;
                self.expect(")")?;
                Ok(expr)
            }
            Token::Punct("[") => Ok(Expr::List(self.arguments("]")?)),
            Token::Punct("{") => {
                let mut entries = Vec::new();
                if !self.eat("}") {
                    loop {
                        let key = self.expression()?;
                        self.expect(":")?;
                        let value = self.expression()?;
                        entries.push((key, value));
                        if self.eat("}") {
                            break;
                        }
                        self.expect(",")?;
                        if self.eat("}") {
                            break;
                        }
                    }
                }
                Ok(Expr::Map(entries))
            }
            _ => Err(ParseError {
                message: "expected a value".into(),
                offset,
            }),
        }
    }
}

/// A failed evaluation; the expression's value is then null.
#[derive(Debug)]
pub(crate) struct Fail;

type Eval<T> = std::result::Result<T, Fail>;

/// Local variables of the `map`, `filter`, `exists` and `all` macros.
#[derive(Default)]
struct Locals {
    vars: Vec<(String, Val)>,
}

/// Evaluates `expr` on `scope`; a failure gives null.
pub(crate) fn eval(expr: &Expr, scope: &dyn Scope) -> Val {
    let mut locals = Locals::default();
    evaluate(expr, scope, &mut locals).unwrap_or(Val::Null)
}

/// A value of the feature that the scope resolves only when read.
enum Lazy {
    Feature,
    Tags,
}

fn evaluate(expr: &Expr, scope: &dyn Scope, locals: &mut Locals) -> Eval<Val> {
    match expr {
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Ident(name) => {
            if let Some((_, v)) = locals.vars.iter().rev().find(|(n, _)| n == name) {
                return Ok(v.clone());
            }
            if name == "feature" {
                // Only reachable as a whole value; `feature.x` goes through
                // `member`.
                return Err(Fail);
            }
            scope.variable(name).ok_or(Fail)
        }
        Expr::Member(target, name) => match lazy(target) {
            Some(Lazy::Feature) => scope.feature(name).ok_or(Fail),
            Some(Lazy::Tags) => Ok(tag_value(scope, name)),
            None => {
                let target = evaluate(target, scope, locals)?;
                member(&target, name)
            }
        },
        Expr::Index(target, index) => {
            let index = evaluate(index, scope, locals)?;
            if let Some(Lazy::Tags) = lazy(target) {
                return match index {
                    Val::Str(key) => Ok(tag_value(scope, &key)),
                    _ => Err(Fail),
                };
            }
            let target = evaluate(target, scope, locals)?;
            match (target, index) {
                (Val::Map(map), Val::Str(key)) => map.get(&key).cloned().ok_or(Fail),
                (Val::List(list), Val::Int(i)) => usize::try_from(i)
                    .ok()
                    .and_then(|i| list.get(i).cloned())
                    .ok_or(Fail),
                _ => Err(Fail),
            }
        }
        Expr::Call(target, name, args) => call(target.as_deref(), name, args, scope, locals),
        Expr::Unary(UnOp::Not, inner) => match evaluate(inner, scope, locals)? {
            Val::Bool(b) => Ok(Val::Bool(!b)),
            _ => Err(Fail),
        },
        Expr::Unary(UnOp::Neg, inner) => match evaluate(inner, scope, locals)? {
            Val::Int(v) => v.checked_neg().map(Val::Int).ok_or(Fail),
            Val::Double(v) => Ok(Val::Double(-v)),
            _ => Err(Fail),
        },
        Expr::Binary(BinOp::And, a, b) => {
            // CEL's commutative logic: an error on one side is absorbed when
            // the other side decides the result.
            let left = evaluate(a, scope, locals);
            if let Ok(Val::Bool(false)) = left {
                return Ok(Val::Bool(false));
            }
            let right = evaluate(b, scope, locals);
            match (left, right) {
                (_, Ok(Val::Bool(false))) => Ok(Val::Bool(false)),
                (Ok(Val::Bool(true)), Ok(Val::Bool(true))) => Ok(Val::Bool(true)),
                _ => Err(Fail),
            }
        }
        Expr::Binary(BinOp::Or, a, b) => {
            let left = evaluate(a, scope, locals);
            if let Ok(Val::Bool(true)) = left {
                return Ok(Val::Bool(true));
            }
            let right = evaluate(b, scope, locals);
            match (left, right) {
                (_, Ok(Val::Bool(true))) => Ok(Val::Bool(true)),
                (Ok(Val::Bool(false)), Ok(Val::Bool(false))) => Ok(Val::Bool(false)),
                _ => Err(Fail),
            }
        }
        Expr::Binary(op, a, b) => {
            let left = evaluate(a, scope, locals)?;
            let right = evaluate(b, scope, locals)?;
            binary(*op, left, right)
        }
        Expr::Cond(condition, then, otherwise) => match evaluate(condition, scope, locals)? {
            Val::Bool(true) => evaluate(then, scope, locals),
            Val::Bool(false) => evaluate(otherwise, scope, locals),
            _ => Err(Fail),
        },
        Expr::List(items) => items
            .iter()
            .map(|item| evaluate(item, scope, locals))
            .collect::<Eval<Vec<_>>>()
            .map(Val::List),
        Expr::Map(entries) => {
            let mut map = BTreeMap::new();
            for (key, value) in entries {
                let Val::Str(key) = evaluate(key, scope, locals)? else {
                    return Err(Fail);
                };
                map.insert(key, evaluate(value, scope, locals)?);
            }
            Ok(Val::Map(map))
        }
    }
}

/// `feature` and `feature.tags`, which are read through the scope.
fn lazy(expr: &Expr) -> Option<Lazy> {
    match expr {
        Expr::Ident(name) if name == "feature" => Some(Lazy::Feature),
        Expr::Member(target, name)
            if name == "tags" && matches!(&**target, Expr::Ident(n) if n == "feature") =>
        {
            Some(Lazy::Tags)
        }
        _ => None,
    }
}

/// `feature.tags.<key>`: the tag, or null. CEL would fail on a missing key,
/// which Planetiler turns into null anyway.
fn tag_value(scope: &dyn Scope, key: &str) -> Val {
    scope
        .tag(key)
        .map_or(Val::Null, |value| Val::Str(value.into()))
}

fn member(target: &Val, name: &str) -> Eval<Val> {
    match target {
        Val::Map(map) => map.get(name).cloned().ok_or(Fail),
        _ => Err(Fail),
    }
}

fn call(
    target: Option<&Expr>,
    name: &str,
    args: &[Expr],
    scope: &dyn Scope,
    locals: &mut Locals,
) -> Eval<Val> {
    // Macros bind a variable rather than evaluating their first argument.
    if let Some(target) = target
        && matches!(name, "map" | "filter" | "exists" | "all" | "exists_one")
        && (args.len() == 2 || (name == "map" && args.len() == 3))
    {
        let Expr::Ident(var) = &args[0] else {
            return Err(Fail);
        };
        let items = match evaluate_target(target, scope, locals)? {
            Val::List(items) => items,
            Val::Map(map) => map.into_keys().map(Val::Str).collect(),
            _ => return Err(Fail),
        };
        let mut out = Vec::new();
        let mut count = 0;
        for item in items {
            locals.vars.push((var.clone(), item.clone()));
            let result = (|| -> Eval<Option<Val>> {
                if name == "map" && args.len() == 3 {
                    if truthy(evaluate(&args[1], scope, locals)?)? {
                        return Ok(Some(evaluate(&args[2], scope, locals)?));
                    }
                    return Ok(None);
                }
                let value = evaluate(&args[1], scope, locals)?;
                Ok(Some(value))
            })();
            locals.vars.pop();
            let Some(value) = result? else { continue };
            match name {
                "map" => out.push(value),
                "filter" => {
                    if truthy(value)? {
                        out.push(item);
                    }
                }
                "exists" => {
                    if truthy(value)? {
                        return Ok(Val::Bool(true));
                    }
                }
                "all" => {
                    if !truthy(value)? {
                        return Ok(Val::Bool(false));
                    }
                }
                _ => count += usize::from(truthy(value)?),
            }
        }
        return Ok(match name {
            "map" | "filter" => Val::List(out),
            "exists" => Val::Bool(false),
            "all" => Val::Bool(true),
            _ => Val::Bool(count == 1),
        });
    }
    // `has(feature.tags.x)` tests for presence without failing.
    if target.is_none() && name == "has" && args.len() == 1 {
        return Ok(Val::Bool(match &args[0] {
            Expr::Member(inner, key) => match lazy(inner) {
                Some(Lazy::Tags) => scope.tag(key).is_some(),
                Some(Lazy::Feature) => scope.feature(key).is_some_and(|v| v != Val::Null),
                None => {
                    matches!(evaluate(inner, scope, locals)?, Val::Map(m) if m.contains_key(key))
                }
            },
            _ => return Err(Fail),
        }));
    }
    if let Some(target) = target {
        // Methods of the feature and of its tags read the scope directly.
        match lazy(target) {
            Some(Lazy::Feature) => {
                let args = evaluate_all(args, scope, locals)?;
                return scope.feature_call(name, &args).ok_or(Fail);
            }
            Some(Lazy::Tags) => {
                let args = evaluate_all(args, scope, locals)?;
                return tags_method(scope, name, &args);
            }
            None => {}
        }
        let receiver = evaluate(target, scope, locals)?;
        let args = evaluate_all(args, scope, locals)?;
        return method(receiver, name, &args);
    }
    let args = evaluate_all(args, scope, locals)?;
    function(name, args)
}

fn evaluate_target(target: &Expr, scope: &dyn Scope, locals: &mut Locals) -> Eval<Val> {
    match lazy(target) {
        Some(Lazy::Tags) => Ok(Val::Map(scope.tags())),
        Some(Lazy::Feature) => Err(Fail),
        None => evaluate(target, scope, locals),
    }
}

fn evaluate_all(args: &[Expr], scope: &dyn Scope, locals: &mut Locals) -> Eval<Vec<Val>> {
    args.iter()
        .map(|arg| match lazy(arg) {
            Some(Lazy::Tags) => Ok(Val::Map(scope.tags())),
            _ => evaluate(arg, scope, locals),
        })
        .collect()
}

fn truthy(value: Val) -> Eval<bool> {
    match value {
        Val::Bool(b) => Ok(b),
        _ => Err(Fail),
    }
}

/// The methods of `feature.tags`, Planetiler's map extensions.
fn tags_method(scope: &dyn Scope, name: &str, args: &[Val]) -> Eval<Val> {
    let key = |i: usize| match args.get(i) {
        Some(Val::Str(key)) => Ok(key.as_str()),
        _ => Err(Fail),
    };
    match (name, args.len()) {
        ("get", 1) => Ok(tag_value(scope, key(0)?)),
        ("get", 2) => Ok(scope
            .tag(key(0)?)
            .map_or_else(|| args[1].clone(), |v| Val::Str(v.into()))),
        ("has", 1) => Ok(Val::Bool(scope.tag(key(0)?).is_some())),
        ("has", _) => {
            let Some(value) = scope.tag(key(0)?) else {
                return Ok(Val::Bool(false));
            };
            Ok(Val::Bool(
                args[1..]
                    .iter()
                    .any(|wanted| matches!(wanted, Val::Str(w) if w == value)),
            ))
        }
        ("size", 0) => Ok(Val::Int(scope.tags().len() as i64)),
        _ => method(Val::Map(scope.tags()), name, args),
    }
}

fn method(receiver: Val, name: &str, args: &[Val]) -> Eval<Val> {
    let string = |i: usize| match args.get(i) {
        Some(Val::Str(s)) => Ok(s.as_str()),
        _ => Err(Fail),
    };
    match (receiver, name, args.len()) {
        (Val::Str(s), "replace", 2) => Ok(Val::Str(s.replace(string(0)?, string(1)?))),
        (Val::Str(s), "replace", 3) => {
            let Val::Int(n) = args[2] else {
                return Err(Fail);
            };
            let (from, to) = (string(0)?, string(1)?);
            Ok(Val::Str(if n < 0 {
                s.replace(from, to)
            } else {
                s.replacen(from, to, n as usize)
            }))
        }
        (Val::Str(s), "split", 1) => {
            let separator = string(0)?;
            if separator.is_empty() {
                return Ok(Val::List(
                    s.chars().map(|c| Val::Str(c.to_string())).collect(),
                ));
            }
            Ok(Val::List(
                s.split(separator)
                    .map(|part| Val::Str(part.into()))
                    .collect(),
            ))
        }
        (Val::Str(s), "contains", 1) => Ok(Val::Bool(s.contains(string(0)?))),
        (Val::Str(s), "startsWith", 1) => Ok(Val::Bool(s.starts_with(string(0)?))),
        (Val::Str(s), "endsWith", 1) => Ok(Val::Bool(s.ends_with(string(0)?))),
        (Val::Str(s), "lowerAscii", 0) => Ok(Val::Str(s.to_ascii_lowercase())),
        (Val::Str(s), "upperAscii", 0) => Ok(Val::Str(s.to_ascii_uppercase())),
        (Val::Str(s), "trim", 0) => Ok(Val::Str(s.trim().into())),
        (Val::Str(s), "size", 0) => Ok(Val::Int(s.chars().count() as i64)),
        (Val::Str(s), "indexOf", 1) => Ok(Val::Int(
            s.find(string(0)?)
                .map_or(-1, |byte| s[..byte].chars().count() as i64),
        )),
        (Val::Str(s), "substring", 1 | 2) => {
            let chars = s.chars().collect::<Vec<_>>();
            let index = |i: usize| match args.get(i) {
                Some(Val::Int(v)) => usize::try_from(*v).map_err(|_| Fail),
                None => Ok(chars.len()),
                _ => Err(Fail),
            };
            let (start, end) = (index(0)?, index(1)?);
            if start > end || end > chars.len() {
                return Err(Fail);
            }
            Ok(Val::Str(chars[start..end].iter().collect()))
        }
        (Val::List(items), "size", 0) => Ok(Val::Int(items.len() as i64)),
        (Val::List(items), "join", 0 | 1) => {
            let separator = if args.is_empty() { "" } else { string(0)? };
            let parts = items
                .iter()
                .map(|item| match item {
                    Val::Str(s) => Ok(s.as_str()),
                    _ => Err(Fail),
                })
                .collect::<Eval<Vec<_>>>()?;
            Ok(Val::Str(parts.join(separator)))
        }
        (Val::Map(map), "size", 0) => Ok(Val::Int(map.len() as i64)),
        (Val::Map(map), "get", 1) => Ok(map.get(string(0)?).cloned().unwrap_or(Val::Null)),
        (Val::Map(map), "get", 2) => Ok(map
            .get(string(0)?)
            .cloned()
            .unwrap_or_else(|| args[1].clone())),
        (Val::Map(map), "has", n) if n >= 1 => {
            let Some(value) = map.get(string(0)?) else {
                return Ok(Val::Bool(false));
            };
            Ok(Val::Bool(n == 1 || args[1..].contains(value)))
        }
        _ => Err(Fail),
    }
}

fn function(name: &str, mut args: Vec<Val>) -> Eval<Val> {
    match (name, args.len()) {
        ("size", 1) => method(args.remove(0), "size", &[]),
        ("coalesce", _) => Ok(args
            .into_iter()
            .find(|v| *v != Val::Null)
            .unwrap_or(Val::Null)),
        ("nullif", 2) => {
            if args[0] == args[1] {
                Ok(Val::Null)
            } else {
                Ok(args.remove(0))
            }
        }
        ("min" | "max", 1) => {
            let Val::List(items) = args.remove(0) else {
                return Err(Fail);
            };
            let mut best: Option<Val> = None;
            for item in items {
                let value = number(&item).ok_or(Fail)?;
                let better = match &best {
                    None => true,
                    Some(b) => {
                        let b = number(b).ok_or(Fail)?;
                        if name == "min" { value < b } else { value > b }
                    }
                };
                if better {
                    best = Some(item);
                }
            }
            best.ok_or(Fail)
        }
        ("int", 1) => convert(&args[0], super::value::DataType::Long).ok_or(Fail),
        ("double", 1) => convert(&args[0], super::value::DataType::Double).ok_or(Fail),
        ("string", 1) => convert(&args[0], super::value::DataType::String).ok_or(Fail),
        ("bool", 1) => match &args[0] {
            Val::Bool(b) => Ok(Val::Bool(*b)),
            Val::Str(s) if s == "true" => Ok(Val::Bool(true)),
            Val::Str(s) if s == "false" => Ok(Val::Bool(false)),
            _ => Err(Fail),
        },
        _ => Err(Fail),
    }
}

fn binary(op: BinOp, left: Val, right: Val) -> Eval<Val> {
    use BinOp::*;
    match op {
        Eq => Ok(Val::Bool(equal(&left, &right))),
        Ne => Ok(Val::Bool(!equal(&left, &right))),
        Lt | Le | Gt | Ge => {
            let ordering = match (&left, &right) {
                (Val::Str(a), Val::Str(b)) => a.cmp(b),
                (Val::Bool(a), Val::Bool(b)) => a.cmp(b),
                (Val::Int(a), Val::Int(b)) => a.cmp(b),
                _ => {
                    let (a, b) = (number(&left).ok_or(Fail)?, number(&right).ok_or(Fail)?);
                    a.partial_cmp(&b).ok_or(Fail)?
                }
            };
            Ok(Val::Bool(match op {
                Lt => ordering.is_lt(),
                Le => ordering.is_le(),
                Gt => ordering.is_gt(),
                _ => ordering.is_ge(),
            }))
        }
        In => match right {
            Val::List(items) => Ok(Val::Bool(items.iter().any(|item| equal(item, &left)))),
            Val::Map(map) => match left {
                Val::Str(key) => Ok(Val::Bool(map.contains_key(&key))),
                _ => Err(Fail),
            },
            _ => Err(Fail),
        },
        Add => match (left, right) {
            (Val::Str(a), Val::Str(b)) => Ok(Val::Str(a + b.as_str())),
            (Val::List(mut a), Val::List(b)) => {
                a.extend(b);
                Ok(Val::List(a))
            }
            (Val::Int(a), Val::Int(b)) => a.checked_add(b).map(Val::Int).ok_or(Fail),
            (a, b) => arithmetic(&a, &b, |x, y| x + y),
        },
        Sub => match (left, right) {
            (Val::Int(a), Val::Int(b)) => a.checked_sub(b).map(Val::Int).ok_or(Fail),
            (a, b) => arithmetic(&a, &b, |x, y| x - y),
        },
        Mul => match (left, right) {
            (Val::Int(a), Val::Int(b)) => a.checked_mul(b).map(Val::Int).ok_or(Fail),
            (a, b) => arithmetic(&a, &b, |x, y| x * y),
        },
        Div => match (left, right) {
            (Val::Int(_), Val::Int(0)) => Err(Fail),
            (Val::Int(a), Val::Int(b)) => a.checked_div(b).map(Val::Int).ok_or(Fail),
            (a, b) => arithmetic(&a, &b, |x, y| x / y),
        },
        Rem => match (left, right) {
            (Val::Int(_), Val::Int(0)) => Err(Fail),
            (Val::Int(a), Val::Int(b)) => a.checked_rem(b).map(Val::Int).ok_or(Fail),
            _ => Err(Fail),
        },
        And | Or => unreachable!("evaluated lazily"),
    }
}

fn arithmetic(a: &Val, b: &Val, f: impl Fn(f64, f64) -> f64) -> Eval<Val> {
    match (a, b) {
        (Val::Double(_), Val::Double(_) | Val::Int(_)) | (Val::Int(_), Val::Double(_)) => Ok(
            Val::Double(f(number(a).ok_or(Fail)?, number(b).ok_or(Fail)?)),
        ),
        _ => Err(Fail),
    }
}

/// CEL equality, with numbers equal across int and double.
fn equal(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Int(_) | Val::Double(_), Val::Int(_) | Val::Double(_)) => number(a) == number(b),
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tags(BTreeMap<String, String>);

    impl Scope for Tags {
        fn tag(&self, key: &str) -> Option<&str> {
            self.0.get(key).map(String::as_str)
        }
        fn tags(&self) -> BTreeMap<String, Val> {
            self.0
                .iter()
                .map(|(k, v)| (k.clone(), Val::Str(v.clone())))
                .collect()
        }
        fn feature(&self, name: &str) -> Option<Val> {
            (name == "id").then_some(Val::Int(7))
        }
        fn feature_call(&self, name: &str, args: &[Val]) -> Option<Val> {
            (name == "area" && args == [Val::Str("km2".into())]).then_some(Val::Double(2.5e6))
        }
        fn variable(&self, name: &str) -> Option<Val> {
            (name == "match_value").then(|| Val::Str("motorway_link".into()))
        }
    }

    fn run(text: &str, tags: &[(&str, &str)]) -> Val {
        let scope = Tags(
            tags.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        eval(&parse(text).unwrap(), &scope)
    }

    #[test]
    fn evaluates_the_scripts_of_planetilers_shortbread_schema() {
        let s = |v: &str| Val::Str(v.into());
        assert_eq!(
            run(r#"match_value.replace("_link", "")"#, &[]),
            s("motorway")
        );
        assert_eq!(
            run(r#"match_value == "wood" ? "forest": match_value"#, &[]),
            s("motorway_link")
        );
        let refs = [("ref", "A 4;E 40;B 55")];
        assert_eq!(
            run(r#"feature.tags["ref"].replace(";", "\n")"#, &refs),
            s("A 4\nE 40\nB 55")
        );
        assert_eq!(
            run(
                r#"max(feature.tags["ref"].split(";").map(r, size(r)))"#,
                &refs
            ),
            Val::Int(4)
        );
        assert_eq!(
            run(r#"size(feature.tags["ref"].split(";"))"#, &refs),
            Val::Int(3)
        );
        assert_eq!(
            run(r#"feature.tags.get("population")"#, &[("population", "12")]),
            s("12")
        );
        assert_eq!(run(r#"feature.tags.get("population")"#, &[]), Val::Null);
        assert_eq!(
            run(
                r#"feature.tags["admin_level"] == "2" && feature.area("km2") >= 2e6"#,
                &[("admin_level", "2")]
            ),
            Val::Bool(true)
        );
        assert_eq!(run(r#"feature.area("km2") >= 7e5"#, &[]), Val::Bool(true));
    }

    #[test]
    fn failures_and_missing_tags_are_null() {
        assert_eq!(run("feature.tags.name.size()", &[]), Val::Null);
        assert_eq!(run("1 / 0", &[]), Val::Null);
        assert_eq!(run(r#""a" + 1"#, &[]), Val::Null);
        assert_eq!(run("unknown_variable", &[]), Val::Null);
        assert_eq!(
            run("has(feature.tags.name)", &[("name", "x")]),
            Val::Bool(true)
        );
        assert_eq!(run("has(feature.tags.name)", &[]), Val::Bool(false));
        // An error absorbed by a deciding side, as in CEL.
        assert_eq!(
            run("feature.tags.x.size() > 1 || true", &[]),
            Val::Bool(true)
        );
    }

    #[test]
    fn arithmetic_and_lists() {
        assert_eq!(run("8 * 24 - 2", &[]), Val::Int(190));
        assert_eq!(run("1 + 0.5", &[]), Val::Double(1.5));
        assert_eq!(run("-3 % 2", &[]), Val::Int(-1));
        assert_eq!(run("2 in [1, 2, 3]", &[]), Val::Bool(true));
        assert_eq!(run("[1, 2, 3].filter(x, x > 1).size()", &[]), Val::Int(2));
        assert_eq!(run("[1, 2].exists(x, x == 2)", &[]), Val::Bool(true));
        assert_eq!(run(r#"{"a": 1}["a"]"#, &[]), Val::Int(1));
        assert_eq!(
            run("coalesce(null, feature.tags.a, 'x')", &[]),
            Val::Str("x".into())
        );
        assert_eq!(run("feature.id", &[]), Val::Int(7));
        assert_eq!(run(r#"int("12")"#, &[]), Val::Int(12));
        assert_eq!(run(r#"double("1.5")"#, &[]), Val::Double(1.5));
    }

    #[test]
    fn rejects_malformed_and_runaway_scripts() {
        for text in ["", "1 +", "(1", "'unterminated", "a ? b", "1 $ 2", "f(,)"] {
            assert!(parse(text).is_err(), "{text}");
        }
        let deep = format!("{}1{}", "(".repeat(200), ")".repeat(200));
        assert!(parse(&deep).unwrap_err().message.contains("nests"));
        let deep = format!("{}true", "!".repeat(200));
        assert!(parse(&deep).is_err());
    }
}
