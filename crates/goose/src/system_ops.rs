//! Filesystem and attachment helpers used by the ACP `_goose/system/*` methods.
//!
//! These helpers were originally implemented as Tauri commands in
//! `ui/goose2/src-tauri/src/commands/system.rs`. They were migrated here as
//! part of [#8692](https://github.com/aaif-goose/goose/issues/8692) so the
//! `goose serve` process owns the filesystem logic and the desktop shell can
//! stay a thin layer.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use base64::Engine;
use fs_err as fs;
use serde::{Deserialize, Serialize};

const DEFAULT_FILE_MENTION_LIMIT: usize = 1500;
const MAX_FILE_MENTION_LIMIT: usize = 5000;
const MAX_SCAN_DEPTH: usize = 8;
const MAX_IMAGE_ATTACHMENT_BYTES: u64 = 20 * 1024 * 1024;

/// A single filesystem entry (file or directory) returned by
/// [`list_directory_entries`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileTreeEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
}

/// Metadata for a single attachment path, returned by
/// [`inspect_attachment_paths`].
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentPathInfo {
    pub name: String,
    pub path: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// Decoded image attachment payload.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachmentPayload {
    pub base64: String,
    pub mime_type: String,
}

/// Resolve the current user's home directory.
pub fn get_home_dir() -> Result<String, String> {
    let home_dir = dirs::home_dir().ok_or("Could not determine home directory")?;
    Ok(home_dir.to_string_lossy().into_owned())
}

/// Returns true if `path` exists on disk.
pub fn path_exists(path: &str) -> bool {
    Path::new(path).exists()
}

/// Write `contents` to `path`, creating any missing parent directories.
pub fn write_file(path: &str, contents: &str) -> Result<(), String> {
    let path = Path::new(path);
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create '{}': {}", parent.display(), e))?;
        }
    }
    fs::write(path, contents)
        .map_err(|e| format!("Failed to write file '{}': {}", path.display(), e))
}

/// List the immediate children of `path`, sorted directories-first then by
/// case-insensitive name. Skips `.git/` to match the desktop shell behaviour.
pub fn list_directory_entries(path: &str) -> Result<Vec<FileTreeEntry>, String> {
    read_directory_entries(Path::new(path))
}

fn read_directory_entries(path: &Path) -> Result<Vec<FileTreeEntry>, String> {
    if !path.exists() {
        return Err(format!("Directory does not exist: {}", path.display()));
    }

    let metadata = fs::metadata(path)
        .map_err(|error| format!("Failed to inspect '{}': {}", path.display(), error))?;
    if !metadata.is_dir() {
        return Err(format!("Path is not a directory: {}", path.display()));
    }

    let mut entries = Vec::new();
    let reader = fs::read_dir(path)
        .map_err(|error| format!("Failed to read directory '{}': {}", path.display(), error))?;

    for entry in reader {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".git" {
            continue;
        }
        let Some(file_tree_entry) = build_file_tree_entry(entry.path(), name) else {
            continue;
        };
        entries.push(file_tree_entry);
    }

    entries.sort_by(|a, b| {
        let a_rank = if a.kind == "directory" { 0 } else { 1 };
        let b_rank = if b.kind == "directory" { 0 } else { 1 };
        a_rank
            .cmp(&b_rank)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.name.cmp(&b.name))
    });

    Ok(entries)
}

fn build_file_tree_entry(path: PathBuf, name: String) -> Option<FileTreeEntry> {
    let metadata = fs::symlink_metadata(&path).ok()?;
    let file_type = metadata.file_type();

    Some(FileTreeEntry {
        name,
        path: path.to_string_lossy().into_owned(),
        kind: if file_type.is_dir() {
            "directory".to_string()
        } else {
            "file".to_string()
        },
    })
}

fn inspect_attachment_path(path: &Path) -> Result<AttachmentPathInfo, String> {
    if !path.exists() {
        return Err(format!(
            "Attachment path does not exist: {}",
            path.display()
        ));
    }

    let metadata = fs::metadata(path)
        .map_err(|error| format!("Failed to inspect '{}': {}", path.display(), error))?;
    let name = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned());

    Ok(AttachmentPathInfo {
        name,
        path: path.to_string_lossy().into_owned(),
        kind: if metadata.is_dir() {
            "directory".to_string()
        } else {
            "file".to_string()
        },
        mime_type: if metadata.is_file() {
            mime_guess::from_path(path)
                .first_raw()
                .map(std::borrow::ToOwned::to_owned)
        } else {
            None
        },
    })
}

