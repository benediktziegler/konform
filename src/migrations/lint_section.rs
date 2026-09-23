//! Migration: flat/category config → `[tool.konform.lint]` + per-rule tables.
//!
//! Upgrades the pre-0.3 config shape:
//!
//! ```toml
//! [tool.konform]
//! select = ["KIS"]
//! cache_dir = ".konform_cache"
//!
//! [tool.konform.KIS]
//! unresolved_level = "warning"
//!
//! [tool.konform.KPT]
//! level = "warning"
//! ```
//!
//! into the current shape:
//!
//! ```toml
//! [tool.konform]
//! cache-dir = ".konform_cache"
//!
//! [tool.konform.lint]
//! select = ["KIS"]
//!
//! [tool.konform.lint.module-only-imports]
//! unresolved-level = "warning"
//!
//! [tool.konform.lint.user-defined-patterns]
//! level = "warning"
//! ```

use super::ConfigMigration;
use toml_edit::{Item, Table, TableLike};

/// Maps a rule's old category prefix to its new config-table name
/// ([`crate::rules::Rule::config_name`]). Extend this when a future rule
/// category also needs to move from `[tool.konform.<CATEGORY>]` into
/// `[tool.konform.lint.<config-name>]`.
const CATEGORY_TO_RULE_CONFIG_NAME: &[(&str, &str)] = &[
    ("KIS", "module-only-imports"),
    ("KPT", "user-defined-patterns"),
];

/// Flat `[tool.konform]` keys that move under `[tool.konform.lint]`,
/// paired with their new kebab-case name.
const FLAT_KEY_RENAMES: &[(&str, &str)] = &[
    ("select", "select"),
    ("ignore", "ignore"),
    ("level", "level"),
    ("per_file_ignores", "per-file-ignores"),
    ("noqa_aliases", "noqa-aliases"),
];

pub struct LintSectionMigration;

impl ConfigMigration for LintSectionMigration {
    fn id(&self) -> &str {
        "lint-section"
    }

    fn description(&self) -> &str {
        "Move rule selection/level settings and per-category config into [tool.konform.lint]"
    }

    fn detect(&self, table: &dyn TableLike) -> bool {
        if table.contains_key("lint") {
            return false; // already migrated
        }
        table.contains_key("cache_dir")
            || FLAT_KEY_RENAMES
                .iter()
                .any(|(old, _)| table.contains_key(old))
            || CATEGORY_TO_RULE_CONFIG_NAME
                .iter()
                .any(|(category, _)| table.contains_key(category))
    }

