#!/usr/bin/env python3
"""Prepare, check, tag, draft and verify a release. Run the commands in the order listed.

A release is an annotated v<VERSION> tag on main. `release.json` records the version and the
piece's own steps; `CHANGELOG.md` holds the next release under `## Unreleased` and each release
under `## VERSION`. The push to main builds the release files once; the tag workflow attaches
that build's files to a draft release and builds nothing.

The newsletter sends this file from thirtytwobits/london; change it there.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import itertools
import json
import os
import re
import subprocess
import sys
import tempfile
import tomllib
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent.parent
VERSION = re.compile(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)(?:-([0-9A-Za-z.-]+))?")
BUMPS = ("major", "minor", "patch")
UNRELEASED = "Unreleased"
SUBSECTIONS = ("Breaking", "Added", "Fixed", "Documentation")
CI_WORKFLOW = "ci.yml"
RELEASE_WORKFLOW = "release.yml"
RELEASE_FILES = "release-files"
STEPS = ("prepare", "gates", "verify")


class Refusal(Exception):
    pass


def run(*command: str, cwd: Path = ROOT, capture: bool = False) -> str:
    print("$", " ".join(command), file=sys.stderr)
    result = subprocess.run(command, cwd=cwd, check=False, text=True, stdout=subprocess.PIPE if capture else None)
    if result.returncode != 0:
        raise Refusal(f"command failed ({result.returncode}): {' '.join(command)}")
    return result.stdout.strip() if capture else ""


# release.json


@dataclass
class Record:
    """The fields of `release.json` this tool reads. Other fields belong to the piece.

    `files` are the patterns the main build's `release-files` artifact must match; `consumer` is the
    crate `verify` adds to a new consumer; `steps` maps `prepare`, `gates` and `verify` to the
    piece's own commands, in which `{version}`, `{previous}` and `{tag}` are replaced.
    """

    version: str
    repository: str
    files: list[str] = field(default_factory=list)
    consumer: str | None = None
    steps: dict[str, list[list[str]]] = field(default_factory=dict)

    @property
    def slug(self) -> str:
        return self.repository.removeprefix("https://github.com/")

    @property
    def title(self) -> str:
        return self.slug.rpartition("/")[2].capitalize()


def load(root: Path) -> tuple[dict[str, Any], Record]:
    data = json.loads((root / "release.json").read_text(encoding="utf-8"))
    steps = data.get("steps", {})
    unknown = sorted(set(steps) - set(STEPS))
    if unknown:
        raise Refusal(f"release.json steps has unknown names: {', '.join(unknown)}")
    if not data.get("repository", "").startswith("https://github.com/"):
        raise Refusal("release.json repository must be https://github.com/OWNER/NAME")
    record = Record(data["version"], data["repository"], data.get("files", []), data.get("consumer"), steps)
    return data, record


def run_steps(record: Record, name: str, root: Path, **values: str) -> None:
    for command in record.steps.get(name, []):
        run(*(argument.format(**values) for argument in command), cwd=root)


# versions


def parse(version: str) -> tuple[int, int, int, str | None]:
    match = VERSION.fullmatch(version)
    if match is None:
        raise Refusal(f"{version} is not MAJOR.MINOR.PATCH[-PRERELEASE]")
    major, minor, patch, prerelease = match.groups()
    return int(major), int(minor), int(patch), prerelease


Order = tuple[int, int, int, bool, tuple[tuple[int, int, str], ...]]


def order(version: str) -> Order:
    """A key sorting versions by Semantic Versioning precedence.

    A release follows its prereleases. Prerelease identifiers compare one by one: numeric ones by
    value and before alphanumeric ones, which compare as text; a longer list follows its prefix.
    """
    major, minor, patch, prerelease = parse(version)
    identifiers = tuple(
        (0, int(identifier), "") if identifier.isdigit() else (1, 0, identifier)
        for identifier in (prerelease or "").split(".")
        if prerelease is not None
    )
    return (major, minor, patch, prerelease is None, identifiers)


def bumped(current: str, bump: str) -> str:
    major, minor, patch, prerelease = parse(current)
    if prerelease is not None:
        raise Refusal(f"{current} is a prerelease; set the release version by hand")
    if bump == "major":
        return f"{major + 1}.0.0"
    if bump == "minor":
        return f"{major}.{minor + 1}.0"
    return f"{major}.{minor}.{patch + 1}"


def cargo_version(manifest: str) -> str | None:
    version = tomllib.loads(manifest).get("workspace", {}).get("package", {}).get("version")
    return version if isinstance(version, str) else None


def set_cargo_version(manifest: str, previous: str, new: str) -> str:
    """`manifest` with `[workspace.package]` and each of its path dependencies moved from `previous` to `new`."""
    if cargo_version(manifest) != previous:
        raise Refusal(f"Cargo.toml [workspace.package] version is not {previous}")
    return set_path_dependencies(manifest, previous, new, workspace_package=True)


def set_path_dependencies(manifest: str, previous: str, new: str, workspace_package: bool = False) -> str:
    """`manifest` with each inline path dependency at `previous`, in any dependency table, moved to `new`.

    With `workspace_package`, the `[workspace.package]` version moves too.
    """
    table = None
    lines = []
    for line in manifest.splitlines(keepends=True):
        header = re.match(r"\s*\[\[?([^\[\]]+)\]\]?\s*(#.*)?$", line)
        if header:
            table = header.group(1).strip()
        elif workspace_package and table == "workspace.package" and re.match(r'\s*version\s*=\s*"', line):
            line = line.replace(f'"{previous}"', f'"{new}"', 1)
        elif table is not None and table.endswith("dependencies") and re.search(r"\bpath\s*=", line):
            line = re.sub(rf'(version\s*=\s*"[=^~]?){re.escape(previous)}"', rf'\g<1>{new}"', line)
        lines.append(line)
    return "".join(lines)


def member_manifests(root: Path) -> list[Path]:
    """The manifests of the workspace's packages, other than the root manifest."""
    metadata = json.loads(run("cargo", "metadata", "--no-deps", "--format-version", "1", cwd=root, capture=True))
    root_manifest = (root / "Cargo.toml").resolve()
    manifests = {Path(package["manifest_path"]).resolve() for package in metadata["packages"]}
    return sorted(manifests - {root_manifest})


