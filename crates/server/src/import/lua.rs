//! Just enough Lua to read Prosody's internal storage: `.dat` files
//! (`return <table>;`) and `.list` files (`item(<table>);` repeated), as
//! written by `prosody.util.serialization`. Values are strings, numbers,
//! booleans, nil and tables with `[key] = value`, `name = value` and
//! positional entries.

use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Nil,
    Bool(bool),
    Num(f64),
    Str(String),
    Table(Table),
}

/// A Lua table: positional entries and keyed entries.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Table {
    pub array: Vec<Value>,
    pub map: BTreeMap<Key, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Key {
    Str(String),
    Bool(bool),
    /// Integer keys (`[1] = …`) are folded into `array` when contiguous.
    Int(i64),
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            Value::Str(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_table(&self) -> Option<&Table> {
        match self {
            Value::Table(t) => Some(t),
            _ => None,
        }
    }
}

impl Table {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.map.get(&Key::Str(key.to_owned()))
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str)
    }

    pub fn table(&self, key: &str) -> Option<&Table> {
        self.get(key).and_then(Value::as_table)
    }

    /// Entries with string keys.
    pub fn string_entries(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.map.iter().filter_map(|(k, v)| match k {
            Key::Str(s) => Some((s.as_str(), v)),
            _ => None,
        })
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn ws(&mut self) {
        loop {
            while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                self.i += 1;
            }
            // `-- comment` to end of line.
            if self.s[self.i..].starts_with(b"--") {
                while self.i < self.s.len() && self.s[self.i] != b'\n' {
                    self.i += 1;
                }
                continue;
            }
            return;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> Result<()> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            bail!("expected {:?} at byte {}", c as char, self.i)
        }
    }

    fn word(&mut self) -> &'a str {
        self.ws();
        let start = self.i;
        while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_') {
            self.i += 1;
        }
        std::str::from_utf8(&self.s[start..self.i]).unwrap_or("")
    }

    fn value(&mut self) -> Result<Value> {
        match self.peek().ok_or_else(|| anyhow!("unexpected end"))? {
            b'{' => self.table().map(Value::Table),
            b'"' | b'\'' => self.string().map(Value::Str),
            b'[' if matches!(self.s.get(self.i + 1), Some(b'[' | b'=')) => self.long_string().map(Value::Str),
            b'-' | b'0'..=b'9' | b'.' => self.number().map(Value::Num),
            _ => match self.word() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "nil" => Ok(Value::Nil),
                w => bail!("unexpected {w:?} at byte {}", self.i),
            },
        }
    }

    fn number(&mut self) -> Result<f64> {
        self.ws();
        let start = self.i;
        while self.i < self.s.len()
            && matches!(self.s[self.i], b'-' | b'+' | b'.' | b'0'..=b'9' | b'e' | b'E' | b'x' | b'X' | b'a'..=b'f' | b'A'..=b'F')
        {
            self.i += 1;
        }
        let t = std::str::from_utf8(&self.s[start..self.i])?;
        if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            return Ok(i64::from_str_radix(h, 16)? as f64);
        }
        match t {
            "-nan" | "nan" => Ok(f64::NAN),
            _ => Ok(t.parse()?),
        }
    }

    fn string(&mut self) -> Result<String> {
        let q = self.s[self.i];
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let c = *self.s.get(self.i).ok_or_else(|| anyhow!("unterminated string"))?;
            self.i += 1;
            if c == q {
                break;
            }
            if c != b'\\' {
                out.push(c);
                continue;
            }
            let e = *self.s.get(self.i).ok_or_else(|| anyhow!("bad escape"))?;
            self.i += 1;
            match e {
                b'n' => out.push(b'\n'),
                b't' => out.push(b'\t'),
                b'r' => out.push(b'\r'),
                b'a' => out.push(7),
                b'b' => out.push(8),
                b'f' => out.push(12),
                b'v' => out.push(11),
                b'\n' => out.push(b'\n'),
                b'\r' => {
                    out.push(b'\n');
                    if self.s.get(self.i) == Some(&b'\n') {
                        self.i += 1;
                    }
                }
                b'x' => {
                    let h = std::str::from_utf8(self.s.get(self.i..self.i + 2).ok_or_else(|| anyhow!("bad \\x"))?)?;
                    out.push(u8::from_str_radix(h, 16)?);
                    self.i += 2;
                }
                b'z' => {
                    while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
                        self.i += 1;
                    }
                }
                b'0'..=b'9' => {
                    let mut n = (e - b'0') as u32;
                    for _ in 0..2 {
                        match self.s.get(self.i) {
                            Some(d @ b'0'..=b'9') => {
                                n = n * 10 + (d - b'0') as u32;
                                self.i += 1;
                            }
                            _ => break,
                        }
                    }
                    out.push(u8::try_from(n).map_err(|_| anyhow!("bad decimal escape"))?);
                }
                other => out.push(other),
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn long_string(&mut self) -> Result<String> {
        self.i += 1;
        let mut level = 0;
        while self.s.get(self.i) == Some(&b'=') {
            level += 1;
            self.i += 1;
        }
        self.eat(b'[')?;
        let close = format!("]{}]", "=".repeat(level));
        let rest = &self.s[self.i..];
        let end = rest
            .windows(close.len())
            .position(|w| w == close.as_bytes())
            .ok_or_else(|| anyhow!("unterminated long string"))?;
        let mut body = &rest[..end];
        if body.first() == Some(&b'\n') {
            body = &body[1..];
        }
        self.i += end + close.len();
        Ok(String::from_utf8_lossy(body).into_owned())
    }

    fn table(&mut self) -> Result<Table> {
        self.eat(b'{')?;
        let mut t = Table::default();
        let mut ints: BTreeMap<i64, Value> = BTreeMap::new();
        loop {
            match self.peek() {
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                None => bail!("unterminated table"),
                _ => {}
            }
            if self.peek() == Some(b'[') && !matches!(self.s.get(self.i + 1), Some(b'[' | b'=')) {
                self.i += 1;
                let k = self.value()?;
                self.eat(b']')?;
                self.eat(b'=')?;
                let v = self.value()?;
                match k {
                    Value::Str(s) => {
                        t.map.insert(Key::Str(s), v);
                    }
                    Value::Bool(b) => {
                        t.map.insert(Key::Bool(b), v);
                    }
                    Value::Num(n) if n.fract() == 0.0 => {
                        ints.insert(n as i64, v);
                    }
                    other => bail!("unsupported table key {other:?}"),
                }
            } else {
                // `name = value` or a positional value.
                let save = self.i;
                let w = self.word();
                if !w.is_empty() && !matches!(w, "true" | "false" | "nil") && self.peek() == Some(b'=') {
                    self.i += 1;
                    let v = self.value()?;
                    t.map.insert(Key::Str(w.to_owned()), v);
                } else {
                    self.i = save;
                    let v = self.value()?;
                    t.array.push(v);
                }
            }
            match self.peek() {
                Some(b',' | b';') => self.i += 1,
                Some(b'}') => {}
                _ => bail!("expected , ; or }} at byte {}", self.i),
            }
        }
        // Integer keys continuing the array (1-based) go into it.
        for (k, v) in ints {
            if k == t.array.len() as i64 + 1 {
                t.array.push(v);
            } else {
                t.map.insert(Key::Int(k), v);
            }
        }
        Ok(t)
    }
}

