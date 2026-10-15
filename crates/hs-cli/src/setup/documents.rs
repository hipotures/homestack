//! Reading and patching an application's configuration document on the
//! desktop (Python `setup_documents.py`): the workspace only moves bytes.
//!
//! TOML keeps its formatting (`toml_edit`, as `tomlkit`); JSON keeps its
//! key order and is written as `json.dumps(indent=2, ensure_ascii=False)`;
//! a YAML document that changes is written again from its values (aliases
//! expanded, merge keys applied), where `ruamel` kept its comments. A
//! document that does not change is returned byte for byte.

use toml_edit::{DocumentMut, Item, TableLike};
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

use crate::AppError;

/// A value as the comparisons see it (Python's `_plain_value`).
#[derive(Debug, Clone)]
pub enum Plain {
    Null,
    Bool(bool),
    Int(i128),
    Float(f64),
    Str(String),
    List(Vec<Plain>),
    Map(Vec<(String, Plain)>),
    /// A TOML date or anything else no desired value can equal.
    Other(String),
}

impl Plain {
    /// A desired value of `client.toml`.
    #[must_use]
    pub fn of_toml(value: &toml::Value) -> Self {
        match value {
            toml::Value::String(text) => Self::Str(text.clone()),
            toml::Value::Integer(number) => Self::Int(i128::from(*number)),
            toml::Value::Float(number) => Self::Float(*number),
            toml::Value::Boolean(flag) => Self::Bool(*flag),
            toml::Value::Datetime(datetime) => Self::Other(datetime.to_string()),
            toml::Value::Array(items) => Self::List(items.iter().map(Self::of_toml).collect()),
            toml::Value::Table(table) => Self::Map(table.iter().map(|(k, v)| (k.clone(), Self::of_toml(v))).collect()),
        }
    }

    /// As JSON, for the inspection's `current`.
    #[must_use]
    pub fn json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(flag) => serde_json::Value::Bool(*flag),
            Self::Int(number) => i64::try_from(*number)
                .map(serde_json::Value::from)
                .unwrap_or_else(|_| serde_json::Value::String(number.to_string())),
            Self::Float(number) => serde_json::Value::from(*number),
            Self::Str(text) | Self::Other(text) => serde_json::Value::String(text.clone()),
            Self::List(items) => serde_json::Value::Array(items.iter().map(Self::json).collect()),
            Self::Map(entries) => {
                serde_json::Value::Object(entries.iter().map(|(k, v)| (k.clone(), v.json())).collect())
            }
        }
    }
}

