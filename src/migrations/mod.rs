//! Config migration framework.
//!
//! konform's TOML config shape is allowed to change between versions
//! (renaming keys, restructuring tables). Rather than support every historic
//! shape forever with `#[serde(alias = ...)]` shims, each breaking change to
//! the config format is expressed as a [`ConfigMigration`] that rewrites an
//! old shape into the current one. Migrations run automatically the first
//! time a config file is loaded, and the rewritten file is persisted using
//! `toml_edit` so comments and formatting elsewhere in the file survive.
//!
//! Add a new migration whenever the config shape changes again: implement
//! [`ConfigMigration`], register it in [`all_migrations`], and it will run
//! (once) for every project still on the old shape.

mod lint_section;

use std::path::Path;
use toml_edit::{DocumentMut, TableLike};

/// A single, one-way rewrite of an old config shape into a newer one.
///
/// Implementations operate on the `[tool.konform]` (or top-level `[konform]`)
/// table directly, using `toml_edit` so surrounding comments/formatting in
/// the rest of the file are preserved.
pub trait ConfigMigration: Send + Sync {
    /// Short, stable identifier, e.g. `"lint-section"`. Used only in log
    /// output, never persisted.
    fn id(&self) -> &str;

    /// One-line human-readable summary shown in the migration notice.
    fn description(&self) -> &str;

    /// Return `true` when `table` still has the old shape this migration
    /// upgrades from. Must return `false` once [`Self::apply`] has run, so
    /// re-running migrations on an already-migrated file is a no-op.
    fn detect(&self, table: &dyn TableLike) -> bool;

    /// Rewrite `table` in place. Returns a list of human-readable change
    /// notes (e.g. `"moved `select` to `lint.select`"`) printed to the user.
    fn apply(&self, table: &mut dyn TableLike) -> Vec<String>;
}

/// Registry of all known migrations, run in order by [`run_migrations`].
pub fn all_migrations() -> Vec<Box<dyn ConfigMigration>> {
    vec![Box::new(lint_section::LintSectionMigration)]
}

/// Report of a single migration that was applied.
#[derive(Debug, Clone)]
pub struct MigrationReport {
    pub id: String,
    pub description: String,
    pub notes: Vec<String>,
}

/// Run every registered migration whose [`ConfigMigration::detect`] matches
/// against `table`, applying it in place.
///
/// Returns one [`MigrationReport`] per migration that actually applied.
pub fn run_migrations(table: &mut dyn TableLike) -> Vec<MigrationReport> {
    let mut reports = Vec::new();
    for migration in all_migrations() {
        if migration.detect(table) {
            let notes = migration.apply(table);
            reports.push(MigrationReport {
                id: migration.id().to_owned(),
                description: migration.description().to_owned(),
                notes,
            });
        }
    }
    reports
}

/// Return the konform section of `doc` to migrate, matching the same
/// section-resolution rules as [`crate::config::load_config`]:
/// `[tool.konform]` for `pyproject.toml`, else `[konform]` if present,
/// else the whole document (bare `konform.toml` with no wrapper table).
fn konform_section_mut(doc: &mut DocumentMut, is_pyproject: bool) -> Option<&mut dyn TableLike> {
    if is_pyproject {
        doc.get_mut("tool")?
            .as_table_like_mut()?
            .get_mut("konform")?
            .as_table_like_mut()
    } else if doc.contains_key("konform") {
        doc.get_mut("konform")?.as_table_like_mut()
    } else {
        Some(doc.as_table_mut())
    }
}

/// Migrate `content` (the raw text of `path`) to the current config shape.
///
/// Returns `Some(new_content)` when at least one migration applied, `None`
/// when the file is already current (or isn't valid TOML / has no konform
/// section to migrate). Does not touch disk — callers decide whether/how to
/// persist the result.
pub fn migrate_content(path: &Path, content: &str) -> Option<(String, Vec<MigrationReport>)> {
    let is_pyproject = path.file_name().is_some_and(|n| n == "pyproject.toml");
    let mut doc: DocumentMut = content.parse().ok()?;
    let section = konform_section_mut(&mut doc, is_pyproject)?;
    let reports = run_migrations(section);
    if reports.is_empty() {
        return None;
    }
    Some((doc.to_string(), reports))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_content_returns_none_for_already_current_config() {
        let content =
            "[tool.konform]\ncache-dir = \".konform_cache\"\n\n[tool.konform.lint]\nselect = []\n";
        let result = migrate_content(Path::new("pyproject.toml"), content);
        assert!(result.is_none());
    }

    #[test]
    fn migrate_content_returns_none_for_konform_toml_without_section() {
        let content = "not-a-konform-key = true\n";
        // Bare konform.toml with no matching keys at all: no migration applies.
        let result = migrate_content(Path::new("konform.toml"), content);
        assert!(result.is_none());
    }

    #[test]
    fn migrate_content_reports_applied_migration() {
        let content = "[tool.konform]\nselect = [\"KIS\"]\n";
        let (_new_content, reports) =
            migrate_content(Path::new("pyproject.toml"), content).expect("should migrate");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].id, "lint-section");
        assert!(!reports[0].notes.is_empty());
    }
}
