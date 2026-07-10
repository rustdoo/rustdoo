//! Minimal Python literal parser — just enough for `__manifest__.py`:
//! dicts, lists, tuples, strings (incl. triple-quoted, prefixes and
//! adjacent concatenation), numbers, True/False/None, comments.

use rusdoo_core::RusdooError;
use serde_json::{Map, Number, Value};

/// Nesting bound: parsing is recursive, so unbounded bracket nesting in a
/// hostile manifest would overflow the stack instead of returning Err.
const MAX_LITERAL_DEPTH: usize = 100;

pub fn parse_py_literal(source: &str) -> Result<Value, RusdooError> {
    let mut parser = Parser {
        chars: source.chars().collect(),
        pos: 0,
    };
    let value = parser.parse_value(0)?;
    parser.skip_trivia();
    if parser.pos < parser.chars.len() {
        return Err(parser.error("unexpected trailing content"));
    }
    Ok(value)
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn error(&self, message: &str) -> RusdooError {
        RusdooError::Validation(format!("python literal: {message} at offset {}", self.pos))
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_trivia(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.pos += 1;
            } else if c == '#' {
                while let Some(c) = self.bump() {
                    if c == '\n' {
                        break;
                    }
                }
            } else {
                break;
            }
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Value, RusdooError> {
        if depth > MAX_LITERAL_DEPTH {
            return Err(self.error("literal nesting too deep"));
        }
        self.skip_trivia();
        match self.peek() {
            Some('{') => self.parse_dict(depth),
            Some('[') => self.parse_seq(']', depth).map(Value::Array),
            Some('(') => self.parse_seq(')', depth).map(Value::Array),
            Some('\'') | Some('"') => self.parse_string_concat().map(Value::String),
            Some(c) if c.is_ascii_digit() || c == '-' || c == '+' => self.parse_number(),
            Some(c) if c.is_alphabetic() || c == '_' => {
                if self.at_string_start() {
                    self.parse_string_concat().map(Value::String)
                } else {
                    self.parse_word()
                }
            }
            _ => Err(self.error("expected a value")),
        }
    }

    fn parse_word(&mut self) -> Result<Value, RusdooError> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_alphanumeric() || c == '_') {
            self.pos += 1;
        }
        let word: String = self.chars[start..self.pos].iter().collect();
        match word.as_str() {
            "True" => Ok(Value::Bool(true)),
            "False" => Ok(Value::Bool(false)),
            "None" => Ok(Value::Null),
            other => Err(self.error(&format!("unsupported identifier {other:?}"))),
        }
    }

    fn at_string_start(&self) -> bool {
        match self.peek() {
            Some('\'') | Some('"') => true,
            Some(c) if "uUrRbB".contains(c) => {
                matches!(self.peek_at(1), Some('\'') | Some('"'))
            }
            _ => false,
        }
    }

    /// Adjacent string literals concatenate, as in Python source.
    fn parse_string_concat(&mut self) -> Result<String, RusdooError> {
        let mut out = self.parse_one_string()?;
        loop {
            let save = self.pos;
            self.skip_trivia();
            if self.at_string_start() {
                out.push_str(&self.parse_one_string()?);
            } else {
                self.pos = save;
                break;
            }
        }
        Ok(out)
    }

    fn parse_one_string(&mut self) -> Result<String, RusdooError> {
        let mut raw = false;
        while let Some(c) = self.peek() {
            match c {
                'u' | 'U' | 'b' | 'B' => {
                    self.pos += 1;
                }
                'r' | 'R' => {
                    raw = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let quote = self.bump().ok_or_else(|| self.error("expected a string"))?;
        if quote != '\'' && quote != '"' {
            return Err(self.error("expected a quote"));
        }
        let triple = self.peek() == Some(quote) && self.peek_at(1) == Some(quote);
        if triple {
            self.pos += 2;
        }
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else {
                return Err(self.error("unterminated string"));
            };
            if c == '\\' && !raw {
                let Some(escape) = self.bump() else {
                    return Err(self.error("unterminated escape"));
                };
                match escape {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '0' => out.push('\0'),
                    '\\' | '\'' | '"' => out.push(escape),
                    // escaped newline: line continuation
                    '\n' => {}
                    'u' => out.push(self.parse_hex_escape(4)?),
                    'x' => out.push(self.parse_hex_escape(2)?),
                    // pass unknown escapes through untouched
                    other => {
                        out.push('\\');
                        out.push(other);
                    }
                }
            } else if c == quote {
                if triple {
                    if self.peek() == Some(quote) && self.peek_at(1) == Some(quote) {
                        self.pos += 2;
                        break;
                    }
                    out.push(c);
                } else {
                    break;
                }
            } else {
                out.push(c);
            }
        }
        Ok(out)
    }

    fn parse_hex_escape(&mut self, digits: usize) -> Result<char, RusdooError> {
        let mut code = 0u32;
        for _ in 0..digits {
            let Some(c) = self.bump().and_then(|c| c.to_digit(16)) else {
                return Err(self.error("invalid hex escape"));
            };
            code = code * 16 + c;
        }
        char::from_u32(code).ok_or_else(|| self.error("invalid unicode escape"))
    }

    fn parse_seq(&mut self, end: char, depth: usize) -> Result<Vec<Value>, RusdooError> {
        self.pos += 1; // opener
        let mut items = Vec::new();
        loop {
            self.skip_trivia();
            if self.peek() == Some(end) {
                self.pos += 1;
                break;
            }
            items.push(self.parse_value(depth + 1)?);
            self.skip_trivia();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some(c) if c == end => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("expected ',' or a closing bracket")),
            }
        }
        Ok(items)
    }

    /// `{...}` is a dict when the first element is followed by ':',
    /// otherwise a set literal (represented as a list).
    fn parse_dict(&mut self, depth: usize) -> Result<Value, RusdooError> {
        self.pos += 1; // '{'
        self.skip_trivia();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Value::Object(Map::new()));
        }
        let first = self.parse_value(depth + 1)?;
        self.skip_trivia();
        if self.peek() != Some(':') {
            return self.parse_set_rest(first, depth).map(Value::Array);
        }
        let Value::String(mut key) = first else {
            return Err(self.error("dict keys must be strings"));
        };
        let mut map = Map::new();
        loop {
            self.pos += 1; // ':'
            let value = self.parse_value(depth + 1)?;
            map.insert(key, value);
            self.skip_trivia();
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("expected ',' or '}'")),
            }
            self.skip_trivia();
            if self.peek() == Some('}') {
                self.pos += 1;
                break;
            }
            let Value::String(next_key) = self.parse_value(depth + 1)? else {
                return Err(self.error("dict keys must be strings"));
            };
            key = next_key;
            self.skip_trivia();
            if self.peek() != Some(':') {
                return Err(self.error("expected ':'"));
            }
        }
        Ok(Value::Object(map))
    }

    /// Remaining elements of a set literal, after its first value.
    fn parse_set_rest(&mut self, first: Value, depth: usize) -> Result<Vec<Value>, RusdooError> {
        let mut items = vec![first];
        loop {
            match self.peek() {
                Some(',') => {
                    self.pos += 1;
                }
                Some('}') => {
                    self.pos += 1;
                    break;
                }
                _ => return Err(self.error("expected ',' or '}' in set literal")),
            }
            self.skip_trivia();
            if self.peek() == Some('}') {
                self.pos += 1;
                break;
            }
            items.push(self.parse_value(depth + 1)?);
            self.skip_trivia();
        }
        Ok(items)
    }

    fn parse_number(&mut self) -> Result<Value, RusdooError> {
        let start = self.pos;
        if matches!(self.peek(), Some('+') | Some('-')) {
            self.pos += 1;
        }
        let mut is_float = false;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() || c == '_' {
                self.pos += 1;
            } else if c == '.' {
                is_float = true;
                self.pos += 1;
            } else if c == 'e' || c == 'E' {
                is_float = true;
                self.pos += 1;
                if matches!(self.peek(), Some('+') | Some('-')) {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
        let text: String = self.chars[start..self.pos]
            .iter()
            .filter(|c| **c != '_')
            .collect();
        if is_float {
            text.parse::<f64>()
                .ok()
                .and_then(Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| self.error("invalid float"))
        } else {
            text.parse::<i64>()
                .map(Value::from)
                .map_err(|_| self.error("invalid integer"))
        }
    }
}