def consumer_manifest(manifest: str) -> str:
    """The manifest of a new consumer of the piece whose root manifest is `manifest`.

    Its own `[workspace]` keeps Cargo from joining, or editing, a workspace above it.
    """
    return (
        '[package]\nname = "release-consumer"\nversion = "0.0.0"\nedition = "2021"\npublish = false\n'
        "\n[workspace]\n" + consumer_patches(manifest)
    )


def consumer_patches(manifest: str) -> str:
    """The `[patch]` tables of `manifest` that a consumer of the release carries, as TOML.

    Workspace patches do not propagate to consumers. Entries that point at a path in the piece are left out.
    """

    def value(item: object) -> str:
        if isinstance(item, bool):
            return "true" if item else "false"
        if isinstance(item, int):
            return str(item)
        if isinstance(item, str):
            return json.dumps(item)
        if isinstance(item, list):
            return "[" + ", ".join(value(element) for element in item) + "]"
        raise Refusal(f"cannot carry the patch value {item!r} to a consumer")

    text = []
    for source, entries in tomllib.loads(manifest).get("patch", {}).items():
        kept = {name: entry for name, entry in entries.items() if "path" not in entry}
        if kept:
            key = source if re.fullmatch(r"[A-Za-z0-9_-]+", source) else json.dumps(source)
            text.append(f"\n[patch.{key}]\n")
            for name, entry in kept.items():
                fields = ", ".join(f"{key} = {value(item)}" for key, item in entry.items())
                text.append(f"{name} = {{ {fields} }}\n")
    return "".join(text)


# CHANGELOG.md


@dataclass
class Entry:
    name: str
    line: int
    body: list[str]


def entries(changelog: str) -> list[Entry]:
    found: list[Entry] = []
    for number, line in enumerate(changelog.splitlines(), 1):
        if line.startswith("## "):
            found.append(Entry(line[3:].strip(), number, []))
        elif found:
            found[-1].body.append(line)
    return found