fn normalized_path_key(path: &Path) -> String {
    if let Ok(canonical) = path.canonicalize() {
        return canonical.to_string_lossy().into_owned();
    }

    let raw = path.to_string_lossy().into_owned();
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        raw.to_lowercase()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        raw
    }
}

fn normalize_attachment_paths(paths: Vec<String>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut normalized = Vec::new();

    for raw_path in paths {
        let trimmed = raw_path.trim();
        if trimmed.is_empty() {
            continue;
        }

        let path = PathBuf::from(trimmed);
        let key = normalized_path_key(&path);
        if seen.insert(key) {
            normalized.push(path);
        }
    }

    normalized
}

/// Look up metadata for each path in `paths`. Missing entries are silently
/// skipped so callers can pass in a heterogeneous batch (e.g. drag-and-drop
/// payloads that include now-deleted paths).
pub fn inspect_attachment_paths(paths: Vec<String>) -> Vec<AttachmentPathInfo> {
    let mut attachments = Vec::new();

    for path in normalize_attachment_paths(paths) {
        if let Ok(attachment) = inspect_attachment_path(&path) {
            attachments.push(attachment);
        }
    }

    attachments
}

/// Read an image file from disk and return it as a base64-encoded payload.
pub fn read_image_attachment(path: &str) -> Result<ImageAttachmentPayload, String> {
    let attachment = inspect_attachment_path(Path::new(path))?;
    let mime_type = attachment
        .mime_type
        .ok_or_else(|| format!("Unable to determine image type for '{}'", attachment.path))?;

    if !mime_type.starts_with("image/") {
        return Err(format!("Attachment is not an image: {}", attachment.path));
    }

    let metadata = fs::metadata(&attachment.path)
        .map_err(|error| format!("Failed to inspect image '{}': {}", attachment.path, error))?;
    if metadata.len() > MAX_IMAGE_ATTACHMENT_BYTES {
        return Err(format!(
            "Image attachment '{}' exceeds the {} MB limit",
            attachment.path,
            MAX_IMAGE_ATTACHMENT_BYTES / (1024 * 1024)
        ));
    }

    let bytes = fs::read(&attachment.path)
        .map_err(|error| format!("Failed to read image '{}': {}", attachment.path, error))?;

    Ok(ImageAttachmentPayload {
        base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        mime_type,
    })
}

fn normalize_roots(roots: Vec<String>) -> Vec<PathBuf> {
    let mut dedup = HashSet::new();
    let mut normalized = Vec::new();
    for root in roots {
        let trimmed = root.trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = PathBuf::from(trimmed);
        let key = normalized_path_key(&path);
        if dedup.insert(key) {
            normalized.push(path);
        }
    }
    normalized
}

