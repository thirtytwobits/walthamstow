//! Copyright (c) Scott A Dixon
//!
//! Paths within a mount: a directory a workspace exposes to clients by id. A mount is opened as a
//! `cap-std` directory of its own, and every path in it is resolved and opened through that
//! handle, so none leaves it.

use cap_std::fs::Dir;
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

/// The mount at `root`, opened to read as a `cap-std` directory through which every path in it
/// is resolved. A mount root that cannot be opened is an internal failure.
pub fn open_mount(root: &Path) -> KernelResult<Dir> {
    walthamstow_kernel::files::open_dir_to_read(root).map_err(|error| {
        KernelError::internal(format!(
            "Could not open the mount {}: {error}",
            root.display()
        ))
    })
}

/// Resolves a mount-relative path within `mount`, returning it normalised and relative to the
/// mount, to be opened through `mount`. The empty path names the mount itself.
///
/// A parent, root or prefix component is refused, and so is a path that resolves out of the
/// mount through a link, each with `invalid_request`. A path that does not exist yet is accepted
/// when nothing on the way to it leaves the mount; anything later opened through `mount` stays
/// within it.
pub fn resolve_mount_path(mount: &Dir, requested_path: &str) -> KernelResult<PathBuf> {
    let refused = || KernelError::invalid_request("Mount paths must stay within the mount root.");
    let normalized = PathBuf::from(normalize_mount_relative_path(requested_path));
    if normalized.as_os_str().is_empty() {
        return Ok(normalized);
    }
    if normalized.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(refused());
    }
    match mount.metadata(&normalized) {
        Ok(_) => Ok(normalized),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(normalized),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(refused()),
        Err(error) => Err(KernelError::internal(error.to_string())),
    }
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
        infer_mime_type, join_mount_path, normalize_mount_relative_path, open_mount,
        resolve_mount_path,
    };
    use std::fs;
    use std::path::{Path, PathBuf};

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
        let mount = open_mount(&dir.path().join("data/assets")).expect("mount opens");

        assert_eq!(
            resolve_mount_path(&mount, "").expect("mount root resolves"),
            PathBuf::new()
        );

        let existing =
            resolve_mount_path(&mount, "album/portrait.png").expect("existing file resolves");
        assert_eq!(existing, Path::new("album/portrait.png"));
        assert_eq!(
            mount.read(&existing).expect("read through the mount"),
            b"png"
        );

        let missing = resolve_mount_path(&mount, "album/generated/new.png")
            .expect("missing in-root descendant resolves");
        assert_eq!(missing, Path::new("album/generated/new.png"));

        let traversal =
            resolve_mount_path(&mount, "../secrets.txt").expect_err("traversal should fail");
        assert_eq!(traversal.code, "invalid_request");
        assert!(traversal
            .message
            .contains("Mount paths must stay within the mount root."));

        fs::create_dir_all(dir.path().join("data/outside")).expect("outside dir");
        let missing_escape = resolve_mount_path(&mount, "../outside/missing.png")
            .expect_err("missing escape should fail");
        assert_eq!(missing_escape.code, "invalid_request");
        assert!(missing_escape
            .message
            .contains("Mount paths must stay within the mount root."));
    }

    #[test]
    fn opening_a_missing_mount_root_is_internal() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = open_mount(&dir.path().join("missing")).expect_err("missing root fails");
        assert_eq!(error.code, "internal_error");
    }

    #[test]
    fn a_parent_component_is_refused_even_behind_a_missing_directory() {
        let dir = make_root();
        let mount = open_mount(&dir.path().join("data/assets")).expect("mount opens");
        for requested in [
            "missing/../../outside/missing.png",
            "album/../portrait.png",
            "missing/..",
        ] {
            let error = resolve_mount_path(&mount, requested).expect_err(requested);
            assert_eq!(error.code, "invalid_request", "{requested}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_path_through_a_link_resolves_within_the_mount_until_the_link_leads_out() {
        let dir = make_root();
        let mount_root_dir = dir.path().join("data/assets");
        let mount = open_mount(&mount_root_dir).expect("mount opens");
        std::os::unix::fs::symlink("album", mount_root_dir.join("alias")).expect("symlink");
        let held = resolve_mount_path(&mount, "alias/new/upload.png").expect("resolves");
        assert_eq!(held, Path::new("alias/new/upload.png"));
        std::fs::remove_file(mount_root_dir.join("alias")).expect("unlink");
        std::os::unix::fs::symlink(dir.path(), mount_root_dir.join("alias")).expect("retarget");
        let error = resolve_mount_path(&mount, "alias/new/upload.png").expect_err("retargeted");
        assert_eq!(error.code, "invalid_request");
        assert!(mount.create_dir_all("alias/new").is_err());
        assert!(!dir.path().join("new").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_path_through_a_link_to_nothing_is_refused() {
        let dir = make_root();
        let mount_root_dir = dir.path().join("data/assets");
        let mount = open_mount(&mount_root_dir).expect("mount opens");
        std::os::unix::fs::symlink(
            dir.path().join("outside/missing"),
            mount_root_dir.join("dangling"),
        )
        .expect("symlink");
        for requested in ["dangling", "dangling/new.png", "dangling/deeper/new.png"] {
            let error = resolve_mount_path(&mount, requested).expect_err(requested);
            assert_eq!(error.code, "invalid_request", "{requested}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn resolve_mount_path_rejects_a_link_that_leads_out_of_the_mount() {
        use std::os::unix::fs::symlink;

        let dir = make_root();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, b"outside").expect("outside file");
        symlink(&outside, dir.path().join("data/assets/escape-link")).expect("symlink");

        let mount = open_mount(&dir.path().join("data/assets")).expect("mount opens");
        let error =
            resolve_mount_path(&mount, "escape-link").expect_err("symlink escape should fail");
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
