//! JSON Schema of the KST rules file (`konform rule --schema`), for editor
//! completion and validation (e.g. taplo, YAML language server).
//!
//! Built in code so the `kind` enum can't drift from [`Kind::NAMES`]; a test
//! keeps the matcher properties in sync with [`RawMatcher`].

use super::node::Kind;
use serde_json::{json, Value};

/// The schema of a rules file: a table with a `rules` array.
pub fn rules_schema() -> Value {
    let kinds: Vec<&str> = Kind::NAMES.iter().map(|(n, _)| *n).collect();
    let names = json!([
        { "type": "string" },
        { "type": "array", "items": { "type": "string" }, "minItems": 1 },
    ]);
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "konform structural rules (KST)",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "rules": { "type": "array", "items": { "$ref": "#/$defs/rule" } }
        },
        "$defs": {
            "rule": {
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "message", "match"],
                "properties": {
                    "id": {
                        "type": "string",
                        "pattern": "^KST",
                        "description": "Rule id; must start with KST."
                    },
                    "message": { "type": "string" },
                    "help": { "type": "string" },
                    "level": { "enum": ["error", "warning"] },
                    "files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Glob patterns; the rule only runs on matching files."
                    },
                    "match": { "$ref": "#/$defs/matcher" },
                    "test": {
                        "type": "object",
                        "additionalProperties": false,
                        "description": "Self-tests run by `konform rule --test`.",
                        "properties": {
                            "valid": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Snippets the rule must not flag."
                            },
                            "invalid": {
                                "type": "array",
                                "items": { "type": "string" },
                                "description": "Snippets the rule must flag."
                            }
                        }
                    }
                }
            },
            "matcher": {
                "type": "object",
                "additionalProperties": false,
                "minProperties": 1,
                "description": "Every condition present must hold.",
                "properties": {
                    "kind": {
                        "description": "Node kind, or a list of kinds (any of).",
                        "oneOf": [
                            { "enum": kinds },
                            { "type": "array", "items": { "enum": kinds }, "minItems": 1 },
                        ]
                    },
                    "name": {
                        "type": "string",
                        "description": "Regex searched in the node's identifier."
                    },
                    "qualname": {
                        "description": "Import-resolved dotted name of a name/attribute node.",
                        "oneOf": names
                    },
                    "callee": {
                        "description": "Import-resolved dotted name of a call's function.",
                        "oneOf": names
                    },
                    "decorated_with": {
                        "description": "Import-resolved decorator name(s) of a function or class.",
                        "oneOf": names
                    },
                    "inside": { "$ref": "#/$defs/matcher", "description": "Some ancestor matches." },
                    "has": { "$ref": "#/$defs/matcher", "description": "Some descendant matches." },
                    "not": { "$ref": "#/$defs/matcher", "description": "The node does not match." },
                    "all": {
                        "type": "array",
                        "items": { "$ref": "#/$defs/matcher" },
                        "minItems": 1,
                        "description": "Every listed matcher matches."
                    },
                    "any": {
                        "type": "array",
                        "items": { "$ref": "#/$defs/matcher" },
                        "minItems": 1,
                        "description": "At least one listed matcher matches."
                    },
                    "stop_by": {
                        "$ref": "#/$defs/matcher",
                        "description": "Only inside `inside` / `has`: stop the search at the first node matching this (inclusive)."
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::matcher::RawMatcher;
    use super::*;
    use crate::rules::Rule;

    fn matcher_props(schema: &Value) -> serde_json::Map<String, Value> {
        schema["$defs"]["matcher"]["properties"]
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn matcher_properties_match_the_rust_type_exactly() {
        let props = matcher_props(&rules_schema());
        // Every schema property must be accepted by the strict deserialiser...
        let sample = json!({
            "kind": "assert", "name": "x", "qualname": "a.b", "callee": ["a.b"],
            "decorated_with": "a", "inside": {"kind": "class"}, "has": {"kind": "class"},
            "not": {"kind": "class"}, "all": [{"kind": "class"}], "any": [{"kind": "class"}],
            "stop_by": {"kind": "class"},
        });
        let mut keys: Vec<&String> = props.keys().collect();
        keys.sort();
        let mut sample_keys: Vec<&String> = sample.as_object().unwrap().keys().collect();
        sample_keys.sort();
        assert_eq!(keys, sample_keys, "sample must cover the schema");
        let parsed: RawMatcher =
            serde_json::from_value(sample).expect("schema key unknown to RawMatcher");
        // ...and every field of the Rust type must be in the schema.
        let mut round: Vec<String> = serde_json::to_value(parsed)
            .unwrap()
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        round.sort();
        let mut expected: Vec<String> = props.keys().cloned().collect();
        expected.sort();
        assert_eq!(round, expected, "RawMatcher field missing from the schema");
    }

    #[test]
    fn kind_enum_comes_from_the_vocabulary() {
        let schema = rules_schema();
        let kinds = &matcher_props(&schema)["kind"]["oneOf"][0]["enum"];
        let expected: Vec<&str> = Kind::NAMES.iter().map(|(n, _)| *n).collect();
        assert_eq!(kinds, &json!(expected));
    }

    #[test]
    fn rule_properties_match_the_rule_file_format() {
        let schema = rules_schema();
        let props = schema["$defs"]["rule"]["properties"].as_object().unwrap();
        let toml = r#"
[[rules]]
id = "KST001"
message = "m"
help = "h"
level = "error"
files = ["a"]
match = { kind = "assert" }
[rules.test]
valid = []
invalid = []
"#;
        let value: toml::Value = toml::from_str(toml).unwrap();
        let rule = value["rules"][0].as_table().unwrap();
        let mut a: Vec<&String> = props.keys().collect();
        let mut b: Vec<&String> = rule.keys().collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
        assert!(super::super::KstRule::new(None)
            .config_errors(&value)
            .is_empty());
    }
}