/// A `.dat` file: `return <value>;`.
pub fn parse_dat(text: &str) -> Result<Value> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    if p.word() != "return" {
        bail!("expected `return`");
    }
    let v = p.value()?;
    let _ = p.peek().filter(|c| *c == b';').map(|_| p.i += 1);
    Ok(v)
}

/// A `.list` file: `item(<table>);` repeated.
pub fn parse_list(text: &str) -> Result<Vec<Table>> {
    let mut p = Parser {
        s: text.as_bytes(),
        i: 0,
    };
    let mut out = Vec::new();
    loop {
        if p.peek().is_none() {
            break;
        }
        if p.word() != "item" {
            bail!("expected `item(` at byte {}", p.i);
        }
        p.eat(b'(')?;
        match p.value()? {
            Value::Table(t) => out.push(t),
            other => bail!("list item is not a table: {other:?}"),
        }
        p.eat(b')')?;
        if p.peek() == Some(b';') {
            p.i += 1;
        }
    }
    Ok(out)
}

/// Decode Prosody's file-name encoding (`%2e` → `.`).
pub fn decode_name(name: &str) -> String {
    let b = name.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len() + 1
            && let Some(h) = name.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(h, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_prosody_files() {
        let acct = parse_dat(
            "return {\n\t[\"stored_key\"] = \"183db897\";\n\t[\"iteration_count\"] = 10000;\n\t[false] = { [\"pending\"] = {}; };\n};\n",
        )
        .unwrap();
        let t = acct.as_table().unwrap();
        assert_eq!(t.str("stored_key"), Some("183db897"));
        assert_eq!(t.get("iteration_count").and_then(Value::as_f64), Some(10000.0));
        assert!(t.map.contains_key(&Key::Bool(false)));

        let list = parse_list(
            "item({\n\t{\n\t\t\"hello \\\"there\\\"\\\n2\";\n\t\t[\"name\"] = \"body\";\n\t\t[\"attr\"] = {};\n\t};\n\
             \t[\"key\"] = \"01a1\";\n\t[\"when\"] = 1794326347.57114005;\n\t[\"name\"] = \"message\";\n});\n\
             item({ [\"name\"] = \"x\"; [1] = \"a\"; [2] = \"b\"; });\n",
        )
        .unwrap();
        assert_eq!(list.len(), 2);
        let body = list[0].array[0].as_table().unwrap();
        assert_eq!(body.array[0].as_str(), Some("hello \"there\"\n2"));
        assert_eq!(list[0].get("when").and_then(Value::as_f64), Some(1_794_326_347.571_14));
        assert_eq!(list[1].array.len(), 2, "integer keys fold into the array");
        assert_eq!(decode_name("agent%2d3e45jec5hu"), "agent-3e45jec5hu");
        assert_eq!(
            decode_name("pep_urn%3axmpp%3aomemo%3a2%3adevices"),
            "pep_urn:xmpp:omemo:2:devices"
        );
        assert_eq!(
            parse_dat("return \"\\226\\128\\148\";").unwrap(),
            Value::Str("—".into())
        );
    }
}