/// Python's `_same_value`: types matter (a bool is no integer, an integer
/// no float); mappings compare by keys, not order.
#[must_use]
pub fn same_value(left: &Plain, right: &Plain) -> bool {
    match (left, right) {
        (Plain::Null, Plain::Null) => true,
        (Plain::Bool(a), Plain::Bool(b)) => a == b,
        (Plain::Int(a), Plain::Int(b)) => a == b,
        #[allow(clippy::float_cmp)]
        (Plain::Float(a), Plain::Float(b)) => a == b,
        (Plain::Str(a), Plain::Str(b)) => a == b,
        (Plain::List(a), Plain::List(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_value(x, y)),
        (Plain::Map(a), Plain::Map(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(key, value)| b.iter().any(|(other, child)| other == key && same_value(value, child)))
        }
        _ => false,
    }
}

fn display_path(path: &[String]) -> String {
    if path.is_empty() {
        "<root>".to_owned()
    } else {
        path.join(".")
    }
}

fn requires_mapping(path: &[String], at: &[String]) -> AppError {
    AppError::new(format!(
        "Configuration path {} requires a mapping at {}",
        display_path(path),
        display_path(at)
    ))
}

/// One leaf of a document: absent, or its current value.
type Found = Option<Plain>;

/// A parsed document of one of the three formats.
enum Document {
    Toml(DocumentMut),
    Json(Json),
    Yaml(Yaml),
}

impl Document {
    fn parse(format: &str, text: Option<&str>) -> Result<Self, AppError> {
        match format {
            "toml" => match text {
                None => Ok(Self::Toml(DocumentMut::new())),
                Some(text) if text.trim().is_empty() => Ok(Self::Toml(DocumentMut::new())),
                Some(text) => text
                    .parse::<DocumentMut>()
                    .map(Self::Toml)
                    // No parser details: they can contain document text.
                    .map_err(|_| AppError::new("TOML configuration could not be parsed")),
            },
            "json" => match text {
                None => Ok(Self::Json(Json::Object(Vec::new()))),
                Some(text) => Json::parse(text)
                    .map(Self::Json)
                    .ok_or_else(|| AppError::new("JSON configuration could not be parsed")),
            },
            "yaml" => parse_yaml(text).map(Self::Yaml),
            _ => Err(AppError::new(
                "Structured configuration format must be TOML, JSON or YAML",
            )),
        }
    }

    fn is_mapping(&self) -> bool {
        match self {
            Self::Toml(_) => true,
            Self::Json(json) => matches!(json, Json::Object(_)),
            Self::Yaml(yaml) => matches!(yaml, Yaml::Hash(_)),
        }
    }

    /// The value at `path`, checking every parent is a mapping.
    fn lookup(&self, path: &[String]) -> Result<Found, AppError> {
        match self {
            Self::Toml(document) => {
                let mut current: &dyn TableLike = document.as_table();
                for (index, segment) in path[..path.len() - 1].iter().enumerate() {
                    let Some(child) = current.get(segment) else {
                        return Ok(None);
                    };
                    current = child
                        .as_table_like()
                        .ok_or_else(|| requires_mapping(path, &path[..=index]))?;
                }
                Ok(current.get(&path[path.len() - 1]).map(toml_plain))
            }
            Self::Json(json) => {
                let mut current = json;
                for (index, segment) in path[..path.len() - 1].iter().enumerate() {
                    let Json::Object(entries) = current else {
                        return Err(requires_mapping(path, &path[..index]));
                    };
                    let Some((_, child)) = entries.iter().find(|(key, _)| key == segment) else {
                        return Ok(None);
                    };
                    if !matches!(child, Json::Object(_)) {
                        return Err(requires_mapping(path, &path[..=index]));
                    }
                    current = child;
                }
                let Json::Object(entries) = current else {
                    return Err(requires_mapping(path, &[]));
                };
                Ok(entries
                    .iter()
                    .find(|(key, _)| *key == path[path.len() - 1])
                    .map(|(_, v)| v.plain()))
            }
            Self::Yaml(yaml) => {
                let mut current = yaml;
                for (index, segment) in path[..path.len() - 1].iter().enumerate() {
                    let Yaml::Hash(hash) = current else {
                        return Err(requires_mapping(path, &path[..index]));
                    };
                    let Some(child) = hash.get(&Yaml::String(segment.clone())) else {
                        return Ok(None);
                    };
                    if !matches!(child, Yaml::Hash(_)) {
                        return Err(requires_mapping(path, &path[..=index]));
                    }
                    current = child;
                }
                let Yaml::Hash(hash) = current else {
                    return Err(requires_mapping(path, &[]));
                };
                Ok(hash.get(&Yaml::String(path[path.len() - 1].clone())).map(yaml_plain))
            }
        }
    }

    /// Set `path` to `value`: `matching`, `different` or `missing`.
    fn set(&mut self, path: &[String], value: &toml::Value) -> Result<String, AppError> {
        let found = self.lookup(path)?;
        let desired = Plain::of_toml(value);
        if let Some(current) = &found
            && same_value(current, &desired)
        {
            return Ok("matching".to_owned());
        }
        match self {
            Self::Toml(document) => toml_set(document, path, value, found.is_some())?,
            Self::Json(json) => json_set(json, path, Json::of_toml(value)),
            Self::Yaml(yaml) => yaml_set(yaml, path, yaml_of_toml(value)),
        }
        Ok(if found.is_some() { "different" } else { "missing" }.to_owned())
    }

    fn dump(&self) -> Result<String, AppError> {
        match self {
            Self::Toml(document) => Ok(document.to_string()),
            Self::Json(json) => Ok(format!("{}\n", json.dump(0))),
            Self::Yaml(yaml) => {
                let mut out = String::new();
                YamlEmitter::new(&mut out)
                    .dump(yaml)
                    .map_err(|_| AppError::new("YAML configuration could not be serialized"))?;
                let body = out
                    .strip_prefix("---\n")
                    .or_else(|| out.strip_prefix("---"))
                    .unwrap_or(&out);
                let body = body.trim_start_matches(' ');
                Ok(if body == "{}" {
                    "{}\n".to_owned()
                } else {
                    format!("{body}\n")
                })
            }
        }
    }
}

fn toml_value_plain(value: &toml_edit::Value) -> Plain {
    match value {
        toml_edit::Value::String(text) => Plain::Str(text.value().clone()),
        toml_edit::Value::Integer(number) => Plain::Int(i128::from(*number.value())),
        toml_edit::Value::Float(number) => Plain::Float(*number.value()),
        toml_edit::Value::Boolean(flag) => Plain::Bool(*flag.value()),
        toml_edit::Value::Datetime(datetime) => Plain::Other(datetime.value().to_string()),
        toml_edit::Value::Array(items) => Plain::List(items.iter().map(toml_value_plain).collect()),
        toml_edit::Value::InlineTable(table) => {
            Plain::Map(table.iter().map(|(k, v)| (k.to_owned(), toml_value_plain(v))).collect())
        }
    }
}

fn toml_plain(item: &Item) -> Plain {
    match item {
        Item::None => Plain::Null,
        Item::Value(value) => toml_value_plain(value),
        Item::Table(table) => Plain::Map(table.iter().map(|(k, v)| (k.to_owned(), toml_plain(v))).collect()),
        Item::ArrayOfTables(tables) => Plain::List(
            tables
                .iter()
                .map(|t| Plain::Map(t.iter().map(|(k, v)| (k.to_owned(), toml_plain(v))).collect()))
                .collect(),
        ),
    }
}

fn toml_edit_value(value: &toml::Value) -> toml_edit::Value {
    match value {
        toml::Value::String(text) => toml_edit::Value::from(text.as_str()),
        toml::Value::Integer(number) => toml_edit::Value::from(*number),
        toml::Value::Float(number) => toml_edit::Value::from(*number),
        toml::Value::Boolean(flag) => toml_edit::Value::from(*flag),
        toml::Value::Datetime(datetime) => datetime
            .to_string()
            .parse::<toml_edit::Value>()
            .unwrap_or_else(|_| toml_edit::Value::from(datetime.to_string())),
        toml::Value::Array(items) => {
            let mut array = toml_edit::Array::new();
            for item in items {
                array.push(toml_edit_value(item));
            }
            toml_edit::Value::Array(array)
        }
        toml::Value::Table(table) => {
            let mut inline = toml_edit::InlineTable::new();
            for (key, child) in table {
                inline.insert(key, toml_edit_value(child));
            }
            toml_edit::Value::InlineTable(inline)
        }
    }
}

/// Set a TOML leaf: an existing value keeps its comments; a missing one
/// gets its parents, an inline table on the way becoming a standard table.
fn toml_set(document: &mut DocumentMut, path: &[String], value: &toml::Value, exists: bool) -> Result<(), AppError> {
    let mut current: &mut dyn TableLike = document.as_table_mut();
    for (index, segment) in path[..path.len() - 1].iter().enumerate() {
        if !current.contains_key(segment) {
            let mut table = toml_edit::Table::new();
            table.set_implicit(true);
            current.insert(segment, Item::Table(table));
        } else if !exists
            && let Some(item) = current.get_mut(segment)
            && item.is_inline_table()
        {
            // An inline table cannot hold a nested standard table: the
            // edited branch is promoted with all its keys.
            let inline = std::mem::take(item);
            if let Item::Value(toml_edit::Value::InlineTable(inline)) = inline {
                *item = Item::Table(inline.into_table());
            }
            // A header has no room for the spaces of `key = {`.
            if let Some(mut header) = current.key_mut(segment) {
                header.leaf_decor_mut().clear();
            }
        }
        current = current
            .get_mut(segment)
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| requires_mapping(path, &path[..=index]))?;
    }
    let key = &path[path.len() - 1];
    let mut new_value = toml_edit_value(value);
    // An existing value keeps its key (and the comments above it) and
    // the comment after it.
    if let Some(item) = current.get_mut(key) {
        if let Item::Value(old) = item {
            *new_value.decor_mut() = old.decor().clone();
        }
        *item = Item::Value(new_value);
    } else {
        current.insert(key, Item::Value(new_value));
    }
    Ok(())
}