def entry_problems(entry: Entry) -> list[str]:
    where = f"CHANGELOG.md:{entry.line} `## {entry.name}`"
    problems = []
    names: list[str] = []
    texts: list[bool] = []
    for line in entry.body:
        if line.startswith("### "):
            names.append(line[4:].strip())
            texts.append(False)
        elif line.strip():
            if not names:
                problems.append(f"{where} has text before its first subsection")
                break
            texts[-1] = True
    if not names:
        problems.append(f"{where} has no subsection; use {', '.join(SUBSECTIONS)}")
    for name, text in zip(names, texts):
        if name not in SUBSECTIONS:
            problems.append(f"{where} has `### {name}`; use {', '.join(SUBSECTIONS)}")
        elif not text:
            problems.append(f"{where} `### {name}` is empty")
    known = [name for name in names if name in SUBSECTIONS]
    if known != sorted(known, key=SUBSECTIONS.index) or len(set(known)) != len(known):
        problems.append(f"{where} subsections must appear once each, in the order {', '.join(SUBSECTIONS)}")
    return problems


def changelog_problems(changelog: str) -> list[str]:
    """What keeps `changelog` from the release form. Released entries are checked for their
    headings alone; `## Unreleased` is checked in full."""
    problems = []
    if not changelog.startswith("# Changelog\n"):
        problems.append("CHANGELOG.md must begin with `# Changelog`")
    versions: list[tuple[Entry, Order]] = []
    for index, entry in enumerate(entries(changelog)):
        if entry.name == UNRELEASED:
            if index:
                problems.append(f"CHANGELOG.md:{entry.line} `## {UNRELEASED}` must be the first entry")
            problems += entry_problems(entry)
        elif VERSION.fullmatch(entry.name):
            versions.append((entry, order(entry.name)))
        else:
            problems.append(
                f"CHANGELOG.md:{entry.line} `## {entry.name}` is neither `## {UNRELEASED}` nor `## VERSION`"
            )
    for (newer, newer_order), (older, older_order) in itertools.pairwise(versions):
        if newer_order <= older_order:
            problems.append(f"CHANGELOG.md:{older.line} `## {older.name}` must follow, not precede, `## {newer.name}`")
    return problems


def entry(changelog: str, name: str) -> Entry:
    matches = [found for found in entries(changelog) if found.name == name]
    if len(matches) != 1:
        raise Refusal(f"CHANGELOG.md needs one `## {name}` entry, found {len(matches)}")
    return matches[0]


def notes(changelog: str, version: str) -> str:
    """The body of the `## version` entry, which must be complete."""
    found = entry(changelog, version)
    problems = entry_problems(found)
    if problems:
        raise Refusal("\n".join(problems))
    return "\n".join(found.body).strip() + "\n"


def released(changelog: str, version: str) -> str:
    """`changelog` with its `## Unreleased` entry renamed `## version`."""
    found = entry(changelog, UNRELEASED)
    problems = entry_problems(found)
    if problems:
        raise Refusal("\n".join(problems))
    lines = changelog.splitlines(keepends=True)
    lines[found.line - 1] = f"## {version}\n"
    return "".join(lines)


# checks


def problems(root: Path, record: Record, tag: str | None) -> list[str]:
    changelog = (root / "CHANGELOG.md").read_text(encoding="utf-8")
    found = changelog_problems(changelog)
    manifest = root / "Cargo.toml"
    cargo = cargo_version(manifest.read_text(encoding="utf-8")) if manifest.exists() else None
    if cargo is not None and cargo != record.version:
        found.append(f"Cargo.toml version {cargo} is not release.json version {record.version}")
    if tag is not None:
        if tag != f"v{record.version}":
            found.append(f"{tag} does not name release.json version {record.version}")
        try:
            notes(changelog, record.version)
        except Refusal as refusal:
            found.append(str(refusal))
    return found


def require(root: Path, record: Record, tag: str | None) -> None:
    found = problems(root, record, tag)
    if found:
        raise Refusal("\n".join(found))


def require_clean(root: Path) -> None:
    if run("git", "status", "--porcelain", cwd=root, capture=True):
        raise Refusal("the work tree has changes; commit or remove them first")


