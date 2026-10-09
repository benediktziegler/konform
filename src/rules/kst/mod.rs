//! KST — Konform Structural rules: user-defined checks on the Python syntax tree.
//!
//! Where KPT matches regexes against text, KST matches *code structure*, so a
//! rule can say "no `assert` inside a `@pytest.fixture` function" regardless
//! of formatting, aliases or comments.
//!
//! Rules come from the first available source, like KPT:
//!
//! 1. inline `[[tool.konform.lint.structural-rules.rules]]`
//! 2. `rules_file = "path"` in `[tool.konform.lint.structural-rules]`
//! 3. an auto-discovered `konform_rules.toml` next to the config file
//!
//! See [`KstRule::explain`] for the rule format and matcher vocabulary.

mod matcher;
mod node;
mod resolve;

use super::docs::{DocSection, Example, RuleDocs, RuleOption};
use super::kpt::{glob_matches, resolve_path};
use super::scope::{build_line_starts, offset_to_line_col};
use super::{has_noqa, FileContext, Rule, RuleDoc};
use crate::types::{Level, Violation};
use globset::{Glob, GlobSet, GlobSetBuilder};
use matcher::{Matcher, RawMatcher};
use node::{walk, Flow, Node, Visitor};
use resolve::Imports;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Raw and compiled rules
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: String,
    message: String,
    #[serde(rename = "match")]
    matcher: RawMatcher,
    #[serde(default)]
    files: Vec<String>,
    level: Option<String>,
    help: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RuleFile {
    #[serde(default)]
    rules: Vec<RawRule>,
}

#[derive(Debug)]
struct CompiledRule {
    id: String,
    source: String,
    message: String,
    help: Option<String>,
    level: Level,
    files: Option<GlobSet>,
    raw_files: Vec<String>,
    raw: RawMatcher,
    matcher: Matcher,
}

impl CompiledRule {
    /// The matcher as compact JSON, for `--explain` and cache fingerprints.
    fn matcher_json(&self) -> String {
        serde_json::to_string(&self.raw).unwrap_or_default()
    }