fn parse_yaml(text: Option<&str>) -> Result<Yaml, AppError> {
    let Some(text) = text.filter(|text| !text.trim().is_empty()) else {
        return Ok(Yaml::Hash(yaml_rust2::yaml::Hash::new()));
    };
    let invalid = || AppError::new("YAML configuration could not be parsed");
    let mut documents = YamlLoader::load_from_str(text).map_err(|_| invalid())?;
    if documents.len() > 1 {
        return Err(invalid());
    }
    let Some(document) = documents.pop() else {
        return Ok(Yaml::Hash(yaml_rust2::yaml::Hash::new()));
    };
    if matches!(document, Yaml::Null) {
        // A comment-only file is an empty mapping; an explicit null is a
        // scalar document, refused as a root.
        let content = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && *line != "---" && *line != "...")
            .any(|line| !line.starts_with('#'));
        if !content {
            return Ok(Yaml::Hash(yaml_rust2::yaml::Hash::new()));
        }
    }
    if matches!(document, Yaml::BadValue) {
        return Err(invalid());
    }
    Ok(merged(document))
}

/// Merge keys (`<<`) applied, as `ruamel`'s mappings show them.
fn merged(value: Yaml) -> Yaml {
    match value {
        Yaml::Hash(hash) => {
            let merge_key = Yaml::String("<<".to_owned());
            let mut out = yaml_rust2::yaml::Hash::new();
            let mut sources = Vec::new();
            for (key, child) in hash {
                if key == merge_key {
                    match merged(child) {
                        Yaml::Hash(source) => sources.push(source),
                        Yaml::Array(items) => sources.extend(items.into_iter().filter_map(|item| match item {
                            Yaml::Hash(source) => Some(source),
                            _ => None,
                        })),
                        _ => {}
                    }
                    continue;
                }
                out.insert(key, merged(child));
            }
            for source in sources {
                for (key, child) in source {
                    if !out.contains_key(&key) {
                        out.insert(key, child);
                    }
                }
            }
            Yaml::Hash(out)
        }
        Yaml::Array(items) => Yaml::Array(items.into_iter().map(merged).collect()),
        other => other,
    }
}