def main_build(record: Record, commit: str) -> str:
    """The successful CI run of the push of `commit` to main."""
    found = run(
        "gh",
        "run",
        "list",
        "--repo",
        record.slug,
        "--workflow",
        CI_WORKFLOW,
        "--event",
        "push",
        "--branch",
        "main",
        "--commit",
        commit,
        "--status",
        "success",
        "--json",
        "databaseId",
        "--jq",
        ".[0].databaseId // empty",
        capture=True,
    )
    if not found:
        raise Refusal(f"CI has not passed on main for {commit}; wait for it, or rerun it")
    return found


def require_release_files(record: Record, build: str) -> None:
    if not record.files:
        return
    live = run(
        "gh",
        "api",
        f"repos/{record.slug}/actions/runs/{build}/artifacts",
        "--jq",
        f'.artifacts[] | select(.name == "{RELEASE_FILES}" and .expired == false) | .id',
        capture=True,
    )
    if not live:
        raise Refusal(f"main build {build} has no {RELEASE_FILES} artifact; rerun that build")


def sha256sums(files: list[Path]) -> str:
    return "".join(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n" for path in sorted(files))


def matched(directory: Path, patterns: list[str]) -> list[Path]:
    files = sorted(path for path in directory.rglob("*") if path.is_file())
    chosen = []
    for pattern in patterns:
        hits = [path for path in files if fnmatch.fnmatch(path.name, pattern)]
        if not hits:
            raise Refusal(f"the {RELEASE_FILES} artifact holds nothing matching {pattern}")
        chosen += [path for path in hits if path not in chosen]
    return chosen


# commands


def prepare(arguments: argparse.Namespace) -> None:
    """On a release branch: set the version, name the Unreleased changelog entry and run the piece's prepare steps."""
    root = arguments.root
    require_clean(root)
    if run("git", "branch", "--show-current", cwd=root, capture=True) == "main":
        raise Refusal("prepare a release on a branch, not on main")
    data, record = load(root)
    previous = record.version
    new = bumped(previous, arguments.bump)
    changelog = root / "CHANGELOG.md"
    changelog.write_text(released(changelog.read_text(encoding="utf-8"), new), encoding="utf-8")
    data["version"] = new
    (root / "release.json").write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    manifest = root / "Cargo.toml"
    if manifest.exists() and cargo_version(manifest.read_text(encoding="utf-8")) is not None:
        members = member_manifests(root)
        manifest.write_text(set_cargo_version(manifest.read_text(encoding="utf-8"), previous, new), encoding="utf-8")
        for member in members:
            text = member.read_text(encoding="utf-8")
            moved = set_path_dependencies(text, previous, new)
            if moved != text:
                member.write_text(moved, encoding="utf-8")
        run("cargo", "update", "--workspace", cwd=root)
    run_steps(record, "prepare", root, version=new, previous=previous, tag=f"v{new}")
    print(
        f"\nPrepared {new} from {previous}. Review `git diff`, commit, open a pull request and run `python3 tools/release.py gates`."
    )


def check(arguments: argparse.Namespace) -> None:
    """Check the changelog's form and that the versions agree; with a tag, that it names a complete release."""
    _, record = load(arguments.root)
    require(arguments.root, record, arguments.tag)
    print(
        f"{arguments.tag} is ready to release" if arguments.tag else "CHANGELOG.md and the versions are in release form"
    )


def gates(arguments: argparse.Namespace) -> None:
    """Before merging the release pull request: run the checks and the piece's gates."""
    root = arguments.root
    _, record = load(root)
    require(root, record, f"v{record.version}")
    run_steps(record, "gates", root, version=record.version, previous=record.version, tag=f"v{record.version}")
    print("\nAll release gates passed. Merge once CI passes, then run `python3 tools/release.py tag` on main.")


def notes_command(arguments: argparse.Namespace) -> None:
    """Print the changelog entry of a version."""
    print(notes((arguments.root / "CHANGELOG.md").read_text(encoding="utf-8"), arguments.version), end="")


def tag(arguments: argparse.Namespace) -> None:
    """On main after merging: check the release and its main build, then push its annotated tag."""
    root = arguments.root
    if run("git", "branch", "--show-current", cwd=root, capture=True) != "main":
        raise Refusal("switch to main first")
    require_clean(root)
    run("git", "pull", "--ff-only", "origin", "main", cwd=root)
    # Read the version only after the pull brings in the merged release pull request.
    _, record = load(root)
    name = f"v{record.version}"
    require(root, record, name)
    if run("git", "tag", "--list", name, cwd=root, capture=True) or run(
        "git", "ls-remote", "origin", f"refs/tags/{name}", cwd=root, capture=True
    ):
        raise Refusal(f"{name} already exists; rerun its workflow instead of moving the tag")
    reporting = json.loads(run("gh", "api", f"repos/{record.slug}/private-vulnerability-reporting", capture=True))
    if not reporting.get("enabled"):
        raise Refusal("enable private vulnerability reporting in the repository's security settings")
    commit = run("git", "rev-parse", "HEAD", cwd=root, capture=True)
    require_release_files(record, main_build(record, commit))
    print(f"\nRelease notes:\n\n{notes((root / 'CHANGELOG.md').read_text(encoding='utf-8'), record.version)}")
    if not arguments.yes and input(f"Tag {commit} as {name} and push it to origin? [y/N] ").strip().lower() != "y":
        raise Refusal("not tagged")
    run("git", "tag", "-a", name, "-m", f"{record.title} {record.version}", cwd=root)
    run("git", "push", "origin", name, cwd=root)
    print(
        f"\nPushed {name}. The Draft release workflow attaches the main build's files to a draft:"
        f" {record.repository}/actions/workflows/{RELEASE_WORKFLOW}\n"
        "Review the draft, publish it in GitHub, then run `python3 tools/release.py verify`."
    )


def draft(arguments: argparse.Namespace) -> None:
    """In the tag workflow: draft the release with the notes and the main build's files. Builds nothing."""
    root = arguments.root
    name = arguments.tag
    _, record = load(root)
    require(root, record, name)
    if run("git", "cat-file", "-t", f"refs/tags/{name}", cwd=root, capture=True) != "tag":
        raise Refusal(f"{name} is not an annotated tag")
    commit = run("git", "rev-parse", f"refs/tags/{name}^{{commit}}", cwd=root, capture=True)
    if subprocess.run(["git", "merge-base", "--is-ancestor", commit, "origin/main"], cwd=root, check=False).returncode:
        raise Refusal(f"{name} names {commit}, which is not on main")
    build = main_build(record, commit)
    view = subprocess.run(
        ["gh", "release", "view", name, "--repo", record.slug, "--json", "isDraft", "--jq", ".isDraft"],
        cwd=root,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    existing = view.stdout.strip() if view.returncode == 0 else None
    if existing == "false":
        raise Refusal(f"{name} is published; a published release is never changed")
    with tempfile.TemporaryDirectory() as work:
        work_path = Path(work)
        notes_file = work_path / "notes.md"
        notes_file.write_text(
            notes((root / "CHANGELOG.md").read_text(encoding="utf-8"), record.version), encoding="utf-8"
        )
        assets: list[Path] = []
        if record.files:
            require_release_files(record, build)
            downloaded = work_path / "files"
            run(
                "gh", "run", "download", build, "--repo", record.slug, "--name", RELEASE_FILES, "--dir", str(downloaded)
            )
            assets = matched(downloaded, record.files)
            sums = work_path / "SHA256SUMS"
            sums.write_text(sha256sums(assets), encoding="utf-8")
            assets.append(sums)
        files = [str(path) for path in assets]
        prerelease = ["--prerelease"] if parse(record.version)[3] else []
        if existing == "true":
            run("gh", "release", "edit", name, "--repo", record.slug, "--notes-file", str(notes_file), cwd=root)
            if files:
                run("gh", "release", "upload", name, "--repo", record.slug, "--clobber", *files, cwd=root)
        else:
            run(
                "gh",
                "release",
                "create",
                name,
                "--repo",
                record.slug,
                "--verify-tag",
                "--draft",
                "--title",
                name,
                "--notes-file",
                str(notes_file),
                *prerelease,
                *files,
                cwd=root,
            )
    print(f"Drafted {name} from main build {build} with {len(files)} file(s).")


def verify(arguments: argparse.Namespace) -> None:
    """After publishing the draft by hand: check the release, a Git consumer and the piece's verify steps."""
    root = arguments.root
    _, record = load(root)
    name = f"v{record.version}"
    release = json.loads(
        run("gh", "release", "view", name, "--repo", record.slug, "--json", "isDraft,url", capture=True)
    )
    if release["isDraft"]:
        raise Refusal(f"{name} is still a draft; publish it first: {release['url']}")
    listing = run("git", "ls-remote", "origin", f"refs/tags/{name}^{{}}", cwd=root, capture=True)
    if not listing:
        raise Refusal(f"origin has no annotated {name} tag")
    commit = listing.split("\t")[0]
    runs = json.loads(
        run(
            "gh",
            "run",
            "list",
            "--repo",
            record.slug,
            "--workflow",
            RELEASE_WORKFLOW,
            "--branch",
            name,
            "--json",
            "conclusion,url",
            "--limit",
            "1",
            capture=True,
        )
    )
    if not runs or runs[0]["conclusion"] != "success":
        raise Refusal(f"the Draft release workflow for {name} has not succeeded")
    evidence = [
        f"- Release: {release['url']}",
        f"- Tag: `{name}`",
        f"- Commit: `{commit}`",
        f"- Draft release workflow: {runs[0]['url']}",
    ]
    if record.consumer:
        run("git", "fetch", "--quiet", "origin", "tag", name, cwd=root)
        with tempfile.TemporaryDirectory() as work:
            consumer = Path(work) / "release-consumer"
            (consumer / "src").mkdir(parents=True)
            (consumer / "src" / "lib.rs").write_text("", encoding="utf-8")
            # `cargo add` resolves the dependency, so the patches come first.
            tagged = run("git", "show", f"{commit}:Cargo.toml", cwd=root, capture=True)
            (consumer / "Cargo.toml").write_text(consumer_manifest(tagged), encoding="utf-8")
            run("cargo", "add", record.consumer, "--git", record.repository, "--tag", name, cwd=consumer)
            run("cargo", "check", cwd=consumer)
            lock = tomllib.loads((consumer / "Cargo.lock").read_text(encoding="utf-8"))
        sources = {package.get("source", "") for package in lock["package"] if package["name"] == record.consumer}
        if sources != {f"git+{record.repository}?tag={name}#{commit}"}:
            raise Refusal(f"the consumer resolved {record.consumer} to {sources}, not {commit}")
        evidence.append(
            f"- Consumer: `cargo add {record.consumer} --git {record.repository} --tag {name}` and `cargo check` resolved `{commit}`."
        )
    run_steps(record, "verify", root, version=record.version, previous=record.version, tag=name)
    text = f"Release {name} is published and verified.\n\n" + "\n".join(evidence) + "\n"
    print(f"\n{text}")
    for issue in arguments.issue:
        repo, _, number = issue.rpartition("#")
        run("gh", "issue", "comment", number, "--repo", repo or record.slug, "--body", text)
    print(
        "Recorded. Close the release issue." if arguments.issue else "Record this in the release issue, then close it."
    )


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.set_defaults(root=ROOT)
    commands = parser.add_subparsers(required=True, metavar="COMMAND")

    def command(function: Callable[[argparse.Namespace], None], name: str | None = None) -> argparse.ArgumentParser:
        name = name or function.__name__
        sub = commands.add_parser(name, help=function.__doc__, description=function.__doc__)
        sub.set_defaults(run=function)
        return sub

    command(prepare).add_argument("bump", choices=BUMPS, help="the semantic version component to increment")
    command(check).add_argument("tag", nargs="?", help="a vVERSION tag the release must match")
    command(gates)
    command(notes_command, "notes").add_argument("version")
    command(tag).add_argument("--yes", action="store_true", help="tag and push without asking")
    command(draft).add_argument("tag")
    command(verify).add_argument(
        "--issue",
        action="append",
        default=[],
        metavar="[OWNER/REPO#]NUMBER",
        help="comment the evidence on this issue; repeat for coordinating issues",
    )
    arguments = parser.parse_args()
    try:
        arguments.run(arguments)
    except Refusal as refusal:
        sys.exit(f"release: {refusal}")


if __name__ == "__main__":
    os.chdir(ROOT)
    main()
