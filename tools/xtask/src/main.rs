//! Copyright (c) Scott A Dixon
//!
//! Walthamstow's repository tasks. Run `cargo xtask <task>`.

mod storage_layout;

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::{env, fs};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// A crate, the Walthamstow crates it may build on, and the Clapham crates it may build on.
type Policy = (
    &'static str,
    &'static [&'static str],
    &'static [&'static str],
);

/// The crates Walthamstow releases, each with its policy.
const CRATES: &[Policy] = &[
    ("walthamstow-kernel", &[], &[]),
    ("walthamstow-workspace", &["walthamstow-kernel"], &[]),
    ("walthamstow-upgrades", &["walthamstow-kernel"], &[]),
    (
        "walthamstow-ops",
        &[
            "walthamstow-kernel",
            "walthamstow-workspace",
            "walthamstow-upgrades",
        ],
        &["clapham"],
    ),
];

/// The examples, which Walthamstow does not release, each with its policy.
const EXAMPLES: &[Policy] = &[(
    "walthamstow-ops-example",
    &["walthamstow-kernel", "walthamstow-ops"],
    &[
        "clapham",
        "clapham-cli",
        "clapham-conformance",
        "clapham-mcp-server",
    ],
)];

/// Crate families a Walthamstow crate never builds on: the products and frameworks above it.
const FORBIDDEN_PREFIXES: &[&str] = &["primrose-", "battersea-"];

const USAGE: &str = "Usage: cargo xtask <task>

Tasks:
  check               Format, boundaries, storage layout, Clippy, tests and documentation.
  boundaries          Each crate builds only on the Walthamstow crates it may, and on no product.
  storage-layout      Production code creates no hidden entry but listed transient staging.
  api [--update]      Compare (or rewrite) the public API snapshots in api/.
  release-check TAG   TAG names the workspace's version, and CHANGELOG.md has its entry.
  release-notes TAG   Print the CHANGELOG.md entry for the version TAG names.";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn run(root: &Path, program: &str, args: &[&str], envs: &[(&str, &str)]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .envs(envs.iter().copied())
        .current_dir(root)
        .status()?;
    if !status.success() {
        return Err(format!("{program} {} failed", args.join(" ")).into());
    }
    Ok(())
}

fn output(root: &Path, program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "{program} {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(out.stdout)?)
}

fn metadata(root: &Path) -> Result<Value> {
    Ok(serde_json::from_str(&output(
        root,
        "cargo",
        &["metadata", "--format-version", "1", "--locked"],
    )?)?)
}

/// Every direct dependency of each released crate, by name and kind.
fn boundaries(root: &Path) -> Result<()> {
    let metadata = metadata(root)?;
    let packages = metadata["packages"].as_array().ok_or("No packages")?;
    let mut errors = Vec::new();
    let policies = CRATES.iter().chain(EXAMPLES);
    for (name, allowed, clapham) in policies.clone() {
        let package = packages
            .iter()
            .find(|package| package["name"] == *name)
            .ok_or_else(|| format!("{name} is not a workspace package"))?;
        for dependency in package["dependencies"].as_array().into_iter().flatten() {
            let dependency = dependency["name"].as_str().unwrap_or_default();
            if FORBIDDEN_PREFIXES
                .iter()
                .any(|prefix| dependency.starts_with(prefix))
            {
                errors.push(format!("{name} must not build on {dependency}"));
            } else if (dependency.starts_with("walthamstow-") && !allowed.contains(&dependency))
                || (dependency.starts_with("clapham") && !clapham.contains(&dependency))
            {
                errors.push(format!("{name} may not build on {dependency}"));
            }
        }
    }
    for package in packages {
        let name = package["name"].as_str().unwrap_or_default();
        if name.starts_with("walthamstow-") && !policies.clone().any(|(known, ..)| *known == name) {
            errors.push(format!("{name} has no dependency policy in tools/xtask"));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n").into())
    }
}

fn check(root: &Path) -> Result<()> {
    run(root, "cargo", &["fmt", "--all", "--check"], &[])?;
    boundaries(root)?;
    storage_layout::check(root)?;
    run(
        root,
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
        &[],
    )?;
    run(root, "cargo", &["test", "--workspace", "--locked"], &[])?;
    run(
        root,
        "cargo",
        &["doc", "--workspace", "--no-deps", "--locked"],
        &[("RUSTDOCFLAGS", "-D warnings")],
    )
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("api/toolchain.json has no {key}").into())
}

