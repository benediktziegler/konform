//! Migration: renamed per-rule config tables under `[tool.konform.lint]`.
//!
//! KIS002's table was renamed from `unnecessary-import-alias` to
//! `import-alias-policy`:
//!
//! ```toml
//! [tool.konform.lint.unnecessary-import-alias]
//! level = "error"
//! ```
//!
//! becomes
//!
//! ```toml
//! [tool.konform.lint.import-alias-policy]
//! level = "error"
//! ```
//!
//! Add further `(old, new)` pairs to [`RULE_TABLE_RENAMES`] when another rule's
//! `config_name` changes.

use super::ConfigMigration;
use toml_edit::TableLike;

/// `(old, new)` names of per-rule tables under `[tool.konform.lint]`.
const RULE_TABLE_RENAMES: &[(&str, &str)] = &[("unnecessary-import-alias", "import-alias-policy")];

pub struct RuleTableRenameMigration;

fn lint_table(table: &dyn TableLike) -> Option<&dyn TableLike> {
    table.get("lint")?.as_table_like()
}

impl ConfigMigration for RuleTableRenameMigration {
    fn id(&self) -> &str {
        "rule-table-rename"
    }

    fn description(&self) -> &str {
        "Rename per-rule config tables under [tool.konform.lint]"
    }

    fn detect(&self, table: &dyn TableLike) -> bool {
        lint_table(table).is_some_and(|lint| {
            RULE_TABLE_RENAMES
                .iter()
                .any(|(old, new)| lint.contains_key(old) && !lint.contains_key(new))
        })
    }

    fn apply(&self, table: &mut dyn TableLike) -> Vec<String> {
        let mut notes = Vec::new();
        let Some(lint) = table
            .get_mut("lint")
            .and_then(|item| item.as_table_like_mut())
        else {
            return notes;
        };
        for (old, new) in RULE_TABLE_RENAMES {
            if lint.contains_key(new) {
                continue;
            }
            if let Some(item) = lint.remove(old) {
                lint.insert(new, item);
                notes.push(format!("renamed `lint.{old}` to `lint.{new}`"));
            }
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
    fn renames_unnecessary_import_alias_table() {
        let mut doc = parse("[lint.unnecessary-import-alias]\nlevel = \"error\"\n");
        let table = doc.as_table_mut();
        assert!(RuleTableRenameMigration.detect(table));
        let notes = RuleTableRenameMigration.apply(table);
        assert_eq!(notes.len(), 1);

        let lint = table.get("lint").and_then(|v| v.as_table_like()).unwrap();
        assert!(!lint.contains_key("unnecessary-import-alias"));
        let renamed = lint
            .get("import-alias-policy")
            .and_then(|v| v.as_table_like())
            .expect("renamed table");
        assert_eq!(renamed.get("level").and_then(|v| v.as_str()), Some("error"));
        assert!(!RuleTableRenameMigration.detect(table));
    }

    #[test]
    fn detect_false_when_nothing_to_rename() {
        let doc = parse("[lint]\nselect = []\n");
        assert!(!RuleTableRenameMigration.detect(doc.as_table()));
    }

    #[test]
    fn existing_new_table_wins_and_is_left_alone() {
        let doc = parse(
            "[lint.unnecessary-import-alias]\nlevel = \"error\"\n\
             [lint.import-alias-policy]\nlevel = \"warning\"\n",
        );
        assert!(!RuleTableRenameMigration.detect(doc.as_table()));
    }
}
