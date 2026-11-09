//! A small subset of JSON Schema, used both to constrain model output (llama.cpp turns it into a
//! sampling grammar) and to validate what actually came back. Grammar sampling is not trusted on
//! its own: every structured answer and every tool argument is validated here.

use serde_json::{Map, Value, json};

#[derive(Clone, Debug, PartialEq)]
pub enum Schema {
    String {
        max_length: Option<usize>,
        one_of: Option<Vec<String>>,
    },
    Integer {
        min: Option<i64>,
        max: Option<i64>,
    },
    Number,
    Boolean,
    Array {
        items: Box<Schema>,
        max_items: Option<usize>,
    },
    /// Closed object: unknown keys are rejected.
    Object(Vec<Field>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub name: String,
    pub description: Option<String>,
    pub schema: Schema,
    pub required: bool,
}

impl Field {
    pub fn required(name: &str, schema: Schema) -> Self {
        Self {
            name: name.into(),
            description: None,
            schema,
            required: true,
        }
    }
    pub fn optional(name: &str, schema: Schema) -> Self {
        Self {
            required: false,
            ..Self::required(name, schema)
        }
    }
    #[must_use]
    pub fn describe(mut self, text: &str) -> Self {
        self.description = Some(text.into());
        self
    }
}

/// Where in a value a problem was found, and what it is.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{path}: {problem}")]
pub struct SchemaError {
    pub path: String,
    pub problem: String,
}

impl Schema {
    pub fn string() -> Self {
        Self::String {
            max_length: None,
            one_of: None,
        }
    }
    pub fn bounded_string(max_length: usize) -> Self {
        Self::String {
            max_length: Some(max_length),
            one_of: None,
        }
    }
    pub fn one_of(values: &[&str]) -> Self {
        Self::String {
            max_length: None,
            one_of: Some(values.iter().map(|v| (*v).to_owned()).collect()),
        }
    }
    pub fn integer(min: Option<i64>, max: Option<i64>) -> Self {
        Self::Integer { min, max }
    }
    pub fn array(items: Schema, max_items: Option<usize>) -> Self {
        Self::Array {
            items: Box::new(items),
            max_items,
        }
    }
    pub fn object(fields: Vec<Field>) -> Self {
        Self::Object(fields)
    }

    pub fn to_json(&self) -> Value {
        match self {
            Self::String { max_length, one_of } => {
                let mut v = json!({"type": "string"});
                if let Some(n) = max_length {
                    v["maxLength"] = json!(n);
                }
                if let Some(values) = one_of {
                    v["enum"] = json!(values);
                }
                v
            }
            Self::Integer { min, max } => {
                let mut v = json!({"type": "integer"});
                if let Some(n) = min {
                    v["minimum"] = json!(n);
                }
                if let Some(n) = max {
                    v["maximum"] = json!(n);
                }
                v
            }
            Self::Number => json!({"type": "number"}),
            Self::Boolean => json!({"type": "boolean"}),
            Self::Array { items, max_items } => {
                let mut v = json!({"type": "array", "items": items.to_json()});
                if let Some(n) = max_items {
                    v["maxItems"] = json!(n);
                }
                v
            }
            Self::Object(fields) => {
                let mut props = Map::new();
                for f in fields {
                    let mut s = f.schema.to_json();
                    if let Some(d) = &f.description {
                        s["description"] = json!(d);
                    }
                    props.insert(f.name.clone(), s);
                }
                let required: Vec<&str> = fields
                    .iter()
                    .filter(|f| f.required)
                    .map(|f| f.name.as_str())
                    .collect();
                json!({
                    "type": "object",
                    "properties": props,
                    "required": required,
                    "additionalProperties": false,
                })
            }
        }
    }

    pub fn validate(&self, value: &Value) -> Result<(), SchemaError> {
        self.check(value, "$")
    }

