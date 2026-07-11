//! Restricted evaluator for the `eval="..."` attribute in addon data,
//! port of the eval context in `odoo/tools/convert.py`.
//!
//! Supports python literals plus the two callables Odoo exposes in that
//! context: `ref('module.xml_id')` (resolved to a database id) and
//! `Command.<method>(...)` (the x2many write commands). Everything else
//! is rejected — this is a data-file evaluator, not a Python interpreter.

use rusdoo_core::RusdooError;
use serde_json::{Map, Number, Value};

/// Resolve an external id to a database id (Odoo's `ref()`).
pub trait RefResolver {
    fn resolve(&self, xml_id: &str) -> Option<i64>;
}

impl<F: Fn(&str) -> Option<i64>> RefResolver for F {
    fn resolve(&self, xml_id: &str) -> Option<i64> {
        self(xml_id)
    }
}

/// Bound on nesting to keep the recursive parser from overflowing the
/// stack on hostile input.
const MAX_DEPTH: usize = 100;

/// Evaluate an `eval="..."` expression: python literals plus `ref('id')`
/// (resolved via `refs`) and `Command.<method>(...)` x2many tuples.
pub fn eval_expr(src: &str, refs: &dyn RefResolver) -> Result<Value, RusdooError> {
    let mut parser = Parser {
        chars: src.chars().collect(),
        pos: 0,
        refs,
    };
    let value = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos < parser.chars.len() {
        return Err(parser.err("unexpected trailing content"));
    }
    Ok(value)
}

struct Parser<'a> {
    chars: Vec<char>,
    pos: usize,
    refs: &'a dyn RefResolver,
}

impl Parser<'_> {
    fn err(&self, message: &str) -> RusdooError {
        RusdooError::Validation(format!("eval: {message} at offset {}", self.pos))
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.pos += 1;
        }
    }

    fn eat(&mut self, expected: char) -> Result<(), RusdooError> {
        self.skip_ws();
        if self.peek() == Some(expected) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err(&format!("expected {expected:?}")))
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Value, RusdooError> {
        if depth > MAX_DEPTH {
            return Err(self.err("expression nested too deep"));
        }
        self.skip_ws();
        match self.peek() {
            Some('[') | Some('(') => self.parse_seq(depth),
            Some('{') => self.parse_dict(depth),
            Some('\'') | Some('"') => Ok(Value::String(self.parse_string()?)),
            Some(c) if c.is_ascii_digit() || c == '-' || c == '+' => self.parse_number(),
            Some(c) if c.is_alphabetic() || c == '_' => self.parse_ident_or_call(depth),
            _ => Err(self.err("expected a value")),
        }
    }

    fn parse_seq(&mut self, depth: usize) -> Result<Value, RusdooError> {
        let close = if self.peek() == Some('[') { ']' } else { ')' };
        self.pos += 1;
        let items = self.parse_items(close, depth)?;
        Ok(Value::Array(items))
    }

    /// Comma-separated values up to `close` (trailing comma allowed).
    fn parse_items(&mut self, close: char, depth: usize) -> Result<Vec<Value>, RusdooError> {
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(close) {
                self.pos += 1;
                break;
            }
            items.push(self.parse_value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(',') => self.pos += 1,
                Some(c) if c == close => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or a closing bracket")),
            }
        }
        Ok(items)
    }

    fn parse_dict(&mut self, depth: usize) -> Result<Value, RusdooError> {
        self.pos += 1; // '{'
        let mut map = Map::new();
        loop {
            self.skip_ws();
            if self.peek() == Some('}') {
                self.pos += 1;
                break;
            }
            let key = self.parse_string()?;
            self.eat(':')?;
            let value = self.parse_value(depth + 1)?;
            map.insert(key, value);
            self.skip_ws();
            match self.peek() {
                Some(',') => self.pos += 1,
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.err("expected ',' or '}'")),
            }
        }
        Ok(Value::Object(map))
    }

    fn parse_string(&mut self) -> Result<String, RusdooError> {
        self.skip_ws();
        let quote = match self.peek() {
            Some(q @ ('\'' | '"')) => q,
            _ => return Err(self.err("expected a string")),
        };
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(self.err("unterminated string")),
                Some('\\') => {
                    self.pos += 1;
                    let escaped = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.pos += 1;
                    out.push(match escaped {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        other => other,
                    });
                }
                Some(c) if c == quote => {
                    self.pos += 1;
                    break;
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
            } else if c == '.' || c == 'e' || c == 'E' {
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
                .and_then(Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| self.err("invalid float"))
        } else {
            text.parse::<i64>()
                .map(Value::from)
                .map_err(|_| self.err("invalid integer"))
        }
    }

    fn parse_identifier(&mut self) -> String {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }

    fn parse_ident_or_call(&mut self, depth: usize) -> Result<Value, RusdooError> {
        let ident = self.parse_identifier();
        self.skip_ws();
        match self.peek() {
            // `ref('x')`
            Some('(') if ident == "ref" => {
                let args = self.parse_call_args(depth)?;
                let [Value::String(xml_id)] = args.as_slice() else {
                    return Err(self.err("ref() takes a single string external id"));
                };
                let id = self.refs.resolve(xml_id).ok_or_else(|| {
                    RusdooError::Validation(format!("unknown external id: {xml_id}"))
                })?;
                Ok(Value::from(id))
            }
            // `Command.method(...)`
            Some('.') if ident == "Command" => {
                self.pos += 1; // '.'
                let method = self.parse_identifier();
                self.skip_ws();
                let args = self.parse_call_args(depth)?;
                command(&method, args).map_err(|m| self.err(&m))
            }
            _ => match ident.as_str() {
                "True" => Ok(Value::Bool(true)),
                "False" => Ok(Value::Bool(false)),
                "None" => Ok(Value::Null),
                other => Err(self.err(&format!("unsupported identifier {other:?}"))),
            },
        }
    }

    fn parse_call_args(&mut self, depth: usize) -> Result<Vec<Value>, RusdooError> {
        self.eat('(')?;
        self.parse_items(')', depth)
    }
}