fn yaml_plain(value: &Yaml) -> Plain {
    match value {
        Yaml::Real(text) => text.parse().map_or_else(|_| Plain::Other(text.clone()), Plain::Float),
        Yaml::Integer(number) => Plain::Int(i128::from(*number)),
        Yaml::String(text) => Plain::Str(text.clone()),
        Yaml::Boolean(flag) => Plain::Bool(*flag),
        Yaml::Array(items) => Plain::List(items.iter().map(yaml_plain).collect()),
        Yaml::Hash(hash) => Plain::Map(
            hash.iter()
                .map(|(key, child)| {
                    let key = match key {
                        Yaml::String(text) | Yaml::Real(text) => text.clone(),
                        Yaml::Integer(number) => number.to_string(),
                        Yaml::Boolean(flag) => flag.to_string(),
                        _ => String::new(),
                    };
                    (key, yaml_plain(child))
                })
                .collect(),
        ),
        Yaml::Null => Plain::Null,
        Yaml::Alias(_) | Yaml::BadValue => Plain::Other(String::new()),
    }
}

fn yaml_of_toml(value: &toml::Value) -> Yaml {
    match value {
        toml::Value::String(text) => Yaml::String(text.clone()),
        toml::Value::Integer(number) => Yaml::Integer(*number),
        toml::Value::Float(number) => Yaml::Real(python_float(*number)),
        toml::Value::Boolean(flag) => Yaml::Boolean(*flag),
        toml::Value::Datetime(datetime) => Yaml::String(datetime.to_string()),
        toml::Value::Array(items) => Yaml::Array(items.iter().map(yaml_of_toml).collect()),
        toml::Value::Table(table) => {
            let mut hash = yaml_rust2::yaml::Hash::new();
            for (key, child) in table {
                hash.insert(Yaml::String(key.clone()), yaml_of_toml(child));
            }
            Yaml::Hash(hash)
        }
    }
}

fn yaml_set(document: &mut Yaml, path: &[String], value: Yaml) {
    let mut current = document;
    for segment in &path[..path.len() - 1] {
        let Yaml::Hash(hash) = current else { return };
        let key = Yaml::String(segment.clone());
        if !hash.contains_key(&key) {
            hash.insert(key.clone(), Yaml::Hash(yaml_rust2::yaml::Hash::new()));
        }
        let Some(child) = hash.get_mut(&key) else { return };
        current = child;
    }
    if let Yaml::Hash(hash) = current {
        let key = Yaml::String(path[path.len() - 1].clone());
        match hash.get_mut(&key) {
            Some(slot) => *slot = value,
            None => {
                hash.insert(key, value);
            }
        }
    }
}

