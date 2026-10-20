//! Copyright (c) Scott A Dixon
//!
//! Paths within a mount: a directory a workspace exposes to clients by id. The helpers
//! normalise mount-relative paths and keep every resolved path inside its mount.

use std::path::{Component, Path, PathBuf};
use walthamstow_kernel::error::{KernelError, KernelResult};

/// Normalises a mount-relative path to the engine's forward-slash form.
///
/// Leading and trailing forward slashes are stripped, backslashes are replaced
/// with `/`, and internal separators or inner whitespace are otherwise
/// preserved.
pub fn normalize_mount_relative_path(path: &str) -> String {
    path.trim_matches('/').replace('\\', "/")
}

/// Joins two mount-relative path segments without introducing duplicate
/// separators at the join boundary.
///
/// The parent is normalised with [`normalize_mount_relative_path`], while the
/// child trims only leading and trailing forward- or backslash separators and
/// converts remaining backslashes to forward slashes.
pub fn join_mount_path(parent: &str, child: &str) -> String {
    let parent = normalize_mount_relative_path(parent);
    let child = child
        .trim_matches(|ch| matches!(ch, '/' | '\\'))
        .replace('\\', "/");
    if parent.is_empty() {
        child
    } else {
        format!("{parent}/{child}")
    }
}

/// Resolves a mount-relative path to an absolute filesystem path while
/// preventing escape from the mount root.
///
/// Empty paths resolve to the mount root itself. Existing paths are
/// canonicalised when possible, while non-existent in-root descendants fall
/// back to their joined path after validating that the deepest existing
/// ancestor still lives inside the canonical mount root. Traversal or
/// canonicalised escape outside the mount root is rejected with
/// `invalid_request`. If the mount root itself cannot be canonicalised, the
/// error is surfaced as `internal`.
pub fn resolve_mount_path(root: &Path, requested_path: &str) -> KernelResult<PathBuf> {
    let mount_root = root
        .canonicalize()
        .map_err(|error| KernelError::internal(error.to_string()))?;
    let normalized = normalize_mount_relative_path(requested_path);

    if normalized.is_empty() {
        return Ok(mount_root);
    }
    if Path::new(&normalized).components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(KernelError::invalid_request(
            "Mount paths must stay within the mount root.",
        ));
    }

    let joined = root.join(&normalized);

    if let Ok(candidate) = joined.canonicalize() {
        if !candidate.starts_with(&mount_root) {
            return Err(KernelError::invalid_request(
                "Mount paths must stay within the mount root.",
            ));
        }
        return Ok(candidate);
    }

    let mut ancestor = joined.as_path();
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or_else(|| {
            KernelError::internal("Failed to resolve a mount path ancestor.".to_string())
        })?;
    }

    let ancestor = ancestor
        .canonicalize()
        .map_err(|error| KernelError::internal(error.to_string()))?;
    if !ancestor.starts_with(&mount_root) {
        return Err(KernelError::invalid_request(
            "Mount paths must stay within the mount root.",
        ));
    }
    Ok(joined)
}