/// Translate a `Command.<method>(args)` call into Odoo's `(code, id, values)`
/// tuple (`odoo/orm/commands.py`).
fn command(method: &str, args: Vec<Value>) -> Result<Value, String> {
    // arity is checked so a typo like `Command.link()` errors instead of
    // silently producing a link to record 0
    let arity = |n: usize| -> Result<(), String> {
        if args.len() == n {
            Ok(())
        } else {
            Err(format!(
                "Command.{method} takes {n} argument(s), got {}",
                args.len()
            ))
        }
    };
    let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::from(0));
    Ok(match method {
        // create(values) -> (0, 0, values)
        "create" => {
            arity(1)?;
            triple(0, Value::from(0), arg(0))
        }
        // update(id, values) -> (1, id, values)
        "update" => {
            arity(2)?;
            triple(1, arg(0), arg(1))
        }
        // delete(id) -> (2, id, 0)
        "delete" => {
            arity(1)?;
            triple(2, arg(0), Value::from(0))
        }
        // unlink(id) -> (3, id, 0)
        "unlink" => {
            arity(1)?;
            triple(3, arg(0), Value::from(0))
        }
        // link(id) -> (4, id, 0)
        "link" => {
            arity(1)?;
            triple(4, arg(0), Value::from(0))
        }
        // clear() -> (5, 0, 0)
        "clear" => {
            arity(0)?;
            triple(5, Value::from(0), Value::from(0))
        }
        // set(ids) -> (6, 0, ids)
        "set" => {
            arity(1)?;
            triple(6, Value::from(0), arg(0))
        }
        other => return Err(format!("unsupported Command.{other}")),
    })
}

fn triple(code: i64, id: Value, values: Value) -> Value {
    Value::Array(vec![Value::from(code), id, values])
}