    fn apply(&self, table: &mut dyn TableLike) -> Vec<String> {
        let mut notes = Vec::new();

        if let Some(item) = table.remove("cache_dir") {
            table.insert("cache-dir", item);
            notes.push("renamed `cache_dir` to `cache-dir`".to_owned());
        }

        let mut lint = Table::new();
        lint.set_implicit(false);

        for (old_key, new_key) in FLAT_KEY_RENAMES {
            if let Some(item) = table.remove(old_key) {
                lint.insert(new_key, item);
                notes.push(format!("moved `{old_key}` to `lint.{new_key}`"));
            }
        }

        for (category, config_name) in CATEGORY_TO_RULE_CONFIG_NAME {
            let Some(mut item) = table.remove(category) else {
                continue;
            };
            if *category == "KIS" {
                if let Some(kis_table) = item.as_table_like_mut() {
                    if let Some(v) = kis_table.remove("unresolved_level") {
                        kis_table.insert("unresolved-level", v);
                        notes.push(
                            "renamed `KIS.unresolved_level` to `unresolved-level`".to_owned(),
                        );
                    }
                }
            }
            lint.insert(config_name, item);
            notes.push(format!(
                "moved `[tool.konform.{category}]` to `lint.{config_name}`"
            ));
        }

        if !lint.is_empty() {
            table.insert("lint", Item::Table(lint));
        }

        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use toml_edit::DocumentMut;

    fn parse(toml: &str) -> DocumentMut {
        toml.parse().expect("valid toml")
    }

    #[test]
    fn detect_false_when_lint_already_present() {
        let doc = parse("[lint]\nselect = []\n");
        assert!(!LintSectionMigration.detect(doc.as_table()));
    }

    #[test]
    fn detect_false_when_nothing_to_migrate() {
        let doc = parse("workers = 4\n");
        assert!(!LintSectionMigration.detect(doc.as_table()));
    }

    #[test]
    fn detect_true_for_flat_keys() {
        let doc = parse("select = [\"KIS\"]\n");
        assert!(LintSectionMigration.detect(doc.as_table()));
    }

    #[test]
    fn detect_true_for_category_tables() {
        let doc = parse("[KIS]\nlevel = \"error\"\n");
        assert!(LintSectionMigration.detect(doc.as_table()));
    }

    #[test]
    fn apply_moves_flat_keys_under_lint_with_kebab_case_renames() {
        let mut doc = parse(
            "select = [\"KIS\"]\nignore = [\"KPT001\"]\nlevel = \"error\"\ncache_dir = \".cache\"\nper_file_ignores = {\"tests/**\" = [\"KIS001\"]}\nnoqa_aliases = {\"IS\" = \"KIS\"}\n",
        );
        let table = doc.as_table_mut();
        LintSectionMigration.apply(table);

        assert!(!table.contains_key("select"));
        assert!(!table.contains_key("cache_dir"));
        assert_eq!(
            table.get("cache-dir").and_then(|v| v.as_str()),
            Some(".cache")
        );

        let lint = table.get("lint").and_then(|v| v.as_table()).unwrap();
        assert_eq!(
            lint.get("select")
                .and_then(|v| v.as_array())
                .and_then(|a| a.get(0))
                .and_then(|v| v.as_str()),
            Some("KIS")
        );
        assert_eq!(lint.get("level").and_then(|v| v.as_str()), Some("error"));
        assert!(lint.contains_key("per-file-ignores"));
        assert!(lint.contains_key("noqa-aliases"));
    }

    #[test]
    fn apply_moves_kis_category_table_and_renames_unresolved_level() {
        let mut doc = parse(
            "[KIS]\nlevel = \"error\"\nunresolved_level = \"warning\"\nexceptions = [\"typing\"]\n",
        );
        let table = doc.as_table_mut();
        LintSectionMigration.apply(table);

        assert!(!table.contains_key("KIS"));
        let lint = table.get("lint").and_then(|v| v.as_table()).unwrap();
        let kis = lint
            .get("module-only-imports")
            .and_then(|v| v.as_table())
            .expect("module-only-imports table");
        assert_eq!(kis.get("level").and_then(|v| v.as_str()), Some("error"));
        assert_eq!(
            kis.get("unresolved-level").and_then(|v| v.as_str()),
            Some("warning")
        );
        assert!(!kis.contains_key("unresolved_level"));
        assert_eq!(
            kis.get("exceptions")
                .and_then(|v| v.as_array())
                .and_then(|a| a.get(0))
                .and_then(|v| v.as_str()),
            Some("typing")
        );
    }

    #[test]
    fn apply_moves_kpt_category_table_unchanged() {
        let mut doc = parse("[KPT]\nlevel = \"warning\"\nrules_file = \"patterns.toml\"\n");
        let table = doc.as_table_mut();
        LintSectionMigration.apply(table);

        assert!(!table.contains_key("KPT"));
        let lint = table.get("lint").and_then(|v| v.as_table()).unwrap();
        let kpt = lint
            .get("user-defined-patterns")
            .and_then(|v| v.as_table())
            .expect("user-defined-patterns table");
        assert_eq!(kpt.get("level").and_then(|v| v.as_str()), Some("warning"));
        assert_eq!(
            kpt.get("rules_file").and_then(|v| v.as_str()),
            Some("patterns.toml")
        );
    }

    #[test]
    fn apply_is_idempotent_via_detect() {
        let mut doc = parse("select = [\"KIS\"]\n[KIS]\nlevel = \"error\"\n");
        let table = doc.as_table_mut();
        assert!(LintSectionMigration.detect(table));
        LintSectionMigration.apply(table);
        // Second pass: detect must now report false since `lint` exists.
        assert!(!LintSectionMigration.detect(table));
    }
}
