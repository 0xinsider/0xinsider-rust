//! Schema helpers and the naming of string enums.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
use serde_json::Value;

use crate::names::pascal;
use crate::py::{self, Obj};

pub type Result<T> = std::result::Result<T, String>;

/// Names for enum value sets whose most common property name is too generic to
/// stand alone, keyed by the set's first value. A key no set starts with is
/// reported and ignored.
const ENUM_NAME_OVERRIDES: &[(&str, &str)] = &[
    ("bad_request", "ErrorCode"),      // ApiError.error.code
    ("cursor_expired", "ErrorReason"), // ApiError.error.reason
    ("initialize", "McpMethod"),       // the MCP JSON-RPC request method
    ("provider", "TrustSourceKind"),   // TrustSource.kind
    ("learn", "ContentKind"),          // ContentSearchResult.kind
];

pub fn ref_name(reference: &str) -> Result<String> {
    reference
        .strip_prefix("#/components/schemas/")
        .map(str::to_string)
        .ok_or_else(|| format!("unsupported $ref outside components.schemas: {reference}"))
}

fn is_null_schema(schema: &Value) -> bool {
    schema.get("type") == Some(&Value::String("null".to_string()))
        || schema.get("enum") == Some(&Value::Array(vec![Value::Null]))
}

/// The schema without its null arm, and whether null is allowed.
pub fn split_nullable(schema: &Obj) -> Result<(Obj, bool)> {
    let mut nullable = py::truthy(schema.get("nullable"));
    let mut schema = schema.clone();
    schema.shift_remove("nullable");
    if let Some(Value::Array(types)) = schema.get("type").cloned() {
        let null = Value::String("null".to_string());
        if types.contains(&null) {
            nullable = true;
        }
        let rest: Vec<Value> = types.iter().filter(|kind| **kind != null).cloned().collect();
        if rest.len() != 1 {
            return Err(format!(
                "unsupported multi-type schema {}",
                py::repr(&Value::Array(types))
            ));
        }
        schema.insert("type".to_string(), rest[0].clone());
    }
    if let Some(Value::Array(values)) = schema.get("enum").cloned()
        && values.contains(&Value::Null)
    {
        nullable = true;
        let kept: Vec<Value> = values.into_iter().filter(|value| !value.is_null()).collect();
        schema.insert("enum".to_string(), Value::Array(kept));
    }
    for key in ["oneOf", "anyOf"] {
        let Some(Value::Array(arms)) = schema.get(key).cloned() else {
            continue;
        };
        if !arms.iter().any(is_null_schema) {
            continue;
        }
        nullable = true;
        let rest: Vec<Value> = arms.into_iter().filter(|arm| !is_null_schema(arm)).collect();
        if rest.len() == 1 {
            let mut merged: Obj = schema
                .iter()
                .filter(|(name, _)| name.as_str() != key)
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            let Some(arm) = rest[0].as_object() else {
                return Err(format!("the one non-null {key} arm is not an object"));
            };
            for (name, value) in arm {
                merged.insert(name.clone(), value.clone());
            }
            schema = merged;
        } else {
            schema.insert(key.to_string(), Value::Array(rest));
        }
    }
    Ok((schema, nullable))
}

/// The values of a string enum of two or more values; a single-value enum is a
/// constant and stays a String.
pub fn string_enum(schema: &Obj) -> Option<Vec<String>> {
    let values = schema.get("enum")?.as_array()?;
    let values: Vec<&Value> = values.iter().filter(|value| !value.is_null()).collect();
    if values.is_empty() || !values.iter().all(|value| value.is_string()) {
        return None;
    }
    if values.len() < 2 {
        return None;
    }
    Some(
        values
            .iter()
            .filter_map(|value| value.as_str())
            .map(str::to_string)
            .collect(),
    )
}

/// Names every string enum by its value set, so one set is one Rust type.
///
/// A set a component schema names keeps that name. Otherwise the set is named
/// after the property that most often carries it (`grade`, `side`); when two
/// sets want one name, the set used more often keeps it and the other is named
/// after its first owner plus the property.
pub struct EnumNames {
    by_values: HashMap<Vec<String>, String>,
    occurrences: IndexMap<Vec<String>, Vec<(String, String)>>,
}

impl EnumNames {
    pub fn new(components: &Obj) -> Result<Self> {
        let mut by_values = HashMap::new();
        for (name, schema) in components {
            let schema = schema
                .as_object()
                .ok_or_else(|| format!("component {name} is not an object"))?;
            if let Some(values) = string_enum(&split_nullable(schema)?.0) {
                by_values.insert(values, name.clone());
            }
        }
        Ok(Self {
            by_values,
            occurrences: IndexMap::new(),
        })
    }