/// Infers a response MIME type for the mount asset server from the file
/// extension.
///
/// Supported image and video extensions are matched case-insensitively.
/// Unknown, extensionless, or directory-like paths return `None`.
pub fn infer_mime_type(path: &str) -> Option<String> {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());
    match extension.as_deref() {
        Some("png") => Some("image/png".to_string()),
        Some("jpg" | "jpeg") => Some("image/jpeg".to_string()),
        Some("webp") => Some("image/webp".to_string()),
        Some("gif") => Some("image/gif".to_string()),
        Some("svg") => Some("image/svg+xml".to_string()),
        Some("mp4") => Some("video/mp4".to_string()),
        Some("webm") => Some("video/webm".to_string()),
        Some("mov") => Some("video/quicktime".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        infer_mime_type, join_mount_path, normalize_mount_relative_path, resolve_mount_path,
    };
    use std::fs;

    fn make_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let mount_root_dir = dir.path().join("data/assets/album");
        fs::create_dir_all(&mount_root_dir).expect("mount root");
        fs::write(mount_root_dir.join("portrait.png"), b"png").expect("portrait");
        fs::create_dir_all(dir.path().join("data/assets/videos")).expect("videos dir");
        dir
    }

    #[test]
    fn normalize_and_join_mount_paths_follow_their_contracts() {
        assert_eq!(normalize_mount_relative_path(""), "");
        assert_eq!(normalize_mount_relative_path("/"), "");
        assert_eq!(normalize_mount_relative_path("///"), "");
        assert_eq!(
            normalize_mount_relative_path("/album\\portraits/hero.png/"),
            "album/portraits/hero.png"
        );
        assert_eq!(
            normalize_mount_relative_path("  album//gallery  "),
            "  album//gallery  "
        );
        assert_eq!(
            normalize_mount_relative_path("\\album\\gallery\\hero.png"),
            "/album/gallery/hero.png"
        );

        assert_eq!(
            join_mount_path("", "album/portrait.png"),
            "album/portrait.png"
        );
        assert_eq!(
            join_mount_path("/", "/album/portrait.png/"),
            "album/portrait.png"
        );
        assert_eq!(join_mount_path("album", ""), "album/");
        assert_eq!(
            join_mount_path("", "\\album\\\\hero.png\\"),
            "album//hero.png"
        );
        assert_eq!(
            join_mount_path("/album\\gallery/", "\\hero.png\\"),
            "album/gallery/hero.png"
        );
        assert_eq!(
            join_mount_path("album", "gallery//hero.png"),
            "album/gallery//hero.png"
        );
    }

    #[test]
    fn resolve_mount_path_keeps_results_inside_the_mount_root() {
        let dir = make_root();
        let mount_root_dir = dir.path().join("data/assets");

        let mount_root = resolve_mount_path(&mount_root_dir, "").expect("mount root resolves");
        assert_eq!(
            mount_root,
            dir.path()
                .join("data/assets")
                .canonicalize()
                .expect("canonical mount root")
        );

        let existing = resolve_mount_path(&mount_root_dir, "album/portrait.png")
            .expect("existing file resolves");
        assert_eq!(
            existing,
            dir.path()
                .join("data/assets/album/portrait.png")
                .canonicalize()
                .expect("canonical existing file")
        );

        let missing = resolve_mount_path(&mount_root_dir, "album/generated/new.png")
            .expect("missing in-root descendant resolves");
        assert_eq!(
            missing,
            dir.path().join("data/assets/album/generated/new.png")
        );

        let traversal = resolve_mount_path(&mount_root_dir, "../secrets.txt")
            .expect_err("traversal should fail");
        assert_eq!(traversal.code, "invalid_request");
        assert!(traversal
            .message
            .contains("Mount paths must stay within the mount root."));

        fs::create_dir_all(dir.path().join("data/outside")).expect("outside dir");
        let missing_escape = resolve_mount_path(&mount_root_dir, "../outside/missing.png")
            .expect_err("missing escape should fail");
        assert_eq!(missing_escape.code, "invalid_request");
        assert!(missing_escape
            .message
            .contains("Mount paths must stay within the mount root."));
    }

    #[test]
    fn resolve_mount_path_returns_internal_when_the_mount_root_is_missing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error =
            resolve_mount_path(&dir.path().join("missing"), "").expect_err("missing root fails");
        assert_eq!(error.code, "internal_error");
    }

    #[test]
    fn a_parent_component_is_refused_even_behind_a_missing_directory() {
        let dir = make_root();
        let mount_root_dir = dir.path().join("data/assets");
        for requested in [
            "missing/../../outside/missing.png",
            "album/../portrait.png",
            "missing/..",
        ] {
            let error = resolve_mount_path(&mount_root_dir, requested).expect_err(requested);
            assert_eq!(error.code, "invalid_request", "{requested}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_mount_path_rejects_symlink_escapes_after_canonicalization() {
        use std::os::unix::fs::symlink;

        let dir = make_root();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, b"outside").expect("outside file");
        symlink(&outside, dir.path().join("data/assets/escape-link")).expect("symlink");

        let error = resolve_mount_path(&dir.path().join("data/assets"), "escape-link")
            .expect_err("symlink escape should fail");
        assert_eq!(error.code, "invalid_request");
        assert!(error
            .message
            .contains("Mount paths must stay within the mount root."));
    }

    #[test]
    fn infer_mime_type_matches_supported_extensions_case_insensitively() {
        let cases = [
            ("image.png", Some("image/png")),
            ("image.JPG", Some("image/jpeg")),
            ("image.jpeg", Some("image/jpeg")),
            ("image.webp", Some("image/webp")),
            ("image.GIF", Some("image/gif")),
            ("image.svg", Some("image/svg+xml")),
            ("clip.mp4", Some("video/mp4")),
            ("clip.WEBM", Some("video/webm")),
            ("clip.mov", Some("video/quicktime")),
            ("folder", None),
            ("archive.txt", None),
            ("nested/path.with.dots/no-extension", None),
            ("nested/folder/", None),
        ];

        for (path, expected) in cases {
            assert_eq!(infer_mime_type(path).as_deref(), expected, "{path}");
        }
    }
}
