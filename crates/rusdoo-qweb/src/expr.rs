//! A small expression evaluator for QWeb directives (`t-if`, `t-esc`,
//! `t-foreach`, ...). QWeb expressions are Python; this supports the
//! common subset: context paths (`book.name`), literals, comparisons,
//! `and`/`or`/`not`, and `+`/`-`. Anything else is an error.

use rusdoo_core::RusdooError;
use serde_json::Value;

const MAX_DEPTH: usize = 64;

/// Python-ish truthiness: null/false/0/""/[]/{} are falsy.
pub fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub fn eval(src: &str, ctx: &Value) -> Result<Value, RusdooError> {
    let mut parser = Parser {
        chars: src.chars().collect(),
        pos: 0,
        ctx,
    };
    let value = parser.parse_or(0)?;
    parser.skip_ws();
    if parser.pos < parser.chars.len() {
        return Err(parser.err("unexpected trailing content"));
    }
    Ok(value)
}

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    ctx: &'a Value,
}

impl Parser<'_> {
    fn err(&self, message: &str) -> RusdooError {
        RusdooError::Validation(format!("qweb expr: {message} at offset {}", self.pos))
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    /// Consume `word` if it appears next as a standalone keyword.
    fn eat_keyword(&mut self, word: &str) -> bool {
        self.skip_ws();
        let end = self.pos + word.len();
        let matches_word = self.chars.get(self.pos..end).is_some_and(|slice| {
            slice.iter().collect::<String>() == word
                && self
                    .chars
                    .get(end)
                    .map(|c| !c.is_alphanumeric() && *c != '_')
                    .unwrap_or(true)
        });
        if matches_word {
            self.pos = end;
        }
        matches_word
    }

    fn parse_or(&mut self, depth: usize) -> Result<Value, RusdooError> {
        if depth > MAX_DEPTH {
            return Err(self.err("expression nested too deep"));
        }
        let mut left = self.parse_and(depth)?;
        while self.eat_keyword("or") {
            let right = self.parse_and(depth)?;
            // python: a or b -> a if truthy else b
            left = if truthy(&left) { left } else { right };
        }
        Ok(left)
    }

    fn parse_and(&mut self, depth: usize) -> Result<Value, RusdooError> {
        let mut left = self.parse_not(depth)?;
        while self.eat_keyword("and") {
            let right = self.parse_not(depth)?;
            left = if truthy(&left) { right } else { left };
        }
        Ok(left)
    }

    fn parse_not(&mut self, depth: usize) -> Result<Value, RusdooError> {
        if self.eat_keyword("not") {
            let v = self.parse_not(depth)?;
            return Ok(Value::Bool(!truthy(&v)));
        }
        self.parse_comparison(depth)
    }

    fn parse_comparison(&mut self, depth: usize) -> Result<Value, RusdooError> {
        let left = self.parse_add(depth)?;
        self.skip_ws();
        if let Some(op) = self.peek_comparison() {
            self.pos += op.len();
            let right = self.parse_add(depth)?;
            return Ok(Value::Bool(compare(&left, &right, op)));
        }
        Ok(left)
    }

    fn peek_comparison(&self) -> Option<&'static str> {
        if let Some(two) = self.chars.get(self.pos..self.pos + 2) {
            match two.iter().collect::<String>().as_str() {
                "==" => return Some("=="),
                "!=" => return Some("!="),
                "<=" => return Some("<="),
                ">=" => return Some(">="),
                _ => {}
            }
        }
        match self.peek()? {
            '<' => Some("<"),
            '>' => Some(">"),
            _ => None,
        }
    }

    fn parse_add(&mut self, depth: usize) -> Result<Value, RusdooError> {
        let mut left = self.parse_primary(depth)?;
        loop {
            self.skip_ws();
            let op = match self.peek() {
                Some('+') => '+',
                Some('-') => '-',
                _ => break,
            };
            self.pos += 1;
            let right = self.parse_primary(depth)?;
            left = arithmetic(&left, &right, op)?;
        }
        Ok(left)
    }

    fn parse_primary(&mut self, depth: usize) -> Result<Value, RusdooError> {
        self.skip_ws();
        match self.peek() {
            Some('(') => {
                self.pos += 1;
                let v = self.parse_or(depth + 1)?;
                self.skip_ws();
                if self.peek() != Some(')') {
                    return Err(self.err("expected ')'"));
                }
                self.pos += 1;
                Ok(v)
            }
            Some('\'') | Some('"') => Ok(Value::String(self.parse_string()?)),
            Some(c) if c.is_ascii_digit() || c == '-' || c == '+' => self.parse_number(),
            Some(c) if c.is_alphabetic() || c == '_' => self.parse_path_or_literal(),
            _ => Err(self.err("expected a value")),
        }
    }

    fn parse_string(&mut self) -> Result<String, RusdooError> {
        let quote = self.peek().expect("checked");
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some(c) if c == quote => {
                    self.pos += 1;
                    break;
                }
                Some('\\') => {
                    self.pos += 1;
                    if let Some(c) = self.peek() {
                        out.push(match c {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
                        self.pos += 1;
                    }
                }
                Some(c) => {
                    out.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(out)
    }

    fn parse_number(&mut self) -> Result<Value, RusdooError> {
        let start = self.pos;
        if matches!(self.peek(), Some('+' | '-')) {
            self.pos += 1;
        }
        let mut is_float = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.pos += 1;
            } else if c == '.' {
                is_float = true;
                self.pos += 1;
            } else {
                break;
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        if is_float {
            text.parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| self.err("invalid float"))
        } else {
            text.parse::<i64>()
                .map(Value::from)
                .map_err(|_| self.err("invalid integer"))
        }
    }

    fn parse_path_or_literal(&mut self) -> Result<Value, RusdooError> {
        let mut segments = vec![self.read_ident()];
        while self.peek() == Some('.') {
            self.pos += 1;
            segments.push(self.read_ident());
        }
        if segments.len() == 1 {
            match segments[0].as_str() {
                "True" => return Ok(Value::Bool(true)),
                "False" => return Ok(Value::Bool(false)),
                "None" => return Ok(Value::Null),
                _ => {}
            }
        }
        // resolve the dotted path against the context (missing -> null)
        let mut current = self.ctx;
        for segment in &segments {
            current = match current.get(segment) {
                Some(v) => v,
                None => return Ok(Value::Null),
            };
        }
        Ok(current.clone())
    }

    fn read_ident(&mut self) -> String {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }
}

fn arithmetic(left: &Value, right: &Value, op: char) -> Result<Value, RusdooError> {
    if op == '+' {
        if let (Value::String(a), Value::String(b)) = (left, right) {
            return Ok(Value::String(format!("{a}{b}")));
        }
    }
    match (left.as_f64(), right.as_f64()) {
        (Some(a), Some(b)) => {
            let result = if op == '+' { a + b } else { a - b };
            if result.fract() == 0.0 {
                Ok(Value::from(result as i64))
            } else {
                Ok(serde_json::Number::from_f64(result)
                    .map(Value::Number)
                    .unwrap_or(Value::Null))
            }
        }
        _ => Err(RusdooError::Validation(format!(
            "qweb expr: cannot apply '{op}' to {left} and {right}"
        ))),
    }
}

fn compare(left: &Value, right: &Value, op: &str) -> bool {
    use std::cmp::Ordering;
    let ordering = match (left.as_f64(), right.as_f64()) {
        (Some(a), Some(b)) => a.partial_cmp(&b),
        _ => match (left.as_str(), right.as_str()) {
            (Some(a), Some(b)) => Some(a.cmp(b)),
            _ => None,
        },
    };
    match op {
        "==" => left == right,
        "!=" => left != right,
        "<" => ordering == Some(Ordering::Less),
        ">" => ordering == Some(Ordering::Greater),
        "<=" => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
        ">=" => matches!(ordering, Some(Ordering::Greater | Ordering::Equal)),
        _ => false,
    }
}