    /// Markdown summary printed by `konform rule --explain <ID>`.
    fn explain(&self) -> String {
        let mut out = format!("# {} — {}\n\n", self.id, self.message);
        out.push_str(&format!("- **Source:** `{}`\n", self.source));
        if !self.raw_files.is_empty() {
            out.push_str(&format!("- **Files:** `{}`\n", self.raw_files.join("`, `")));
        }
        out.push_str(&format!("- **Level:** {}\n", self.level));
        if let Some(help) = &self.help {
            out.push_str(&format!("- **Help:** {help}\n"));
        }
        out.push_str(&format!("- **Match:** `{}`\n", self.matcher_json()));
        out
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

const INLINE_SOURCE: &str = "inline config";

fn parse_default_level(cfg: &toml::Value) -> Level {
    cfg.get("level")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(Level::Warning)
}

fn load_rules(cfg: &toml::Value, config_dir: Option<&Path>) -> Vec<CompiledRule> {
    let default_level = parse_default_level(cfg);

    if let Some(arr) = cfg
        .get("rules")
        .and_then(|v| v.as_array())
        .filter(|a| !a.is_empty())
    {
        let raws = arr
            .iter()
            .filter_map(|v| match RawRule::deserialize(v.clone()) {
                Ok(raw) => Some(raw),
                Err(e) => {
                    eprintln!("konform: skipping structural rule — {e}");
                    None
                }
            })
            .collect();
        return compile_rules(raws, default_level, INLINE_SOURCE);
    }

    if let Some(file) = cfg.get("rules_file").and_then(|v| v.as_str()) {
        return load_file(&resolve_path(file, config_dir), default_level);
    }

    if let Some(candidate) = config_dir.map(|d| d.join("konform_rules.toml")) {
        if candidate.is_file() {
            return load_file(&candidate, default_level);
        }
    }
    vec![]
}

fn load_file(path: &Path, default_level: Level) -> Vec<CompiledRule> {
    let parsed = std::fs::read_to_string(path)
        .map_err(|e| e.to_string())
        .and_then(|content| {
            if path.extension().is_some_and(|e| e == "yaml" || e == "yml") {
                serde_yaml::from_str::<RuleFile>(&content).map_err(|e| e.to_string())
            } else {
                toml::from_str::<RuleFile>(&content).map_err(|e| e.to_string())
            }
        });
    match parsed {
        Ok(file) => compile_rules(file.rules, default_level, &path.display().to_string()),
        Err(e) => {
            eprintln!(
                "konform: cannot load structural rules from {}: {e}",
                path.display()
            );
            vec![]
        }
    }
}

fn compile_rules(raws: Vec<RawRule>, default_level: Level, source: &str) -> Vec<CompiledRule> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for raw in raws {
        let RawRule {
            id,
            message,
            matcher: raw_matcher,
            files: raw_files,
            level,
            help,
        } = raw;
        let skip = |why: &str| eprintln!("konform: skipping structural rule '{id}' — {why}");

        if !id.starts_with("KST") {
            skip("id must start with 'KST' so select/ignore/noqa can address it");
            continue;
        }
        if !seen.insert(id.clone()) {
            skip("duplicate id");
            continue;
        }
        let matcher = match Matcher::compile(&raw_matcher) {
            Ok(m) => m,
            Err(e) => {
                skip(&e);
                continue;
            }
        };
        let files = if raw_files.is_empty() {
            None
        } else {
            let mut builder = GlobSetBuilder::new();
            let mut ok = true;
            for glob in &raw_files {
                match Glob::new(glob) {
                    Ok(g) => {
                        builder.add(g);
                    }
                    Err(e) => {
                        skip(&format!("invalid glob '{glob}': {e}"));
                        ok = false;
                    }
                }
            }
            match builder.build() {
                // A rule whose scope cannot be honoured must not run everywhere.
                Ok(gs) if ok => Some(gs),
                _ => continue,
            }
        };
        out.push(CompiledRule {
            id,
            source: source.to_owned(),
            message,
            help,
            level: level
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(default_level),
            files,
            raw_files,
            raw: raw_matcher,
            matcher,
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Rule
// ---------------------------------------------------------------------------

/// KST — user-defined structural (AST) rules.
pub struct KstRule {
    /// Directory of the config file; resolves `rules_file` and locates
    /// `konform_rules.toml`.
    config_dir: Option<PathBuf>,
    /// Compiled rules for the config last seen; see `KptRule::compiled`.
    compiled: Mutex<Option<(toml::Value, Arc<Vec<CompiledRule>>)>>,
}

impl KstRule {
    pub fn new(config_dir: Option<PathBuf>) -> Self {
        Self {
            config_dir,
            compiled: Mutex::new(None),
        }
    }

    fn rules(&self, cfg: &toml::Value) -> Arc<Vec<CompiledRule>> {
        let mut slot = self.compiled.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((key, rules)) = slot.as_ref() {
            if key == cfg {
                return Arc::clone(rules);
            }
        }
        let rules = Arc::new(load_rules(cfg, self.config_dir.as_deref()));
        *slot = Some((cfg.clone(), Arc::clone(&rules)));
        rules
    }
}

/// One pass over the tree, testing every active rule at every node.
struct Walker<'a> {
    ctx: &'a FileContext,
    active: Vec<&'a CompiledRule>,
    imports: Imports,
    noqa: Vec<&'a str>,
    line_starts: Vec<u32>,
    /// Ancestors of the node being visited, outermost first.
    path: Vec<Node<'a>>,
    out: Vec<Violation>,
}

impl<'a> Visitor<'a> for Walker<'a> {
    fn enter(&mut self, node: Node<'a>) -> Flow {
        for rule in &self.active {
            if !rule.matcher.matches(node, &self.path, &self.imports) {
                continue;
            }
            let span = node.report_span(&self.ctx.source);
            let (line, col) = offset_to_line_col(&self.line_starts, span.start);
            let (end_line, end_col) = offset_to_line_col(&self.line_starts, span.end);
            let noqa = self.noqa.get(line - 1).copied().unwrap_or("");
            if !self.ctx.ignore_noqa && has_noqa(noqa, &rule.id, &self.ctx.noqa_aliases) {
                continue;
            }
            self.out.push(Violation {
                rule: rule.id.clone(),
                line,
                col,
                end_line,
                end_col,
                message: format!("{}: {}", rule.id, rule.message),
                help: rule.help.clone(),
                level: rule.level,
                fixable: false,
            });
        }
        self.path.push(node);
        Flow::Descend
    }

    fn leave(&mut self, _node: Node<'a>) {
        self.path.pop();
    }
}

impl Rule for KstRule {
    fn code(&self) -> &str {
        "KST000"
    }

    fn category(&self) -> &str {
        "KST"
    }

    fn gates_per_violation(&self) -> bool {
        true
    }

    fn category_title(&self) -> &str {
        "User-defined structural rules"
    }

    fn config_name(&self) -> &str {
        "structural-rules"
    }

    fn name(&self) -> &str {
        "Structural rules"
    }

    fn description(&self) -> &str {
        "Checks Python code structure against user-defined AST rules from konform_rules.toml."
    }

    fn check(&self, ctx: &FileContext, cfg: &toml::Value) -> Vec<Violation> {
        let rules = self.rules(cfg);
        if rules.is_empty()
            || !ctx
                .path
                .extension()
                .is_some_and(|e| e == "py" || e == "pyi")
        {
            return vec![];
        }
        let cwd = std::env::current_dir().ok();
        let active: Vec<&CompiledRule> = rules
            .iter()
            .filter(|r| {
                ctx.is_enabled(&r.id)
                    && r.files.as_ref().is_none_or(|gs| {
                        glob_matches(gs, &ctx.path, self.config_dir.as_deref(), cwd.as_deref())
                    })
            })
            .collect();
        if active.is_empty() || !ctx.has_valid_syntax() {
            return vec![];
        }

        let root = Node::root(ctx.parsed().syntax());
        let mut walker = Walker {
            ctx,
            active,
            imports: Imports::collect(root),
            noqa: ctx.noqa_lines(),
            line_starts: build_line_starts(&ctx.source),
            path: Vec::new(),
            out: Vec::new(),
        };
        walk(root, &mut walker);
        walker.out
    }

    fn fingerprint(&self, cfg: &toml::Value) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = seahash::SeaHasher::new();
        for r in self.rules(cfg).iter() {
            (&r.id, &r.message, &r.help, r.level.to_string()).hash(&mut h);
            (&r.raw_files, r.matcher_json()).hash(&mut h);
        }
        h.finish()
    }

    fn catalog(&self, cfg: &toml::Value) -> Vec<RuleDoc> {
        let rules = self.rules(cfg);
        if rules.is_empty() {
            return vec![RuleDoc::of(self)];
        }
        rules
            .iter()
            .map(|r| RuleDoc {
                code: r.id.clone(),
                category: self.category().to_owned(),
                config_name: self.config_name().to_owned(),
                name: "User structural rule".to_owned(),
                description: r.message.clone(),
                explain: format!("{}\n{}", r.explain(), self.explain()),
            })
            .collect()
    }

    fn docs(&self) -> RuleDocs {
        RuleDocs {
            what_it_does: "Matches **code structure** (the syntax tree) against rules you define, \
                so a rule keeps working whatever the formatting, comments or import aliases. \
                KPT matches text; KST matches nodes. Each rule has its own id (starting with \
                `KST`) and is addressable in `select`, `ignore`, `per-file-ignores` and \
                `# noqa`. KST rules are not auto-fixable.",
            why_bad: "Some conventions are about structure, which a regex cannot express reliably: \
                no `assert` inside pytest fixtures, no call to X from within Y, no function with \
                a given decorator. A structural rule states that directly.",
            example: Some(Example {
                bad: "import pytest\n\n\n@pytest.fixture\ndef my_fixture():\n    a = 1\n    assert a == 3  # error[KST001]",
                good: "import pytest\n\n\n@pytest.fixture\ndef my_fixture():\n    a = 1\n    if a != 3:\n        raise AssertionError(a)",
            }),
            sections: vec![
                DocSection {
                    title: "Defining rules",
                    body: "```toml\n\
                        [[tool.konform.lint.structural-rules.rules]]\n\
                        id      = \"KST001\"\n\
                        message = \"Do not use assert inside a pytest fixture.\"\n\
                        help    = \"Raise an explicit exception instead.\"\n\
                        level   = \"error\"\n\
                        files   = [\"tests/**\"]\n\
                        match   = { kind = \"assert\", inside = { decorated_with = \"pytest.fixture\" } }\n\
                        ```\n\n\
                        Rules are loaded from the first available source:\n\n\
                        1. Inline `[[tool.konform.lint.structural-rules.rules]]` in the config file.\n\
                        2. `rules_file = \"path\"` in `[tool.konform.lint.structural-rules]` (`.toml` or `.yaml`).\n\
                        3. `konform_rules.toml` next to the config file (use `[[rules]]` tables).\n\n\
                        An invalid rule is reported on stderr and skipped; the others still run. \
                        Violations are reported at the matched node: the name for functions and \
                        classes, the first line for other blocks.",
                },
                DocSection {
                    title: "Matchers",
                    body: "A `match` is a table whose conditions must **all** hold:\n\n\
                        | Key | Meaning |\n\
                        | --- | --- |\n\
                        | `kind` | node kind, or a list (any of): `function` `class` `lambda` `assert` `assign` `import` `return` `raise` `yield` `try` `with` `for` `while` `if` `call` `await` `name` `attribute` |\n\
                        | `name` | regex searched in the node's identifier (anchor it with `^…$`) |\n\
                        | `qualname` | import-resolved dotted name of a `name` / `attribute` node |\n\
                        | `callee` | import-resolved dotted name of a call's function |\n\
                        | `decorated_with` | import-resolved decorator name(s) on a function or class |\n\
                        | `inside` | some ancestor matches |\n\
                        | `has` | some descendant matches |\n\
                        | `not` | the node does not match |\n\
                        | `all` / `any` | lists of matchers; all / at least one must match |",
                },
                DocSection {
                    title: "Stopping the search",
                    body: "`inside` and `has` accept `stop_by`, a matcher that ends the search: the \
                        node matching it is still tried first, nothing beyond it is. To ignore \
                        helper functions nested in a fixture:\n\n\
                        ```toml\n\
                        match = { kind = \"assert\", inside = { decorated_with = \"pytest.fixture\", stop_by = { kind = \"function\" } } }\n\
                        ```",
                },
                DocSection {
                    title: "Name resolution",
                    body: "Names resolve through the file's imports: `@fixture` after \
                        `from pytest import fixture`, `@pt.fixture` after `import pytest as pt` and \
                        `@pytest.fixture(scope=\"session\")` all count as `pytest.fixture`. Simple \
                        alias assignments (`fx = pytest.fixture`) resolve the same way; they are \
                        ignored for any name that is also assigned something else in the file, \
                        and a call result (`fx = pytest.fixture(scope=\"session\")`) is a value, \
                        not an alias. Resolution is file-wide and ignores local rebinding.",
                },
            ],
            options: vec![
                RuleOption {
                    name: "level",
                    ty: "\"warning\" | \"error\"",
                    default: "`\"warning\"`".to_owned(),
                    description: "Default severity for rules that don't set their own.",
                },
                RuleOption {
                    name: "rules_file",
                    ty: "str",
                    default: "unset".to_owned(),
                    description: "Load rules from this file instead of auto-discovery.",
                },
                RuleOption {
                    name: "rules",
                    ty: "array of tables",
                    default: "`[]`".to_owned(),
                    description: "Inline rules.",
                },
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuleSelection;

    /// Architecture guard: KST's public vocabulary is konform-defined, so
    /// parser crates may only be named in the adapter layer.
    #[test]
    fn no_parser_types_outside_adapter() {
        const ADAPTER: [&str; 2] = ["node.rs", "resolve.rs"];
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/rules/kst");
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            if ADAPTER.contains(&file.as_str()) || !file.ends_with(".rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            // Only inspect code above the test module.
            let code = text.split("#[cfg(test)]").next().unwrap();
            assert!(
                !code.contains("ruff_"),
                "{file} names a parser crate; route it through kst/node.rs"
            );
        }
    }

    const NO_ASSERT_IN_FIXTURE: &str = r#"
[[rules]]
id = "KST001"
message = "no assert in fixtures"
level = "error"
match = { kind = "assert", inside = { kind = "function", decorated_with = "pytest.fixture" } }
"#;

    fn ctx_path(path: &str, source: &str) -> FileContext {
        FileContext::from_source(PathBuf::from(path), source.to_owned())
    }

    fn ctx(source: &str) -> FileContext {
        ctx_path("tests/test_x.py", source)
    }

    fn cfg(toml_src: &str) -> toml::Value {
        toml::from_str(toml_src).unwrap()
    }

    fn rule() -> KstRule {
        KstRule::new(None)
    }

    /// `(rule id, line)` of every violation, for compact assertions.
    fn hits(rules_toml: &str, source: &str) -> Vec<(String, usize)> {
        rule()
            .check(&ctx(source), &cfg(rules_toml))
            .into_iter()
            .map(|v| (v.rule, v.line))
            .collect()
    }

    fn ids(rules_toml: &str, source: &str) -> Vec<String> {
        hits(rules_toml, source)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    // ── the motivating example ────────────────────────────────────────────

    #[test]
    fn assert_in_pytest_fixture_is_reported() {
        let src =
            "import pytest\n\n\n@pytest.fixture\ndef my_fixture():\n    a = 1\n    assert a == 3\n";
        let vs = rule().check(&ctx(src), &cfg(NO_ASSERT_IN_FIXTURE));
        assert_eq!(vs.len(), 1);
        let v = &vs[0];
        assert_eq!((v.rule.as_str(), v.line, v.col), ("KST001", 7, 4));
        assert_eq!(v.message, "KST001: no assert in fixtures");
        assert_eq!(v.level, Level::Error);
        assert!(!v.fixable);
    }

    #[test]
    fn assert_in_plain_function_or_test_is_fine() {
        let src = "import pytest\n\ndef helper():\n    assert 1\n\ndef test_it():\n    assert 2\n";
        assert!(hits(NO_ASSERT_IN_FIXTURE, src).is_empty());
    }

    #[test]
    fn fixture_spellings_are_resolved_through_imports() {
        for (imports, deco) in [
            ("import pytest", "@pytest.fixture"),
            ("import pytest", "@pytest.fixture(scope='session')"),
            ("import pytest as pt", "@pt.fixture"),
            ("from pytest import fixture", "@fixture"),
            ("from pytest import fixture as fx", "@fx()"),
        ] {
            let src = format!("{imports}\n\n{deco}\ndef f():\n    assert 1\n");
            assert_eq!(
                hits(NO_ASSERT_IN_FIXTURE, &src).len(),
                1,
                "{imports} / {deco}"
            );
        }
    }

    #[test]
    fn assigned_fixture_aliases_are_resolved() {
        for (setup, deco) in [
            ("import pytest\nfx = pytest.fixture", "@fx"),
            ("import pytest\nfx = pytest.fixture", "@fx(scope='session')"),
            ("import pytest as pt\nfx = pt.fixture", "@fx"),
            ("from pytest import fixture\nfx = fixture", "@fx"),
            ("import pytest\nfx: object = pytest.fixture", "@fx"),
            ("import pytest\nfx = pytest.fixture\nfy = fx", "@fy"),
        ] {
            let src = format!("{setup}\n\n{deco}\ndef f():\n    assert 1\n");
            assert_eq!(
                hits(NO_ASSERT_IN_FIXTURE, &src).len(),
                1,
                "{setup} / {deco}"
            );
        }
    }

    #[test]
    fn rebound_alias_is_not_trusted() {
        let src = "import pytest\nfx = pytest.fixture\nfx = something_else()\n\n@fx\ndef f():\n    assert 1\n";
        assert!(hits(NO_ASSERT_IN_FIXTURE, src).is_empty());
    }

    #[test]
    fn unrelated_decorator_with_same_name_is_not_a_fixture() {
        let src = "from mylib import fixture\n\n@fixture\ndef f():\n    assert 1\n";
        assert!(hits(NO_ASSERT_IN_FIXTURE, src).is_empty());
    }

    #[test]
    fn async_fixture_and_multiple_asserts() {
        let src = "import pytest\n\n@pytest.fixture\nasync def f():\n    assert 1\n    if x:\n        assert 2\n";
        assert_eq!(
            hits(NO_ASSERT_IN_FIXTURE, src),
            [("KST001".into(), 5), ("KST001".into(), 7)]
        );
    }

    // ── relations ─────────────────────────────────────────────────────────

    const STOP_AT_NESTED_DEF: &str = r#"
[[rules]]
id = "KST001"
message = "m"
match = { kind = "assert", inside = { kind = "function", decorated_with = "pytest.fixture", stop_by = { kind = "function" } } }
"#;

    const NESTED: &str = "import pytest\n\n@pytest.fixture\ndef f():\n    def check():\n        assert 1\n    assert 2\n";

    #[test]
    fn inside_searches_all_ancestors_by_default() {
        assert_eq!(hits(NO_ASSERT_IN_FIXTURE, NESTED).len(), 2);
    }

    #[test]
    fn stop_by_ends_the_ancestor_search() {
        assert_eq!(hits(STOP_AT_NESTED_DEF, NESTED), [("KST001".into(), 7)]);
    }

    #[test]
    fn stop_by_node_is_still_tested_against_the_relation() {
        // The nearest function IS the fixture: it matches before it stops.
        let src = "import pytest\n\n@pytest.fixture\ndef f():\n    assert 1\n";
        assert_eq!(hits(STOP_AT_NESTED_DEF, src).len(), 1);
    }

    const FIXTURE_WITHOUT_YIELD: &str = r#"
[[rules]]
id = "KST002"
message = "fixture never yields"
match = { kind = "function", decorated_with = "pytest.fixture", not = { has = { kind = ["yield"] } } }
"#;

    #[test]
    fn has_and_not_find_fixtures_without_yield() {
        let src = "import pytest\n\n@pytest.fixture\ndef plain():\n    return 1\n\n@pytest.fixture\ndef gen():\n    yield 1\n";
        // Reported on the function name's line, not the decorator.
        assert_eq!(hits(FIXTURE_WITHOUT_YIELD, src), [("KST002".into(), 4)]);
    }

    #[test]
    fn has_stop_by_ignores_nested_scopes() {
        let toml = r#"
[[rules]]
id = "KST003"
message = "m"
match = { kind = "function", has = { kind = "yield", stop_by = { kind = ["function", "lambda"] } } }
"#;
        // Only `outer` yields in its own scope; `wrapper`'s yield is nested.
        let src = "def outer():\n    yield 1\n\ndef wrapper():\n    def inner():\n        yield 2\n    return inner\n";
        assert_eq!(
            hits(toml, src),
            [("KST003".into(), 1), ("KST003".into(), 5)]
        );
    }

    // ── leaf conditions and combinators ───────────────────────────────────

    #[test]
    fn callee_resolves_aliases_and_from_imports() {
        let toml = "[[rules]]\nid = \"KST010\"\nmessage = \"m\"\nmatch = { kind = \"call\", callee = [\"os.system\"] }\n";
        for src in [
            "import os\nos.system('x')\n",
            "import os as o\no.system('x')\n",
            "from os import system\nsystem('x')\n",
            "from os import system as sh\nsh('x')\n",
        ] {
            assert_eq!(ids(toml, src), ["KST010"], "{src}");
        }
        assert!(ids(toml, "import subprocess\nsubprocess.run('x')\n").is_empty());
    }

    #[test]
    fn name_is_a_regex_search_on_the_identifier() {
        let toml = "[[rules]]\nid = \"KST011\"\nmessage = \"m\"\nmatch = { kind = \"function\", name = \"^test_\" }\n";
        let src = "def test_a(): pass\ndef helper(): pass\nclass T:\n    def test_b(self): pass\n";
        assert_eq!(
            hits(toml, src),
            [("KST011".into(), 1), ("KST011".into(), 4)]
        );
    }

    #[test]
    fn qualname_matches_name_and_attribute_nodes() {
        let toml =
            "[[rules]]\nid = \"KST012\"\nmessage = \"m\"\nmatch = { qualname = \"os.environ\" }\n";
        let src = "import os\nx = os.environ\nfrom os import environ\ny = environ\n";
        assert_eq!(
            hits(toml, src),
            [("KST012".into(), 2), ("KST012".into(), 4)]
        );
    }

    #[test]
    fn any_all_and_not_combine() {
        let toml = r#"
[[rules]]
id = "KST013"
message = "m"
match = { any = [{ kind = "raise" }, { kind = "assert" }], not = { inside = { kind = "class" } } }
"#;
        let src = "assert 1\nraise E\nclass C:\n    def f(self):\n        assert 2\n";
        assert_eq!(
            hits(toml, src),
            [("KST013".into(), 1), ("KST013".into(), 2)]
        );

        let toml_all = r#"
[[rules]]
id = "KST014"
message = "m"
match = { all = [{ kind = "function" }, { name = "^_" }] }
"#;
        assert_eq!(
            hits(toml_all, "def _a(): pass\ndef b(): pass\n"),
            [("KST014".into(), 1)]
        );
    }

    #[test]
    fn decorated_with_works_on_classes() {
        let toml = "[[rules]]\nid = \"KST015\"\nmessage = \"m\"\nmatch = { kind = \"class\", decorated_with = \"dataclasses.dataclass\" }\n";
        let src = "from dataclasses import dataclass\n\n@dataclass\nclass A: pass\nclass B: pass\n";
        assert_eq!(hits(toml, src), [("KST015".into(), 4)]);
    }

    #[test]
    fn compound_statements_are_reported_on_their_header_line() {
        let toml = "[[rules]]\nid = \"KST016\"\nmessage = \"m\"\nmatch = { kind = \"try\" }\n";
        let src = "try:\n    pass\nexcept Exception:\n    pass\n";
        let v = &rule().check(&ctx(src), &cfg(toml))[0];
        assert_eq!((v.line, v.col, v.end_line, v.end_col), (1, 0, 1, 4));
    }

    // ── suppression, selection, scope ─────────────────────────────────────

    const FIXTURE_ASSERT_LINE: &str = "import pytest\n\n@pytest.fixture\ndef f():\n    assert 1";

    #[test]
    fn noqa_suppresses_exact_code_and_category() {
        for comment in ["# noqa: KST001", "# noqa: KST", "# noqa"] {
            let src = format!("{FIXTURE_ASSERT_LINE}  {comment}\n");
            assert!(hits(NO_ASSERT_IN_FIXTURE, &src).is_empty(), "{comment}");
        }
        let src = format!("{FIXTURE_ASSERT_LINE}  # noqa: KST002\n");
        assert_eq!(hits(NO_ASSERT_IN_FIXTURE, &src).len(), 1);
    }

    #[test]
    fn ignore_noqa_flag_reports_anyway() {
        let mut c = ctx(&format!("{FIXTURE_ASSERT_LINE}  # noqa\n"));
        c.ignore_noqa = true;
        assert_eq!(rule().check(&c, &cfg(NO_ASSERT_IN_FIXTURE)).len(), 1);
    }

    #[test]
    fn select_and_ignore_gate_each_rule_id() {
        let two = format!(
            "{NO_ASSERT_IN_FIXTURE}\n[[rules]]\nid = \"KST002\"\nmessage = \"m\"\nmatch = {{ kind = \"assert\" }}\n"
        );
        let src = format!("{FIXTURE_ASSERT_LINE}\n");
        let run = |select: &[&str], ignore: &[&str]| {
            let mut c = ctx(&src);
            c.selection = RuleSelection {
                select: select.iter().map(|s| s.to_string()).collect(),
                ignore: ignore.iter().map(|s| s.to_string()).collect(),
            };
            let mut found: Vec<String> = rule()
                .check(&c, &cfg(&two))
                .into_iter()
                .map(|v| v.rule)
                .collect();
            found.sort();
            found
        };
        assert_eq!(run(&[], &[]), ["KST001", "KST002"]);
        assert_eq!(run(&[], &["KST001"]), ["KST002"]);
        assert_eq!(run(&[], &["KST"]), Vec::<String>::new());
        assert_eq!(run(&["KST002"], &[]), ["KST002"]);
    }

    #[test]
    fn files_glob_limits_where_a_rule_runs() {
        let toml = NO_ASSERT_IN_FIXTURE.replace("level = \"error\"", "files = [\"tests/**\"]");
        let src = format!("{FIXTURE_ASSERT_LINE}\n");
        let r = rule();
        let c = cfg(&toml);
        assert_eq!(r.check(&ctx_path("tests/test_a.py", &src), &c).len(), 1);
        assert!(r.check(&ctx_path("src/a.py", &src), &c).is_empty());
    }

    #[test]
    fn non_python_and_broken_files_are_skipped() {
        let src = format!("{FIXTURE_ASSERT_LINE}\n");
        assert!(rule()
            .check(&ctx_path("README.md", &src), &cfg(NO_ASSERT_IN_FIXTURE))
            .is_empty());
        assert!(rule()
            .check(&ctx("def (:\n"), &cfg(NO_ASSERT_IN_FIXTURE))
            .is_empty());
    }

    #[test]
    fn no_rules_means_no_violations() {
        assert!(rule().check(&ctx("assert 1\n"), &cfg("")).is_empty());
    }

    // ── config validation: bad rules are skipped, good ones still run ─────

    fn loaded(rules_toml: &str) -> Vec<String> {
        rule()
            .rules(&cfg(rules_toml))
            .iter()
            .map(|r| r.id.clone())
            .collect()
    }

    fn one(extra: &str) -> String {
        format!("[[rules]]\nid = \"KST900\"\nmessage = \"m\"\n{extra}\n")
    }

    #[test]
    fn invalid_rules_are_skipped() {
        for (why, extra) in [
            ("unknown kind", "match = { kind = \"banana\" }"),
            ("empty matcher", "match = {}"),
            (
                "unknown matcher field",
                "match = { kind = \"assert\", insde = { kind = \"class\" } }",
            ),
            ("bad regex", "match = { name = \"(\" }"),
            (
                "top-level stop_by",
                "match = { kind = \"assert\", stop_by = { kind = \"class\" } }",
            ),
            (
                "stop_by under not",
                "match = { not = { kind = \"assert\", stop_by = { kind = \"class\" } } }",
            ),
            ("empty any", "match = { any = [] }"),
            ("empty kind list", "match = { kind = [] }"),
            (
                "relation without conditions",
                "match = { kind = \"assert\", inside = { stop_by = { kind = \"class\" } } }",
            ),
            ("bad glob", "files = [\"[\"]\nmatch = { kind = \"assert\" }"),
            ("missing match", ""),
            (
                "unknown rule field",
                "colour = \"red\"\nmatch = { kind = \"assert\" }",
            ),
        ] {
            assert!(loaded(&one(extra)).is_empty(), "{why} must be rejected");
        }
    }

    #[test]
    fn id_must_have_the_kst_prefix() {
        let toml = "[[rules]]\nid = \"MINE001\"\nmessage = \"m\"\nmatch = { kind = \"assert\" }\n";
        assert!(loaded(toml).is_empty());
    }

    #[test]
    fn duplicate_ids_keep_the_first() {
        let toml = format!(
            "{}\n{}",
            one("match = { kind = \"assert\" }"),
            one("match = { kind = \"raise\" }")
        );
        assert_eq!(loaded(&toml), ["KST900"]);
        assert_eq!(ids(&toml, "assert 1\nraise E\n"), ["KST900"]);
    }

    #[test]
    fn one_bad_rule_does_not_disable_the_others() {
        let toml = format!(
            "{}\n[[rules]]\nid = \"KST901\"\nmessage = \"m\"\nmatch = {{ kind = \"nope\" }}\n",
            one("match = { kind = \"assert\" }")
        );
        assert_eq!(loaded(&toml), ["KST900"]);
    }

    #[test]
    fn invalid_level_falls_back_to_the_default() {
        let toml = format!(
            "level = \"error\"\n{}",
            one("level = \"loud\"\nmatch = { kind = \"assert\" }")
        );
        // `level` before `[[rules]]` is the section default.
        let v = rule().check(&ctx("assert 1\n"), &cfg(&toml));
        assert_eq!(v[0].level, Level::Error);
    }

    // ── loading from files ────────────────────────────────────────────────

    const FILE_RULES: &str =
        "[[rules]]\nid = \"KST100\"\nmessage = \"m\"\n[rules.match]\nkind = \"assert\"\n";

    #[test]
    fn auto_discovers_konform_rules_toml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("konform_rules.toml"), FILE_RULES).unwrap();
        let r = KstRule::new(Some(dir.path().to_path_buf()));
        assert_eq!(r.check(&ctx("assert 1\n"), &cfg("")).len(), 1);
    }

    #[test]
    fn rules_file_accepts_toml_and_yaml() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("r.toml"), FILE_RULES).unwrap();
        std::fs::write(
            dir.path().join("r.yaml"),
            "rules:\n  - id: KST101\n    message: m\n    match:\n      kind: assert\n",
        )
        .unwrap();
        let r = KstRule::new(Some(dir.path().to_path_buf()));
        let toml_hits = r.check(&ctx("assert 1\n"), &cfg("rules_file = \"r.toml\""));
        assert_eq!(toml_hits[0].rule, "KST100");
        let yaml_hits = r.check(&ctx("assert 1\n"), &cfg("rules_file = \"r.yaml\""));
        assert_eq!(yaml_hits[0].rule, "KST101");
    }

    #[test]
    fn missing_or_broken_rules_file_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.toml"), "[[rules]\n").unwrap();
        let r = KstRule::new(Some(dir.path().to_path_buf()));
        assert!(r
            .check(&ctx("assert 1\n"), &cfg("rules_file = \"bad.toml\""))
            .is_empty());
        assert!(r
            .check(&ctx("assert 1\n"), &cfg("rules_file = \"nope.toml\""))
            .is_empty());
    }

    #[test]
    fn inline_rules_win_over_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("konform_rules.toml"), FILE_RULES).unwrap();
        let r = KstRule::new(Some(dir.path().to_path_buf()));
        let inline = one("match = { kind = \"assert\" }");
        assert_eq!(r.check(&ctx("assert 1\n"), &cfg(&inline))[0].rule, "KST900");
    }