/// A JSON value in its document's key order.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    /// An integer, as written (Python's integers have no limit).
    Int(String),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn of_toml(value: &toml::Value) -> Self {
        match value {
            toml::Value::String(text) => Self::Str(text.clone()),
            toml::Value::Integer(number) => Self::Int(number.to_string()),
            toml::Value::Float(number) => Self::Float(*number),
            toml::Value::Boolean(flag) => Self::Bool(*flag),
            toml::Value::Datetime(datetime) => Self::Str(datetime.to_string()),
            toml::Value::Array(items) => Self::Array(items.iter().map(Self::of_toml).collect()),
            toml::Value::Table(table) => {
                Self::Object(table.iter().map(|(k, v)| (k.clone(), Self::of_toml(v))).collect())
            }
        }
    }

    fn plain(&self) -> Plain {
        match self {
            Self::Null => Plain::Null,
            Self::Bool(flag) => Plain::Bool(*flag),
            Self::Int(text) => text.parse().map_or_else(|_| Plain::Other(text.clone()), Plain::Int),
            Self::Float(number) => Plain::Float(*number),
            Self::Str(text) => Plain::Str(text.clone()),
            Self::Array(items) => Plain::List(items.iter().map(Self::plain).collect()),
            Self::Object(entries) => Plain::Map(entries.iter().map(|(k, v)| (k.clone(), v.plain())).collect()),
        }
    }

    /// `json.loads` without NaN or Infinity and without duplicate keys.
    fn parse(text: &str) -> Option<Self> {
        let mut parser = JsonParser {
            bytes: text.as_bytes(),
            at: 0,
        };
        parser.space();
        let value = parser.value(0)?;
        parser.space();
        (parser.at == parser.bytes.len()).then_some(value)
    }

    /// `json.dumps(value, ensure_ascii=False, indent=2)` at `depth`.
    fn dump(&self, depth: usize) -> String {
        let indent = |depth: usize| "  ".repeat(depth);
        match self {
            Self::Null => "null".to_owned(),
            Self::Bool(flag) => if *flag { "true" } else { "false" }.to_owned(),
            Self::Int(text) => text.clone(),
            Self::Float(number) => python_float(*number),
            Self::Str(text) => json_string(text),
            Self::Array(items) if items.is_empty() => "[]".to_owned(),
            Self::Array(items) => {
                let inner: Vec<String> = items
                    .iter()
                    .map(|item| format!("{}{}", indent(depth + 1), item.dump(depth + 1)))
                    .collect();
                format!("[\n{}\n{}]", inner.join(",\n"), indent(depth))
            }
            Self::Object(entries) if entries.is_empty() => "{}".to_owned(),
            Self::Object(entries) => {
                let inner: Vec<String> = entries
                    .iter()
                    .map(|(key, value)| format!("{}{}: {}", indent(depth + 1), json_string(key), value.dump(depth + 1)))
                    .collect();
                format!("{{\n{}\n{}}}", inner.join(",\n"), indent(depth))
            }
        }
    }
}

fn json_set(document: &mut Json, path: &[String], value: Json) {
    let mut current = document;
    for segment in &path[..path.len() - 1] {
        let Json::Object(entries) = current else { return };
        let index = match entries.iter().position(|(key, _)| key == segment) {
            Some(index) => index,
            None => {
                entries.push((segment.clone(), Json::Object(Vec::new())));
                entries.len() - 1
            }
        };
        current = &mut entries[index].1;
    }
    if let Json::Object(entries) = current {
        let key = &path[path.len() - 1];
        match entries.iter_mut().find(|(known, _)| known == key) {
            Some(slot) => slot.1 = value,
            None => entries.push((key.clone(), value)),
        }
    }
}