fn api(root: &Path, update: bool) -> Result<()> {
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(root.join("api/toolchain.json"))?)?;
    let version = string(&settings, "cargo-public-api")?;
    if output(root, "cargo", &["public-api", "--version"])?.trim()
        != format!("cargo-public-api {version}")
    {
        return Err(format!("Install cargo-public-api {version} with --locked").into());
    }
    let toolchain = format!("+{}", string(&settings, "rustdoc")?);
    for (name, ..) in CRATES {
        let value = output(
            root,
            "cargo",
            &[&toolchain, "public-api", "--color", "never", "-p", name],
        )?;
        let file = root.join("api").join(format!("{name}.txt"));
        if update {
            fs::write(&file, value)?;
        } else if fs::read_to_string(&file)? != value {
            return Err(format!(
                "{} is stale; review and run cargo xtask api --update",
                file.display()
            )
            .into());
        }
    }
    Ok(())
}

fn release_check(root: &Path, tag: &str) -> Result<()> {
    let metadata = metadata(root)?;
    let version = metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|package| package["name"] == "walthamstow-kernel")
        .and_then(|package| package["version"].as_str())
        .ok_or("walthamstow-kernel has no version")?;
    if tag != format!("v{version}") {
        return Err(format!("Tag {tag} does not name version {version}").into());
    }
    release_notes(root, version)?;
    Ok(())
}

/// The CHANGELOG.md entry for `version`.
fn release_notes(root: &Path, version: &str) -> Result<String> {
    let changelog = fs::read_to_string(root.join("CHANGELOG.md"))?;
    changelog_entry(&changelog, version)
        .ok_or_else(|| format!("CHANGELOG.md has no entry for {version}").into())
}

/// The text under the `## VERSION` heading of `changelog`, up to the next `## ` heading, or
/// `None` when the heading is absent or the entry is empty. Text after VERSION in the heading,
/// such as a date, is free.
fn changelog_entry(changelog: &str, version: &str) -> Option<String> {
    let heading = format!("## {version}");
    let mut lines = changelog
        .lines()
        .skip_while(|line| *line != heading && !line.starts_with(&format!("{heading} ")));
    lines.next()?;
    let entry = lines
        .take_while(|line| !line.starts_with("## "))
        .collect::<Vec<_>>()
        .join("\n");
    let entry = entry.trim();
    (!entry.is_empty()).then(|| format!("{entry}\n"))
}

fn main() -> ExitCode {
    let root = root();
    let args: Vec<String> = env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["check"] => check(&root),
        ["boundaries"] => boundaries(&root),
        ["storage-layout"] => storage_layout::check(&root),
        ["api"] => api(&root, false),
        ["api", "--update"] => api(&root, true),
        ["release-check", tag] => release_check(&root, tag),
        ["release-notes", tag] => tag
            .strip_prefix('v')
            .ok_or_else(|| format!("{tag} is not a vX.Y.Z tag").into())
            .and_then(|version| release_notes(&root, version))
            .map(|notes| print!("{notes}")),
        ["--help"] | ["help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(USAGE.into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANGELOG: &str = "# Changelog

## 0.4.1 — 2026-11-02

- Fix the second thing.

## 0.4.0 — 2026-10-27

- Breaking: the first thing.
  - In detail.

## 0.3.0

- The oldest thing.
";

    #[test]
    fn an_entry_runs_from_its_heading_to_the_next() {
        assert_eq!(
            changelog_entry(CHANGELOG, "0.4.0").as_deref(),
            Some("- Breaking: the first thing.\n  - In detail.\n")
        );
    }

    #[test]
    fn the_last_entry_runs_to_the_end_and_its_heading_needs_no_date() {
        assert_eq!(
            changelog_entry(CHANGELOG, "0.3.0").as_deref(),
            Some("- The oldest thing.\n")
        );
    }

    #[test]
    fn a_version_without_a_heading_or_with_an_empty_entry_has_no_entry() {
        assert_eq!(changelog_entry(CHANGELOG, "0.4"), None);
        assert_eq!(changelog_entry(CHANGELOG, "0.2.0"), None);
        assert_eq!(
            changelog_entry("## 0.5.0\n\n## 0.4.0\n- A.\n", "0.5.0"),
            None
        );
    }
}
