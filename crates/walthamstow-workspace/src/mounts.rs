//! Copyright (c) Scott A Dixon
//!
//! Provides the workspace mount abstraction used by the engine server to expose
//! media assets safely. The engine has a single read-only workspace media mount,
//! and the helpers here normalise and validate mount-relative paths.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use walthamstow_kernel::error::{KernelError, KernelResult};

/// A mount: a directory a workspace exposes to clients by id.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
pub struct MountPointDescriptor {
    pub mount_id: String,
    pub label: String,
    pub root_path: String,
    pub read_only: bool,
}

/// Returns the engine's single built-in read-only `media` mount.
pub fn default_media_mount(root_path: &str) -> MountPointDescriptor {
    MountPointDescriptor {
        mount_id: "media".to_string(),
        label: "Workspace media".to_string(),
        root_path: root_path.to_string(),
        read_only: true,
    }
}

/// Resolves a caller-supplied mount id to the built-in workspace media mount.
///
/// Any id other than `media`, including blank ids, is rejected with
/// `not_found`.
pub fn require_mount(mount_id: &str, root_path: &str) -> KernelResult<MountPointDescriptor> {
    let mount = default_media_mount(root_path);
    if mount.mount_id == mount_id {
        Ok(mount)
    } else {
        Err(
            KernelError::not_found(format!("No mount exists with id \"{}\".", mount_id))
                .with_data(serde_json::json!({"mount_id": mount_id}))
                .with_reason(crate::reason_codes::MOUNT_NOT_FOUND),
        )
    }
}

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
        default_media_mount, infer_mime_type, join_mount_path, normalize_mount_relative_path,
        require_mount, resolve_mount_path,
    };
    use std::fs;

    fn make_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let media_root = dir.path().join("data/media/iris");
        fs::create_dir_all(&media_root).expect("media root");
        fs::write(media_root.join("portrait.png"), b"png").expect("portrait");
        fs::create_dir_all(dir.path().join("data/media/videos")).expect("videos dir");
        dir
    }

    #[test]
    fn default_media_mount_and_require_mount_follow_their_contracts() {
        let configured_root = "fixture/content-assets";
        let mount = default_media_mount(configured_root);
        assert_eq!(mount.mount_id, "media");
        assert_eq!(mount.label, "Workspace media");
        assert_eq!(mount.root_path, configured_root);
        assert!(mount.read_only);

        let resolved = require_mount("media", configured_root).expect("media mount");
        assert_eq!(resolved, mount);

        let missing =
            require_mount("missing", configured_root).expect_err("unknown mount should fail");
        assert_eq!(missing.code, "not_found");
        assert_eq!(
            missing.reason_code(),
            Some(crate::reason_codes::MOUNT_NOT_FOUND)
        );
        assert_eq!(missing.data.as_ref().unwrap()["mount_id"], "missing");

        let blank = require_mount("", configured_root).expect_err("blank mount should fail");
        assert_eq!(blank.code, "not_found");
        assert_eq!(
            blank.reason_code(),
            Some(crate::reason_codes::MOUNT_NOT_FOUND)
        );
        assert_eq!(blank.data.as_ref().unwrap()["mount_id"], "");
    }

    #[test]
    fn normalize_and_join_mount_paths_follow_their_contracts() {
        assert_eq!(normalize_mount_relative_path(""), "");
        assert_eq!(normalize_mount_relative_path("/"), "");
        assert_eq!(normalize_mount_relative_path("///"), "");
        assert_eq!(
            normalize_mount_relative_path("/iris\\portraits/hero.png/"),
            "iris/portraits/hero.png"
        );
        assert_eq!(
            normalize_mount_relative_path("  iris//gallery  "),
            "  iris//gallery  "
        );
        assert_eq!(
            normalize_mount_relative_path("\\iris\\gallery\\hero.png"),
            "/iris/gallery/hero.png"
        );

        assert_eq!(
            join_mount_path("", "iris/portrait.png"),
            "iris/portrait.png"
        );
        assert_eq!(
            join_mount_path("/", "/iris/portrait.png/"),
            "iris/portrait.png"
        );
        assert_eq!(join_mount_path("iris", ""), "iris/");
        assert_eq!(
            join_mount_path("", "\\iris\\\\hero.png\\"),
            "iris//hero.png"
        );
        assert_eq!(
            join_mount_path("/iris\\gallery/", "\\hero.png\\"),
            "iris/gallery/hero.png"
        );
        assert_eq!(
            join_mount_path("iris", "gallery//hero.png"),
            "iris/gallery//hero.png"
        );
    }

    #[test]
    fn resolve_mount_path_keeps_results_inside_the_mount_root() {
        let dir = make_root();
        let media_root = dir.path().join("data/media");

        let mount_root = resolve_mount_path(&media_root, "").expect("mount root resolves");
        assert_eq!(
            mount_root,
            dir.path()
                .join("data/media")
                .canonicalize()
                .expect("canonical mount root")
        );

        let existing =
            resolve_mount_path(&media_root, "iris/portrait.png").expect("existing file resolves");
        assert_eq!(
            existing,
            dir.path()
                .join("data/media/iris/portrait.png")
                .canonicalize()
                .expect("canonical existing file")
        );

        let missing = resolve_mount_path(&media_root, "iris/generated/new.png")
            .expect("missing in-root descendant resolves");
        assert_eq!(
            missing,
            dir.path().join("data/media/iris/generated/new.png")
        );

        let traversal =
            resolve_mount_path(&media_root, "../secrets.txt").expect_err("traversal should fail");
        assert_eq!(traversal.code, "invalid_request");
        assert!(traversal
            .message
            .contains("Mount paths must stay within the mount root."));

        fs::create_dir_all(dir.path().join("data/outside")).expect("outside dir");
        let missing_escape = resolve_mount_path(&media_root, "../outside/missing.png")
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

    #[cfg(unix)]
    #[test]
    fn resolve_mount_path_rejects_symlink_escapes_after_canonicalization() {
        use std::os::unix::fs::symlink;

        let dir = make_root();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, b"outside").expect("outside file");
        symlink(&outside, dir.path().join("data/media/escape-link")).expect("symlink");

        let error = resolve_mount_path(&dir.path().join("data/media"), "escape-link")
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