    fn check(&self, value: &Value, path: &str) -> Result<(), SchemaError> {
        let fail = |problem: String| {
            Err(SchemaError {
                path: path.to_owned(),
                problem,
            })
        };
        match self {
            Self::String { max_length, one_of } => {
                let Some(s) = value.as_str() else {
                    return fail(format!("expected a string, found {}", kind(value)));
                };
                if let Some(n) = max_length
                    && s.chars().count() > *n
                {
                    return fail(format!("longer than {n} characters"));
                }
                if let Some(values) = one_of
                    && !values.iter().any(|v| v == s)
                {
                    return fail(format!("{s:?} is not one of {}", values.join(", ")));
                }
                Ok(())
            }
            Self::Integer { min, max } => {
                let Some(n) = value.as_i64() else {
                    return fail(format!("expected an integer, found {}", kind(value)));
                };
                if min.is_some_and(|m| n < m) || max.is_some_and(|m| n > m) {
                    return fail(format!("{n} is out of range"));
                }
                Ok(())
            }
            Self::Number if value.is_number() => Ok(()),
            Self::Number => fail(format!("expected a number, found {}", kind(value))),
            Self::Boolean if value.is_boolean() => Ok(()),
            Self::Boolean => fail(format!("expected true or false, found {}", kind(value))),
            Self::Array { items, max_items } => {
                let Some(a) = value.as_array() else {
                    return fail(format!("expected an array, found {}", kind(value)));
                };
                if let Some(n) = max_items
                    && a.len() > *n
                {
                    return fail(format!("more than {n} items"));
                }
                for (i, v) in a.iter().enumerate() {
                    items.check(v, &format!("{path}[{i}]"))?;
                }
                Ok(())
            }
            Self::Object(fields) => {
                let Some(o) = value.as_object() else {
                    return fail(format!("expected an object, found {}", kind(value)));
                };
                for key in o.keys() {
                    if !fields.iter().any(|f| &f.name == key) {
                        return fail(format!("unexpected key {key:?}"));
                    }
                }
                for f in fields {
                    match o.get(&f.name) {
                        Some(v) => f.schema.check(v, &format!("{path}.{}", f.name))?,
                        None if f.required => return fail(format!("missing key {:?}", f.name)),
                        None => {}
                    }
                }
                Ok(())
            }
        }
    }
}

fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// llama.cpp writes properties in the schema's order, so the order the fields are declared in
    /// (for example a name before the value it names) must survive.
    #[test]
    fn properties_keep_the_declared_order() {
        let s = Schema::object(vec![
            Field::required("name", Schema::string()),
            Field::required("input", Schema::string()),
        ]);
        let keys: Vec<String> = s.to_json()["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, ["name", "input"]);
    }

    fn answer() -> Schema {
        Schema::object(vec![
            Field::required("code", Schema::bounded_string(20)),
            Field::optional(
                "requires",
                Schema::array(Schema::one_of(&["clojure.string"]), Some(2)),
            ),
            Field::required("confidence", Schema::integer(Some(0), Some(10))),
        ])
    }

    #[test]
    fn accepts_conforming_values() {
        answer()
            .validate(&json!({"code": "(inc x)", "confidence": 3}))
            .unwrap();
        answer()
            .validate(&json!({"code": "", "requires": ["clojure.string"], "confidence": 0}))
            .unwrap();
    }

    #[test]
    fn reports_the_first_violation_with_its_path() {
        let cases = [
            (json!([]), "$", "expected an object"),
            (json!({"confidence": 1}), "$", "missing key \"code\""),
            (
                json!({"code": "x", "confidence": 1, "extra": 1}),
                "$",
                "unexpected key",
            ),
            (json!({"code": 5, "confidence": 1}), "$.code", "expected a string"),
            (
                json!({"code": "x".repeat(21), "confidence": 1}),
                "$.code",
                "longer than 20",
            ),
            (
                json!({"code": "x", "confidence": 11}),
                "$.confidence",
                "out of range",
            ),
            (
                json!({"code": "x", "confidence": 1.5}),
                "$.confidence",
                "expected an integer",
            ),
            (
                json!({"code": "x", "confidence": 1, "requires": ["java.io"]}),
                "$.requires[0]",
                "not one of",
            ),
        ];
        for (value, path, problem) in cases {
            let e = answer().validate(&value).unwrap_err();
            assert_eq!(e.path, path, "{value}");
            assert!(e.problem.contains(problem), "{value}: {}", e.problem);
        }
    }

    #[test]
    fn json_form_is_closed_and_lists_required_keys() {
        let j = answer().to_json();
        assert_eq!(j["additionalProperties"], false);
        assert_eq!(j["required"], json!(["code", "confidence"]));
        assert_eq!(j["properties"]["requires"]["maxItems"], 2);
    }
}