    // ── caching, fingerprint, docs ────────────────────────────────────────

    #[test]
    fn rules_are_compiled_once_per_config() {
        let c = cfg(NO_ASSERT_IN_FIXTURE);
        let r = rule();
        let first = r.rules(&c);
        r.check(&ctx("x = 1\n"), &c);
        assert!(Arc::ptr_eq(&first, &r.rules(&c)));
        assert!(!Arc::ptr_eq(
            &first,
            &r.rules(&cfg(&one("match = { kind = \"raise\" }")))
        ));
    }

    #[test]
    fn fingerprint_tracks_matcher_and_message_changes() {
        let r = rule();
        let a = r.fingerprint(&cfg(&one("match = { kind = \"assert\" }")));
        assert_eq!(
            a,
            r.fingerprint(&cfg(&one("match = { kind = \"assert\" }")))
        );
        assert_ne!(a, r.fingerprint(&cfg(&one("match = { kind = \"raise\" }"))));
        assert_ne!(a, r.fingerprint(&cfg("")));
    }

    #[test]
    fn fingerprint_tracks_rules_file_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("konform_rules.toml");
        std::fs::write(&path, FILE_RULES).unwrap();
        let before = KstRule::new(Some(dir.path().to_path_buf())).fingerprint(&cfg(""));
        std::fs::write(&path, FILE_RULES.replace("assert", "raise")).unwrap();
        let after = KstRule::new(Some(dir.path().to_path_buf())).fingerprint(&cfg(""));
        assert_ne!(before, after);
    }

    #[test]
    fn catalog_lists_user_rules_or_the_umbrella() {
        let r = rule();
        let empty = r.catalog(&cfg(""));
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0].code, "KST000");

        let docs = r.catalog(&cfg(NO_ASSERT_IN_FIXTURE));
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].code, "KST001");
        assert_eq!(docs[0].description, "no assert in fixtures");
        assert!(docs[0].explain.contains("inline config"));
        assert!(docs[0].explain.contains("decorated_with"));
        assert!(docs[0].explain.contains("# structural-rules (KST*)"));
    }

    #[test]
    fn is_not_fixable_and_gates_per_violation() {
        assert!(!rule().fixable());
        assert!(rule().gates_per_violation());
    }

    // ── coverage of the node vocabulary ───────────────────────────────────

    fn kind_rule(kind: &str) -> String {
        format!("[[rules]]\nid = \"KST900\"\nmessage = \"m\"\nmatch = {{ kind = \"{kind}\" }}\n")
    }

    #[test]
    fn every_node_kind_can_be_matched() {
        for (kind, src) in [
            ("lambda", "f = lambda x: x\n"),
            ("with", "with a as b:\n    pass\n"),
            ("for", "for i in x:\n    pass\n"),
            ("while", "while x:\n    pass\n"),
            ("await", "async def f():\n    await g()\n"),
            ("class", "class C:\n    pass\n"),
            ("try", "try:\n    pass\nexcept E:\n    pass\n"),
            ("if", "if x:\n    pass\n"),
        ] {
            assert_eq!(
                ids(&kind_rule(kind), src),
                vec!["KST900".to_owned()],
                "kind {kind}"
            );
        }
    }

    #[test]
    fn name_matches_classes_and_names_but_not_unnamed_nodes() {
        let rules = "[[rules]]\nid = \"KST901\"\nmessage = \"m\"\nmatch = { kind = \"class\", name = \"^Foo$\" }\n";
        assert_eq!(
            ids(rules, "class Foo:\n    pass\nclass Bar:\n    pass\n").len(),
            1
        );
        // A lambda has no identifier, so a `name` condition never matches it.
        let rules = "[[rules]]\nid = \"KST902\"\nmessage = \"m\"\nmatch = { kind = \"lambda\", name = \".\" }\n";
        assert!(ids(rules, "f = lambda x: x\n").is_empty());
    }

    #[test]
    fn name_of_an_attribute_or_method_call_is_the_attribute() {
        let attr = "[[rules]]\nid = \"KST908\"\nmessage = \"m\"\nmatch = { kind = \"attribute\", name = \"^path$\" }\n";
        assert_eq!(ids(attr, "import os\nos.path\nos.sep\n").len(), 1);
        let call = "[[rules]]\nid = \"KST909\"\nmessage = \"m\"\nmatch = { kind = \"call\", name = \"^getcwd$\" }\n";
        assert_eq!(ids(call, "import os\nos.getcwd()\nos.listdir()\n").len(), 1);
    }

    #[test]
    fn resolved_names_only_apply_to_the_nodes_they_describe() {
        // `callee` needs a call, `qualname` a name/attribute,
        // `decorated_with` a function or class.
        for cond in [
            "callee = \"os.getcwd\"",
            "qualname = \"os\"",
            "decorated_with = \"x\"",
        ] {
            let rules = format!(
                "[[rules]]\nid = \"KST903\"\nmessage = \"m\"\nmatch = {{ kind = \"assert\", {cond} }}\n"
            );
            assert!(ids(&rules, "assert 1\n").is_empty(), "{cond}");
        }
        let rules = "[[rules]]\nid = \"KST904\"\nmessage = \"m\"\nmatch = { kind = \"class\", decorated_with = \"dataclasses.dataclass\" }\n";
        let src = "import dataclasses\n\n@dataclasses.dataclass\nclass C:\n    pass\n\nclass D:\n    pass\n";
        assert_eq!(hits(rules, src), vec![("KST904".to_owned(), 4)]);
    }

    #[test]
    fn name_of_a_call_is_its_callee_name_when_there_is_one() {
        let rules = "[[rules]]\nid = \"KST907\"\nmessage = \"m\"\nmatch = { kind = \"call\", name = \"^print$\" }\n";
        assert_eq!(ids(rules, "print(1)\nlen(2)\n").len(), 1);
        // The callee of `(lambda: 1)()` or `f()()` has no simple name.
        assert!(ids(rules, "(lambda: 1)()\nf()()\n").is_empty());
    }

    #[test]
    fn has_stops_at_the_first_descendant_that_matches() {
        let rules = "[[rules]]\nid = \"KST905\"\nmessage = \"m\"\nmatch = { kind = \"function\", has = { kind = \"assert\" } }\n";
        let src = "def f():\n    assert 1\n    assert 2\n    assert 3\n";
        assert_eq!(ids(rules, src).len(), 1);
    }

    #[test]
    fn explain_lists_files_and_help_of_a_rule() {
        let cfg = cfg(
            "[[rules]]\nid = \"KST906\"\nmessage = \"m\"\nhelp = \"do better\"\nfiles = [\"src/**\"]\nmatch = { kind = \"assert\" }\n",
        );
        let explain = rule().catalog(&cfg).remove(0).explain;
        assert!(explain.contains("- **Files:** `src/**`"), "{explain}");
        assert!(explain.contains("- **Help:** do better"), "{explain}");
    }
}
