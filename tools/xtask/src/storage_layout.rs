//! Copyright (c) Scott A Dixon
//!
//! Hidden entries the kernel may create.
//!
//! Durable content belongs in a product's configured workspace paths and journals in the engine
//! data directory (`walthamstow-kernel/src/journals.rs`). A hidden name is only for staging that
//! is gone once its operation ends; each one is listed here with the code that removes it. The
//! kernel lists none: its atomic writes stage beside their file under a visible name.

use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const TRANSIENT: &[(&str, &str)] = &[];

pub(crate) fn check(root: &Path) -> Result<()> {
    let mut violations = Vec::new();
    for entry in std::fs::read_dir(root.join("crates"))? {
        visit(&entry?.path().join("src"), &mut violations)?;
    }
    if violations.is_empty() {
        return Ok(());
    }
    violations.push(
        "Durable content goes in a product's workspace paths, journals in journals.rs. \
         Transient staging is listed in tools/xtask/src/storage_layout.rs."
            .to_owned(),
    );
    Err(violations.join("\n").into())
}

fn visit(dir: &Path, violations: &mut Vec<String>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            visit(&path, violations)?;
            continue;
        }
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let source = std::fs::read_to_string(&path)?;
        for (line, name) in hidden_entries(&source) {
            violations.push(format!("{}:{line}: hidden entry {name:?}", path.display()));
        }
    }
    Ok(())
}

/// Unlisted hidden names that production code joins onto a path, by line.
fn hidden_entries(source: &str) -> Vec<(usize, String)> {
    let hidden = regex_lite::Regex::new(
        r#"(?:\.join\(\s*(?:format!\(\s*)?|const\s+([A-Z_]+)\s*:\s*&str\s*=\s*)"(\.[^"]*)""#,
    )
    .expect("hidden entry pattern");
    let production = source
        .split("#[cfg(test)]\nmod tests {")
        .next()
        .unwrap_or(source);
    hidden
        .captures_iter(production)
        // A suffix names no entry of its own.
        .filter(|capture| {
            !capture
                .get(1)
                .is_some_and(|constant| constant.as_str().ends_with("_SUFFIX"))
        })
        .filter(|capture| !is_permitted(&capture[2]))
        .map(|capture| {
            let line = production[..capture.get(0).unwrap().start()]
                .lines()
                .count();
            (line, capture[2].to_owned())
        })
        .collect()
}

fn is_permitted(name: &str) -> bool {
    // A relative parent is navigation.
    name.starts_with("..") || TRANSIENT.iter().any(|(prefix, _)| name.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relative_parent_is_permitted_and_a_hidden_name_is_not() {
        assert!(is_permitted("../.."));
        assert!(!is_permitted(".lifecycle"));
        assert!(!is_permitted(".workspace-upgrades"));
    }

    #[test]
    fn a_hidden_join_or_constant_is_found_and_staging_suffixes_and_tests_are_not() {
        let source = r#"
fn lifecycle_root(workspace_root: &Path) -> PathBuf {
    workspace_root.join(".lifecycle")
}
fn scratch(root: &Path) -> PathBuf {
    root.join(format!(".journal-{}", 1))
}
const RECORDS: &str = ".records";
const SIDECAR_SUFFIX: &str = ".provenance.json";
fn parent(root: &Path) -> PathBuf {
    root.join("../..")
}
#[cfg(test)]
mod tests {
    fn fixture(root: &Path) -> PathBuf {
        root.join(".fixture")
    }
}
"#;
        let names: Vec<String> = hidden_entries(source)
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        assert_eq!(names, [".lifecycle", ".journal-{}", ".records"]);
    }
}
