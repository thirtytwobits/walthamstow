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
  release-check TAG   TAG names the workspace's version.";

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
    let changelog = fs::read_to_string(root.join("CHANGELOG.md"))?;
    if !changelog.contains(&format!("## {version} ")) {
        return Err(format!("CHANGELOG.md has no entry for {version}").into());
    }
    Ok(())
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