/// Walk `roots` and return up to `max_results` file paths, respecting hidden /
/// gitignore rules so we don't surface things like `node_modules` or `.git`.
pub fn list_files_for_mentions(roots: Vec<String>, max_results: Option<usize>) -> Vec<String> {
    let roots = normalize_roots(roots);
    if roots.is_empty() {
        return Vec::new();
    }

    let limit = max_results
        .unwrap_or(DEFAULT_FILE_MENTION_LIMIT)
        .clamp(1, MAX_FILE_MENTION_LIMIT);

    let mut builder = ignore::WalkBuilder::new(&roots[0]);
    for root in &roots[1..] {
        builder.add(root);
    }
    builder
        .max_depth(Some(MAX_SCAN_DEPTH))
        .follow_links(false)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true);

    // Canonicalize roots so we can reject paths that escape via symlink targets.
    let canonical_roots: Vec<PathBuf> = roots
        .iter()
        .filter_map(|root| root.canonicalize().ok())
        .collect();

    let mut seen = HashSet::new();
    let mut files = Vec::new();

    for entry in builder.build().flatten() {
        if files.len() >= limit {
            break;
        }
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_file() {
            continue;
        }
        let canonical = match entry.path().canonicalize() {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !canonical_roots
            .iter()
            .any(|root| canonical.starts_with(root))
        {
            continue;
        }
        let path_str = entry.path().to_string_lossy().to_string();
        let dedup_key = normalized_path_key(entry.path());
        if seen.insert(dedup_key) {
            files.push(path_str);
        }
    }

    files.sort_by_key(|path| path.to_lowercase());
    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use tempfile::tempdir;

    /// Create a temp dir with `git init` so the ignore crate picks up `.gitignore`.
    fn git_tempdir() -> tempfile::TempDir {
        let dir = tempdir().expect("tempdir");
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .output()
            .expect("git init");
        dir
    }

    #[test]
    fn respects_gitignore() {
        let dir = git_tempdir();
        let root = dir.path();
        let src = root.join("src");
        let ignored = root.join("node_modules").join("pkg");

        fs::create_dir_all(&src).expect("src dir");
        fs::create_dir_all(&ignored).expect("ignored dir");
        fs::write(src.join("main.ts"), "export {}").expect("source file");
        fs::write(ignored.join("index.js"), "module.exports = {}").expect("ignored file");
        fs::write(root.join(".gitignore"), "node_modules/\n").expect(".gitignore");

        let files = list_files_for_mentions(vec![root.to_string_lossy().to_string()], Some(50));

        let joined = files.join("\n");
        assert!(joined.contains("main.ts"), "should include source files");
        assert!(
            !joined.contains("node_modules"),
            "should respect .gitignore"
        );
    }

    #[test]
    fn skips_hidden_files() {
        let dir = git_tempdir();
        let root = dir.path();

        fs::write(root.join("visible.ts"), "").expect("visible file");
        fs::write(root.join(".hidden"), "").expect("hidden file");

        let files = list_files_for_mentions(vec![root.to_string_lossy().to_string()], Some(50));

        let joined = files.join("\n");
        assert!(joined.contains("visible.ts"));
        assert!(!joined.contains(".hidden"));
    }

    #[test]
    fn lists_directory_entries_with_expected_sorting_and_visibility() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();

        fs::create_dir_all(root.join(".git")).expect(".git dir");
        fs::create_dir_all(root.join(".github")).expect(".github dir");
        fs::create_dir_all(root.join("node_modules")).expect("node_modules dir");
        fs::create_dir_all(root.join("src")).expect("src dir");
        fs::write(root.join(".env"), "").expect(".env");
        fs::write(root.join(".gitignore"), "node_modules/\n").expect(".gitignore");
        fs::write(root.join("README.md"), "").expect("README");
        fs::write(root.join("alpha.ts"), "").expect("alpha");

        let entries = read_directory_entries(root).expect("entries");

        assert_eq!(
            entries,
            vec![
                FileTreeEntry {
                    name: ".github".into(),
                    path: root.join(".github").to_string_lossy().into_owned(),
                    kind: "directory".into(),
                },
                FileTreeEntry {
                    name: "node_modules".into(),
                    path: root.join("node_modules").to_string_lossy().into_owned(),
                    kind: "directory".into(),
                },
                FileTreeEntry {
                    name: "src".into(),
                    path: root.join("src").to_string_lossy().into_owned(),
                    kind: "directory".into(),
                },
                FileTreeEntry {
                    name: ".env".into(),
                    path: root.join(".env").to_string_lossy().into_owned(),
                    kind: "file".into(),
                },
                FileTreeEntry {
                    name: ".gitignore".into(),
                    path: root.join(".gitignore").to_string_lossy().into_owned(),
                    kind: "file".into(),
                },
                FileTreeEntry {
                    name: "alpha.ts".into(),
                    path: root.join("alpha.ts").to_string_lossy().into_owned(),
                    kind: "file".into(),
                },
                FileTreeEntry {
                    name: "README.md".into(),
                    path: root.join("README.md").to_string_lossy().into_owned(),
                    kind: "file".into(),
                },
            ]
        );
    }

    #[test]
    fn list_directory_entries_errors_for_missing_paths() {
        let dir = tempdir().expect("tempdir");
        let missing = dir.path().join("missing");

        let error = read_directory_entries(&missing).expect_err("missing dir should error");
        assert!(error.contains("Directory does not exist"));
    }

    #[test]
    fn build_file_tree_entry_skips_missing_children() {
        let dir = tempdir().expect("tempdir");
        let missing = dir.path().join("missing.ts");

        let entry = build_file_tree_entry(missing, "missing.ts".into());

        assert_eq!(entry, None);
    }

    #[test]
    #[cfg(unix)]
    fn list_directory_entries_errors_for_unreadable_directories() {
        let dir = tempdir().expect("tempdir");
        let blocked = dir.path().join("blocked");
        fs::create_dir(&blocked).expect("blocked dir");

        let original_permissions = fs::metadata(&blocked).expect("metadata").permissions();
        let mut unreadable_permissions = original_permissions.clone();
        unreadable_permissions.set_mode(0o000);
        fs::set_permissions(&blocked, unreadable_permissions).expect("set unreadable");

        let error = read_directory_entries(&blocked).expect_err("unreadable dir should error");

        let mut restored_permissions = original_permissions;
        restored_permissions.set_mode(0o700);
        fs::set_permissions(&blocked, restored_permissions).expect("restore permissions");

        assert!(error.contains("Failed to read directory"));
    }

    #[test]
    fn inspects_file_and_directory_attachments() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        let folder = root.join("screenshots");
        let file = root.join("report.txt");

        fs::create_dir_all(&folder).expect("folder");
        fs::write(&file, "hello").expect("file");

        let inspected_dir = inspect_attachment_path(&folder).expect("directory");
        let inspected_file = inspect_attachment_path(&file).expect("file");

        assert_eq!(inspected_dir.kind, "directory");
        assert_eq!(inspected_dir.name, "screenshots");
        assert_eq!(inspected_dir.mime_type, None);

        assert_eq!(inspected_file.kind, "file");
        assert_eq!(inspected_file.name, "report.txt");
        assert_eq!(inspected_file.mime_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn reads_image_attachment_payloads() {
        let dir = tempdir().expect("tempdir");
        let image = dir.path().join("pixel.png");
        let png_bytes = base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9sU4nS0AAAAASUVORK5CYII=")
            .expect("decode png");

        fs::write(&image, png_bytes).expect("png file");

        let payload = read_image_attachment(&image.to_string_lossy()).expect("payload");

        assert_eq!(payload.mime_type, "image/png");
        assert!(!payload.base64.is_empty());
    }

    #[test]
    fn dedupes_attachment_paths_using_platform_path_rules() {
        let normalized = normalize_attachment_paths(vec![
            "/tmp/Readme.md".into(),
            "/tmp/README.md".into(),
            "/tmp/Readme.md".into(),
        ]);

        if cfg!(any(target_os = "macos", target_os = "windows")) {
            assert_eq!(normalized, vec![PathBuf::from("/tmp/Readme.md")]);
        } else {
            assert_eq!(
                normalized,
                vec![
                    PathBuf::from("/tmp/Readme.md"),
                    PathBuf::from("/tmp/README.md")
                ]
            );
        }
    }

    #[test]
    fn skips_invalid_attachment_paths_without_dropping_valid_ones() {
        let dir = tempdir().expect("tempdir");
        let valid = dir.path().join("report.txt");
        let missing = dir.path().join("missing.txt");
        fs::write(&valid, "hello").expect("file");

        let attachments = inspect_attachment_paths(vec![
            valid.to_string_lossy().into_owned(),
            missing.to_string_lossy().into_owned(),
        ]);

        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].name, "report.txt");
        assert_eq!(attachments[0].kind, "file");
    }

    #[test]
    fn dedupes_mention_roots_using_platform_path_rules() {
        let normalized = normalize_roots(vec![
            "/tmp/Workspace".into(),
            "/tmp/workspace".into(),
            "/tmp/Workspace".into(),
        ]);

        if cfg!(any(target_os = "macos", target_os = "windows")) {
            assert_eq!(normalized, vec![PathBuf::from("/tmp/Workspace")]);
        } else {
            assert_eq!(
                normalized,
                vec![
                    PathBuf::from("/tmp/Workspace"),
                    PathBuf::from("/tmp/workspace")
                ]
            );
        }
    }

    #[test]
    fn rejects_oversized_image_attachment_payloads() {
        let dir = tempdir().expect("tempdir");
        let image = dir.path().join("huge.png");
        fs::write(
            &image,
            vec![0_u8; (MAX_IMAGE_ATTACHMENT_BYTES as usize) + 1],
        )
        .expect("oversized image file");

        let error = read_image_attachment(&image.to_string_lossy()).expect_err("size limit");

        assert!(error.contains("exceeds the 20 MB limit"));
    }

    #[test]
    fn write_file_creates_missing_parent_directories() {
        let dir = tempdir().expect("tempdir");
        let nested = dir.path().join("a/b/c/out.json");

        write_file(&nested.to_string_lossy(), "{\"ok\":true}").expect("write");

        let read_back = std::fs::read_to_string(&nested).expect("read back");
        assert_eq!(read_back, "{\"ok\":true}");
    }

    #[test]
    fn path_exists_reflects_disk_state() {
        let dir = tempdir().expect("tempdir");
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, "hi").expect("file");

        assert!(path_exists(&file.to_string_lossy()));
        assert!(!path_exists(
            &dir.path().join("missing.txt").to_string_lossy()
        ));
    }
}
