//! Three immutable source files, typed application inputs and data-only delivery.
use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_json::{Value, json};
use yaml_rust2::{Yaml, YamlLoader};

use super::manifest::{Checked, MAX_BYTES};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contract {
    pub inputs: Vec<Input>,
    pub always_numbered: bool,
    pub configuration: Option<Configuration>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    #[serde(default)]
    pub required: bool,
    pub default: Option<Value>,
    pub min: Option<serde_json::Number>,
    pub max: Option<serde_json::Number>,
    pub min_length: Option<usize>,
    pub max_length: Option<usize>,
    pub regex: Option<String>,
    #[serde(default)]
    pub choices: Vec<String>,
    #[serde(default)]
    pub allowed_schemes: Vec<String>,
    pub target: Target,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    String,
    Integer,
    Number,
    Boolean,
    Url,
    #[serde(alias = "select")]
    Enum,
    #[serde(alias = "password")]
    Secret,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Target {
    Environment { variable: String },
    ConfigJsonPointer { pointer: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub delivery: Delivery,
    pub document: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    #[serde(rename = "type")]
    pub kind: String,
    pub variable: String,
}

fn identifier(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 64
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
        && text.starts_with(|c: char| c.is_ascii_lowercase())
}

pub fn environment_name(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn keys(value: &Value, path: &str, allowed: &[&str]) -> Result<(), String> {
    let map = value.as_object().ok_or_else(|| format!("{path}: expected a mapping"))?;
    for name in map.keys() {
        if !allowed.contains(&name.as_str()) {
            return Err(format!("{path}.{name}: unknown field"));
        }
    }
    Ok(())
}

pub fn yaml_value(text: &str) -> Result<Value, String> {
    fn convert(yaml: Yaml) -> Result<Value, String> {
        Ok(match yaml {
            Yaml::String(v) => Value::String(v),
            Yaml::Integer(v) => v.into(),
            Yaml::Real(v) => serde_json::from_str(&v).map_err(|_| "non-finite YAML number")?,
            Yaml::Boolean(v) => v.into(),
            Yaml::Null => Value::Null,
            Yaml::Array(v) => Value::Array(v.into_iter().map(convert).collect::<Result<_, _>>()?),
            Yaml::Hash(v) => {
                let mut map = serde_json::Map::new();
                for (key, value) in v {
                    let key = key.as_str().ok_or("field names must be strings")?.to_owned();
                    map.insert(key, convert(value)?);
                }
                Value::Object(map)
            }
            _ => return Err("unsupported YAML value".into()),
        })
    }
    if text.len() as u64 > MAX_BYTES {
        return Err(format!("larger than {MAX_BYTES} bytes"));
    }
    let mut documents = YamlLoader::load_from_str(text).map_err(|e| format!("invalid YAML: {e}"))?;
    if documents.len() != 1 {
        return Err("expected exactly one YAML document".into());
    }
    convert(documents.remove(0))
}

fn decode<T: serde::de::DeserializeOwned>(value: Value, path: &str) -> Result<T, String> {
    serde_path_to_error::deserialize(value).map_err(|e| format!("{path}.{e}"))
}

/// Reuse the schema-1 resource, network, persistence and health rules without
/// changing that parser or relaxing the three-file schema's field checks.
pub fn parse(service: &str, defaults: &str, inputs: &str, directory: &str) -> Result<Checked, String> {
    let service = yaml_value(service)?;
    let mut defaults = yaml_value(defaults)?;
    let inputs = yaml_value(inputs)?;
    keys(
        &service,
        "service",
        &["schema", "id", "name", "image", "runtime", "persistence", "exposure"],
    )?;
    keys(&defaults, "defaults", &["schema", "service", "resources", "deployment"])?;
    keys(&inputs, "inputs", &["schema", "service", "inputs"])?;
    for (name, doc, id_key) in [
        ("service", &service, "id"),
        ("defaults", &defaults, "service"),
        ("inputs", &inputs, "service"),
    ] {
        if doc["schema"] != json!(3) {
            return Err(format!("{name}.schema: must be integer 3"));
        }
        if doc[id_key].as_str() != Some(directory) {
            return Err(format!("{name}.{id_key}: must equal directory ID {directory}"));
        }
    }
    for name in ["name", "image", "runtime", "persistence", "exposure"] {
        if service.get(name).is_none() {
            return Err(format!("service.{name}: required"));
        }
    }
    keys(&service["image"], "service.image", &["repository"])?;
    keys(
        &service["runtime"],
        "service.runtime",
        &["ports", "healthcheck", "configuration"],
    )?;
    if let Some(fields) = inputs["inputs"].as_array() {
        for (index, input) in fields.iter().enumerate() {
            for name in ["default", "min", "max", "min_length", "max_length", "regex"] {
                if input.get(name).is_some_and(Value::is_null) {
                    return Err(format!("inputs.inputs[{index}].{name}: null is not a typed value"));
                }
            }
        }
    }
    let mut contract = Contract {
        inputs: decode(
            inputs.get("inputs").cloned().ok_or("inputs.inputs: required")?,
            "inputs.inputs",
        )?,
        always_numbered: false,
        configuration: service["runtime"]
            .get("configuration")
            .cloned()
            .map(|v| decode(v, "service.runtime.configuration"))
            .transpose()?,
    };
    let deployment = defaults.get_mut("deployment").ok_or("defaults.deployment: required")?;
    let naming = deployment
        .as_object_mut()
        .ok_or("defaults.deployment: expected a mapping")?
        .remove("instance_naming");
    if let Some(naming) = naming {
        match naming.as_str() {
            Some("always-numbered") => contract.always_numbered = true,
            Some("automatic") => {}
            _ => return Err("defaults.deployment.instance_naming: expected always-numbered or automatic".into()),
        }
    }
    contract.validate()?;
    let mut runtime = service["runtime"].clone();
    let runtime_map = runtime.as_object_mut().ok_or("service.runtime: expected a mapping")?;
    runtime_map.remove("configuration");
    runtime_map.insert(
        "resources".into(),
        defaults
            .get("resources")
            .cloned()
            .ok_or("defaults.resources: required")?,
    );
    let empty = |v: &Value| {
        if v.as_object().is_some_and(serde_json::Map::is_empty) {
            Value::Null
        } else {
            v.clone()
        }
    };
    let legacy = json!({"schema":1,"id":service["id"],"name":service["name"],"image":service["image"],
        "runtime":runtime,"persistence":empty(&service["persistence"]),"exposure":empty(&service["exposure"]),"deployment":defaults["deployment"]});
    let mut checked = super::manifest::parse(&legacy.to_string())?;
    if !checked.issues.is_empty() {
        return Err(checked
            .issues
            .iter()
            .map(|issue| {
                let mut path = issue.path.clone();
                if path.starts_with(&["runtime".into(), "resources".into()]) {
                    path[0] = "defaults".into();
                } else if path.first().is_some_and(|p| p == "deployment") {
                    path.insert(0, "defaults".into());
                } else {
                    path.insert(0, "service".into());
                }
                format!("{}: {}", path.join("."), issue.message)
            })
            .collect::<Vec<_>>()
            .join("; "));
    }

    let mut displayed = legacy;
    displayed["schema"] = json!(3);
    displayed["deployment"] = defaults["deployment"].clone();
    if contract.always_numbered {
        displayed["deployment"]["instance_naming"] = json!("always-numbered");
    }
    if let Some(configuration) = service["runtime"].get("configuration") {
        displayed["runtime"]["configuration"] = configuration.clone();
    }
    displayed["inputs"] = inputs["inputs"].clone();
    checked.document = YamlLoader::load_from_str(&displayed.to_string())
        .map_err(|_| "Cannot display schema 3 contract")?
        .remove(0);
    if let Some(manifest) = &mut checked.manifest {
        manifest.schema3 = Some(contract);
    }
    Ok(checked)
}

impl Input {
    pub fn title(&self) -> String {
        format!("Application configuration · {} ({})", self.label, self.id)
    }

    /// Parse text once, retaining JSON numbers and booleans in private storage.
    pub fn value(&self, text: &str) -> Result<Option<Value>, String> {
        if self.kind == Kind::Secret {
            return Err(format!(
                "{}: protected secret delivery is not available; plain PVE environment is refused",
                self.id
            ));
        }
        if text.is_empty() {
            if let Some(value) = &self.default {
                self.validate_value(value)?;
                return Ok(Some(value.clone()));
            }
            if self.required {
                return Err(format!("{}: required", self.id));
            }
            return Ok(None);
        }
        let value = match self.kind {
            Kind::Integer | Kind::Number | Kind::Boolean => {
                serde_json::from_str(text).map_err(|_| format!("{}: invalid typed value", self.id))?
            }
            _ => Value::String(text.to_owned()),
        };
        self.validate_value(&value)?;
        Ok(Some(value))
    }

    pub fn validate_value(&self, value: &Value) -> Result<(), String> {
        let fail = |message: &str| Err(format!("{}: {message}", self.id));
        match self.kind {
            Kind::Integer if !(value.is_i64() || value.is_u64()) => return fail("expected an integer"),
            Kind::Number if !value.is_number() => return fail("expected a finite number"),
            Kind::Boolean if !value.is_boolean() => return fail("expected true or false"),
            Kind::String | Kind::Url | Kind::Enum | Kind::Secret if !value.is_string() => {
                return fail("expected a string");
            }
            _ => {}
        }
        let integer = |number: &serde_json::Number| {
            number
                .as_i64()
                .map(i128::from)
                .or_else(|| number.as_u64().map(i128::from))
        };
        if self.kind == Kind::Integer {
            let number = value
                .as_i64()
                .map(i128::from)
                .or_else(|| value.as_u64().map(i128::from))
                .ok_or_else(|| format!("{}: expected an integer", self.id))?;
            if self.min.as_ref().and_then(integer).is_some_and(|min| number < min)
                || self.max.as_ref().and_then(integer).is_some_and(|max| number > max)
            {
                return fail("outside the declared min/max bounds");
            }
        } else if let Some(number) = value.as_f64()
            && (self
                .min
                .as_ref()
                .and_then(serde_json::Number::as_f64)
                .is_some_and(|min| number < min)
                || self
                    .max
                    .as_ref()
                    .and_then(serde_json::Number::as_f64)
                    .is_some_and(|max| number > max))
        {
            return fail("outside the declared min/max bounds");
        }
        if let Some(text) = value.as_str() {
            if text.chars().any(char::is_control) {
                return fail("control characters are forbidden");
            }
            let len = text.chars().count();
            if (self.required && text.is_empty())
                || self.min_length.is_some_and(|min| len < min)
                || self.max_length.is_some_and(|max| len > max)
            {
                return fail("outside the declared length bounds");
            }
            if self.kind == Kind::Enum && !self.choices.iter().any(|v| v == text) {
                return fail("choose a declared value");
            }
            if let Some(pattern) = &self.regex {
                let regex = regex::Regex::new(pattern).map_err(|_| format!("{}: invalid regex", self.id))?;
                if !regex.is_match(text) {
                    // The pattern itself means nothing to an operator; the
                    // field's description says what is expected.
                    return if self.description.is_empty() {
                        fail("does not match the declared format")
                    } else {
                        fail(&format!("does not match the expected format. {}", self.description))
                    };
                }
            }
            if self.kind == Kind::Url {
                let url = url::Url::parse(text).map_err(|_| format!("{}: expected an absolute URL", self.id))?;
                if url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.fragment().is_some()
                    || !self.allowed_schemes.iter().any(|scheme| scheme == url.scheme())
                {
                    return fail("URL requires an allowed scheme and host, without credentials or fragment");
                }
            }
        }
        Ok(())
    }
}

impl Contract {
    fn validate(&self) -> Result<(), String> {
        if self.inputs.len() > 64 {
            return Err("inputs.inputs: at most 64 fields".into());
        }
        let mut ids = BTreeSet::new();
        let mut targets = BTreeSet::new();
        if let Some(config) = &self.configuration {
            if config.delivery.kind != "environment-json" || !environment_name(&config.delivery.variable) {
                return Err("runtime.configuration.delivery: expected environment-json and a valid variable".into());
            }
            targets.insert(format!("env:{}", config.delivery.variable));
        }
        for input in &self.inputs {
            let fail = |msg| Err(format!("inputs.{}: {msg}", input.id));
            if !identifier(&input.id) || !ids.insert(&input.id) {
                return fail("invalid or duplicate ID");
            }
            if input.label.is_empty()
                || input.label.chars().any(char::is_control)
                || input.description.chars().any(char::is_control)
            {
                return fail("label/description must be printable and label nonempty");
            }
            if input.kind == Kind::Integer
                && [&input.min, &input.max]
                    .into_iter()
                    .flatten()
                    .any(|n| !(n.is_i64() || n.is_u64()))
            {
                return fail("integer bounds must be integers");
            }
            if input
                .min
                .as_ref()
                .and_then(serde_json::Number::as_f64)
                .zip(input.max.as_ref().and_then(serde_json::Number::as_f64))
                .is_some_and(|(a, b)| a > b)
                || input.min_length.zip(input.max_length).is_some_and(|(a, b)| a > b)
            {
                return fail("inverted bounds");
            }
            if (input.min.is_some() || input.max.is_some()) && !matches!(input.kind, Kind::Integer | Kind::Number) {
                return fail("numeric bounds require integer or number");
            }
            if (input.regex.is_some() || input.min_length.is_some() || input.max_length.is_some())
                && matches!(input.kind, Kind::Integer | Kind::Number | Kind::Boolean)
            {
                return fail("text constraints require a text field");
            }
            if input.kind == Kind::Enum
                && (input.choices.is_empty()
                    || input.choices.iter().collect::<BTreeSet<_>>().len() != input.choices.len())
            {
                return fail("enum choices must be nonempty and unique");
            }
            if input.kind != Kind::Enum && !input.choices.is_empty() {
                return fail("choices require enum");
            }
            if input.kind == Kind::Url
                && (input.allowed_schemes.is_empty()
                    || input
                        .allowed_schemes
                        .iter()
                        .any(|scheme| !["http", "https"].contains(&scheme.as_str())))
            {
                return fail("URL schemes must explicitly select http and/or https");
            }
            if input.kind != Kind::Url && !input.allowed_schemes.is_empty() {
                return fail("allowed_schemes requires url");
            }
            if input.regex.as_ref().is_some_and(|r| regex::Regex::new(r).is_err()) {
                return fail("invalid regex");
            }
            if let Some(value) = &input.default {
                if input.kind == Kind::Secret {
                    return fail("secret defaults are forbidden");
                }
                input.validate_value(value)?;
            }
            let target = match &input.target {
                Target::Environment { variable } => {
                    if !environment_name(variable)
                        || (super::deploy::secret_name(&variable.to_ascii_uppercase()) && input.kind != Kind::Secret)
                    {
                        return fail("invalid environment variable");
                    }
                    format!("env:{variable}")
                }
                Target::ConfigJsonPointer { pointer } => {
                    if !pointer.starts_with('/') || pointer.split('~').skip(1).any(|s| !s.starts_with(['0', '1'])) {
                        return fail("invalid RFC 6901 pointer");
                    }
                    let leaf = self
                        .configuration
                        .as_ref()
                        .and_then(|c| c.document.pointer(pointer))
                        .ok_or_else(|| {
                            format!(
                                "inputs.{}: target must resolve to an existing configuration leaf",
                                input.id
                            )
                        })?;
                    if leaf.is_object() || leaf.is_array() {
                        return fail("target must be a leaf");
                    }
                    let canonical: String = pointer
                        .split('/')
                        .skip(1)
                        .map(|part| part.replace("~1", "/").replace("~0", "~"))
                        .map(|part| format!("/{}", part.replace('~', "~0").replace('/', "~1")))
                        .collect();
                    format!("config:{canonical}")
                }
            };
            if !targets.insert(target) {
                return fail("duplicate target");
            }
        }
        Ok(())
    }

    pub fn values(&self, texts: &BTreeMap<String, String>) -> Result<BTreeMap<String, Value>, String> {
        if texts.keys().any(|id| !self.inputs.iter().any(|input| &input.id == id)) {
            return Err("Unknown application input ID".into());
        }
        let mut values = BTreeMap::new();
        for input in &self.inputs {
            if let Some(value) = input.value(texts.get(&input.id).map_or("", String::as_str))? {
                values.insert(input.id.clone(), value);
            }
        }
        self.resolve(&values)?;
        Ok(values)
    }

    pub fn owns_environment(&self, key: &str) -> bool {
        self.inputs
            .iter()
            .any(|input| matches!(&input.target, Target::Environment { variable } if variable == key))
            || self.configuration.as_ref().is_some_and(|c| c.delivery.variable == key)
    }

    pub fn resolve(&self, values: &BTreeMap<String, Value>) -> Result<Vec<(String, String)>, String> {
        self.validate()?;
        if let Some(id) = values
            .keys()
            .find(|id| !self.inputs.iter().any(|input| &input.id == *id))
        {
            return Err(format!(
                "Application configuration has {id}, which the manifest no longer declares; set it again under Application configuration"
            ));
        }
        let mut environment = Vec::new();
        let mut document = self.configuration.as_ref().map(|c| c.document.clone());
        for input in &self.inputs {
            if input.kind == Kind::Secret {
                return Err(format!("{}: protected secret delivery is unavailable", input.id));
            }
            let value = values.get(&input.id).or(input.default.as_ref());
            let Some(value) = value else {
                if input.required {
                    return Err(format!("{}: required", input.id));
                }
                continue;
            };
            input.validate_value(value)?;
            match &input.target {
                Target::Environment { variable } => environment.push((variable.clone(), display(value))),
                Target::ConfigJsonPointer { pointer } => {
                    let leaf = document
                        .as_mut()
                        .and_then(|doc| doc.pointer_mut(pointer))
                        .ok_or("Configuration target disappeared")?;
                    *leaf = value.clone();
                }
            }
        }
        if let (Some(document), Some(configuration)) = (document, self.configuration.as_ref()) {
            fn unresolved(v: &Value) -> bool {
                match v {
                    Value::Null => true,
                    Value::Array(a) => a.iter().any(unresolved),
                    Value::Object(o) => o.values().any(unresolved),
                    _ => false,
                }
            }
            if unresolved(&document) {
                return Err("runtime.configuration.document: unresolved null leaf".into());
            }
            environment.push((configuration.delivery.variable.clone(), document.to_string()));
        }
        validate_environment(&environment)?;
        Ok(environment)
    }
}

pub fn display(value: &Value) -> String {
    value.as_str().map_or_else(|| value.to_string(), str::to_owned)
}

pub fn validate_environment(values: &[(String, String)]) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for (key, value) in values {
        if !environment_name(key) || !seen.insert(key) || value.chars().any(char::is_control) {
            return Err("Runtime environment contains invalid names, duplicate entries or control characters".into());
        }
    }
    if values.iter().map(|(k, v)| k.len() + v.len() + 2).sum::<usize>() > 64 * 1024 {
        return Err("Runtime environment exceeds 64 KiB".into());
    }
    Ok(())
}