    pub fn record(&mut self, values: Vec<String>, owner: &str, prop: &str) {
        if !self.by_values.contains_key(&values) {
            let prop = if prop.is_empty() { owner } else { prop };
            self.occurrences
                .entry(values)
                .or_default()
                .push((owner.to_string(), prop.to_string()));
        }
    }

    pub fn assign(&mut self, taken: &mut HashSet<String>) -> Result<()> {
        let mut wanted: IndexMap<String, Vec<(usize, Vec<String>)>> = IndexMap::new();
        for (key, occurrences) in &self.occurrences {
            // `min_grade` and `grade` carry one set; name it after the value, not the filter.
            let mut counts: IndexMap<String, usize> = IndexMap::new();
            for (_, prop) in occurrences {
                let prop = prop.strip_prefix("min_").unwrap_or(prop);
                *counts.entry(prop.to_string()).or_default() += 1;
            }
            let mut ranked: Vec<(&String, &usize)> = counts.iter().collect();
            ranked.sort_by(|left, right| right.1.cmp(left.1).then_with(|| left.0.cmp(right.0)));
            let best = ranked.first().map(|(prop, _)| prop.as_str()).unwrap_or_default();
            wanted
                .entry(pascal(best)?)
                .or_default()
                .push((occurrences.len(), key.clone()));
        }
        for key in self.occurrences.keys() {
            let first = key.first().map(String::as_str).unwrap_or_default();
            if let Some((_, name)) = ENUM_NAME_OVERRIDES.iter().find(|(value, _)| *value == first) {
                taken.insert(name.to_string());
                self.by_values.insert(key.clone(), name.to_string());
            }
        }
        let firsts: HashSet<&str> = self
            .occurrences
            .keys()
            .filter_map(|key| key.first().map(String::as_str))
            .collect();
        let mut unused: Vec<&str> = ENUM_NAME_OVERRIDES
            .iter()
            .map(|(value, _)| *value)
            .filter(|value| !firsts.contains(value))
            .collect();
        unused.sort();
        for first in unused {
            eprintln!(
                "warning: ENUM_NAME_OVERRIDES[{}] matches no enum; remove it",
                py::repr_str(first)
            );
        }
        for (name, claims) in wanted {
            let mut claims: Vec<(usize, Vec<String>)> = claims
                .into_iter()
                .filter(|(_, key)| !self.by_values.contains_key(key))
                .collect();
            claims.sort_by_key(|claim| std::cmp::Reverse(claim.0));
            for (index, (_, key)) in claims.into_iter().enumerate() {
                let mut candidate = name.clone();
                if index > 0 || taken.contains(&candidate) {
                    let (owner, prop) = self
                        .occurrences
                        .get(&key)
                        .and_then(|occurrences| occurrences.first())
                        .ok_or("an enum claim lost its occurrences")?;
                    candidate = pascal(owner)? + &pascal(prop)?;
                }
                let base = candidate.clone();
                let mut n = 2;
                while taken.contains(&candidate) {
                    candidate = format!("{base}{n}");
                    n += 1;
                }
                taken.insert(candidate.clone());
                self.by_values.insert(key, candidate);
            }
        }
        Ok(())
    }

    pub fn name(&self, values: &[String]) -> Result<String> {
        self.by_values
            .get(values)
            .cloned()
            .ok_or_else(|| format!("no enum name for {}", py::repr_list(values)))
    }

    pub fn has_name(&self, name: &str) -> bool {
        self.by_values.values().any(|taken| taken == name)
    }
}

/// Record each inline string enum under a schema with the property that carries it.
pub fn collect_enums(node: &Value, enums: &mut EnumNames, owner: &str, prop: &str) -> Result<()> {
    if let Value::Array(items) = node {
        for item in items {
            collect_enums(item, enums, owner, prop)?;
        }
        return Ok(());
    }
    let Some(node) = node.as_object() else {
        return Ok(());
    };
    if node.contains_key("$ref") {
        return Ok(());
    }
    if let Some(values) = string_enum(&split_nullable(node)?.0) {
        enums.record(values, owner, prop);
    }
    for (key, value) in node {
        match key.as_str() {
            "properties" => {
                if let Some(properties) = value.as_object() {
                    for (name, child) in properties {
                        let child_owner = if prop.is_empty() {
                            owner.to_string()
                        } else {
                            format!("{owner}{}", pascal(prop)?)
                        };
                        collect_enums(child, enums, &child_owner, name)?;
                    }
                }
            }
            "items" | "additionalProperties" | "oneOf" | "anyOf" | "allOf" => collect_enums(value, enums, owner, prop)?,
            _ => {}
        }
    }
    Ok(())
}