/// A JSON string as Python writes it with `ensure_ascii=False`.
fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Python's `repr` of a float: the shortest digits, positional between
/// 1e-4 and 1e16, else `1e+16` style.
#[must_use]
pub fn python_float(number: f64) -> String {
    if number.is_nan() {
        return "NaN".to_owned();
    }
    if number.is_infinite() {
        return if number > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if number == 0.0 {
        return if number.is_sign_negative() { "-0.0" } else { "0.0" }.to_owned();
    }
    let scientific = format!("{number:e}");
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if negative { "-" } else { "" };
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let text = if point <= 0 {
            format!("0.{}{digits}", "0".repeat(usize::try_from(-point).unwrap_or(0)))
        } else {
            let point = usize::try_from(point).unwrap_or(0);
            if digits.len() <= point {
                format!("{digits}{}.0", "0".repeat(point - digits.len()))
            } else {
                format!("{}.{}", &digits[..point], &digits[point..])
            }
        };
        return format!("{sign}{text}");
    }
    let rest = if digits.len() > 1 {
        format!(".{}", &digits[1..])
    } else {
        String::new()
    };
    let exp_sign = if exponent < 0 { "-" } else { "+" };
    format!("{sign}{}{rest}e{exp_sign}{:02}", &digits[..1], exponent.abs())
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl JsonParser<'_> {
    fn space(&mut self) {
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            self.at += 1;
        }
    }

    fn eat(&mut self, text: &str) -> bool {
        if self.bytes[self.at..].starts_with(text.as_bytes()) {
            self.at += text.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Option<Json> {
        if depth > 1000 {
            return None;
        }
        self.space();
        match *self.bytes.get(self.at)? {
            b'{' => {
                self.at += 1;
                let mut entries: Vec<(String, Json)> = Vec::new();
                self.space();
                if self.eat("}") {
                    return Some(Json::Object(entries));
                }
                loop {
                    self.space();
                    let key = self.string()?;
                    if entries.iter().any(|(known, _)| *known == key) {
                        return None;
                    }
                    self.space();
                    if !self.eat(":") {
                        return None;
                    }
                    let value = self.value(depth + 1)?;
                    entries.push((key, value));
                    self.space();
                    if self.eat(",") {
                        continue;
                    }
                    return self.eat("}").then_some(Json::Object(entries));
                }
            }
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                self.space();
                if self.eat("]") {
                    return Some(Json::Array(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.space();
                    if self.eat(",") {
                        continue;
                    }
                    return self.eat("]").then_some(Json::Array(items));
                }
            }
            b'"' => self.string().map(Json::Str),
            b't' => self.eat("true").then_some(Json::Bool(true)),
            b'f' => self.eat("false").then_some(Json::Bool(false)),
            b'n' => self.eat("null").then_some(Json::Null),
            _ => self.number(),
        }
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.at;
        let mut float = false;
        if self.bytes.get(self.at) == Some(&b'-') {
            self.at += 1;
        }
        let digits = |parser: &mut Self| {
            let begin = parser.at;
            while parser.bytes.get(parser.at).is_some_and(u8::is_ascii_digit) {
                parser.at += 1;
            }
            parser.at > begin
        };
        let int_start = self.at;
        if !digits(self) {
            return None;
        }
        // No leading zeros, as JSON (and Python) require.
        if self.bytes[int_start] == b'0' && self.at - int_start > 1 {
            return None;
        }
        if self.bytes.get(self.at) == Some(&b'.') {
            self.at += 1;
            float = true;
            if !digits(self) {
                return None;
            }
        }
        if matches!(self.bytes.get(self.at), Some(b'e' | b'E')) {
            self.at += 1;
            float = true;
            if matches!(self.bytes.get(self.at), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !digits(self) {
                return None;
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at]).ok()?;
        if float {
            text.parse().ok().filter(|n: &f64| n.is_finite()).map(Json::Float)
        } else {
            Some(Json::Int(text.to_owned()))
        }
    }

    fn string(&mut self) -> Option<String> {
        if !self.eat("\"") {
            return None;
        }
        let mut out = String::new();
        loop {
            let byte = *self.bytes.get(self.at)?;
            match byte {
                b'"' => {
                    self.at += 1;
                    return Some(out);
                }
                b'\\' => {
                    self.at += 1;
                    let escape = *self.bytes.get(self.at)?;
                    self.at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let unit = self.hex4()?;
                            if (0xd800..0xdc00).contains(&unit) && self.eat("\\u") {
                                let low = self.hex4()?;
                                let code = 0x10000 + ((unit - 0xd800) << 10) + (low.wrapping_sub(0xdc00) & 0x3ff);
                                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                            } else {
                                out.push(char::from_u32(unit).unwrap_or('\u{fffd}'));
                            }
                        }
                        _ => return None,
                    }
                }
                // Python's json takes control characters only escaped.
                0..=0x1f => return None,
                _ => {
                    let rest = std::str::from_utf8(&self.bytes[self.at..]).ok()?;
                    let c = rest.chars().next()?;
                    out.push(c);
                    self.at += c.len_utf8();
                }
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let text = std::str::from_utf8(self.bytes.get(self.at..self.at + 4)?).ok()?;
        let value = u32::from_str_radix(text, 16).ok()?;
        self.at += 4;
        Some(value)
    }
}

fn check_leaves(leaves: &[(Vec<String>, toml::Value)]) -> Result<(), AppError> {
    let mut seen: Vec<&Vec<String>> = Vec::new();
    for (path, _) in leaves {
        if path.is_empty() {
            return Err(AppError::new(
                "Structured configuration paths must contain one or more text keys",
            ));
        }
        if seen.contains(&path) {
            return Err(AppError::new(
                "Structured configuration contains duplicate managed paths",
            ));
        }
        seen.push(path);
    }
    Ok(())
}

fn check_format(format: &str) -> Result<String, AppError> {
    let format = format.to_lowercase();
    if ["toml", "json", "yaml"].contains(&format.as_str()) {
        Ok(format)
    } else {
        Err(AppError::new(
            "Structured configuration format must be TOML, JSON or YAML",
        ))
    }
}

/// Each leaf's live state (`matching`, `different`, `missing`,
/// `unavailable`) and its value when it exists; nothing else of the
/// document is read out (Python `inspect_document_values`).
pub fn inspect_document_values(
    format: &str,
    text: Option<&str>,
    leaves: &[(Vec<String>, toml::Value)],
) -> Result<Vec<(String, Option<Plain>)>, AppError> {
    let format = check_format(format)?;
    check_leaves(leaves)?;
    let document = Document::parse(&format, text)?;
    if !document.is_mapping() {
        return Ok(vec![("unavailable".to_owned(), None); leaves.len()]);
    }
    Ok(leaves
        .iter()
        .map(|(path, desired)| match document.lookup(path) {
            Err(_) => ("unavailable".to_owned(), None),
            Ok(None) => ("missing".to_owned(), None),
            Ok(Some(current)) => {
                let state = if same_value(&current, &Plain::of_toml(desired)) {
                    "matching"
                } else {
                    "different"
                };
                (state.to_owned(), Some(current))
            }
        })
        .collect())
}

/// Python's `inspect_document`: the states only.
pub fn inspect_document(
    format: &str,
    text: Option<&str>,
    leaves: &[(Vec<String>, toml::Value)],
) -> Result<Vec<String>, AppError> {
    Ok(inspect_document_values(format, text, leaves)?
        .into_iter()
        .map(|(state, _)| state)
        .collect())
}

/// Patch the declared leaves into the document; the candidate text and each
/// leaf's state. A document already right comes back byte for byte.
pub fn merge_document(
    format: &str,
    text: Option<&str>,
    leaves: &[(Vec<String>, toml::Value)],
) -> Result<(String, Vec<String>), AppError> {
    let format = check_format(format)?;
    check_leaves(leaves)?;
    let mut document = Document::parse(&format, text)?;
    if !document.is_mapping() {
        return Err(AppError::new(
            "Structured configuration document root must be a mapping",
        ));
    }
    // Every path first: a structural conflict blocks the whole candidate.
    for (path, _) in leaves {
        document.lookup(path)?;
    }
    let mut states = Vec::new();
    for (path, value) in leaves {
        states.push(document.set(path, value)?);
    }
    let candidate = match text {
        Some(text) if states.iter().all(|state| state == "matching") => text.to_owned(),
        _ => document.dump()?,
    };
    // The written candidate is read again and every leaf checked.
    let checked = Document::parse(&format, Some(&candidate))?;
    if !checked.is_mapping() {
        return Err(AppError::new(
            "Structured configuration candidate root is not a mapping",
        ));
    }
    for (path, value) in leaves {
        let found = checked
            .lookup(path)
            .map_err(|_| AppError::new("Structured configuration candidate lost a managed value"))?
            .ok_or_else(|| AppError::new("Structured configuration candidate lost a managed value"))?;
        if !same_value(&found, &Plain::of_toml(value)) {
            return Err(AppError::new(
                "Structured configuration candidate does not match managed values",
            ));
        }
    }
    Ok((candidate, states))
}
