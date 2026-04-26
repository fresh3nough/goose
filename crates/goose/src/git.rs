//! Git helpers used by ACP custom methods that the goose2 desktop client invokes.
//!
//! These shell out to the system `git` binary so that operations match what the
//! user would see on the command line. This module is intentionally pure Rust:
//! it has no Tauri or transport dependencies and is reused by
//! `crates/goose/src/acp/server.rs` to back the `_goose/git/*` ACP methods.
//!
//! Wire data types live in `goose_sdk::custom_requests` so they're shared with
//! the generated TypeScript SDK.

use anyhow::{anyhow, bail, Context, Result};
use goose_sdk::custom_requests::{ChangedFile, CreatedWorktree, GitState, WorktreeInfo};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn get_git_state(path: &str) -> Result<GitState> {
    let repo_path = PathBuf::from(path);
    if !repo_path.exists() {
        bail!("Path does not exist: {}", path);
    }

    if !is_git_repo(&repo_path)? {
        return Ok(GitState::default());
    }

    let current_root = trim_to_option(run_git_success(
        &repo_path,
        &["rev-parse", "--show-toplevel"],
    )?)
    .ok_or_else(|| anyhow!("Could not determine repository root"))?;
    let current_branch =
        trim_to_option(run_git_success(&repo_path, &["branch", "--show-current"])?);
    let dirty_file_count = count_lines(&run_git_success(&repo_path, &["status", "--porcelain"])?);
    let git_common_dir = trim_to_option(run_git_success(
        &repo_path,
        &["rev-parse", "--git-common-dir"],
    )?);
    let main_worktree_path = git_common_dir
        .as_deref()
        .and_then(|git_common_dir| resolve_main_worktree_path(git_common_dir, &current_root))
        .as_deref()
        .map(normalize_path_string);
    let worktrees_output = run_git_success(&repo_path, &["worktree", "list", "--porcelain"])?;
    let worktrees = parse_worktrees(&worktrees_output, main_worktree_path.as_deref());
    let is_worktree = main_worktree_path
        .as_deref()
        .map(|main_path| normalize_path_string(&current_root) != main_path)
        .unwrap_or(false);
    let incoming_commit_count = count_incoming_commits(&repo_path).unwrap_or(0);
    let local_branches = list_local_branches(&repo_path).unwrap_or_default();

    Ok(GitState {
        is_git_repo: true,
        current_branch,
        dirty_file_count,
        incoming_commit_count,
        worktrees,
        is_worktree,
        main_worktree_path,
        local_branches,
    })
}

pub fn switch_branch(path: &str, branch: &str) -> Result<()> {
    let repo_path = resolve_repo_path(path)?;
    run_git_success(&repo_path, &["switch", branch])?;
    Ok(())
}

pub fn stash(path: &str) -> Result<()> {
    let repo_path = resolve_repo_path(path)?;
    run_git_success(&repo_path, &["stash"])?;
    Ok(())
}

pub fn init(path: &str) -> Result<()> {
    let repo_path = resolve_repo_path(path)?;
    run_git_success(&repo_path, &["init"])?;
    Ok(())
}

pub fn fetch(path: &str) -> Result<()> {
    let repo_path = resolve_repo_path(path)?;
    run_git_success(&repo_path, &["fetch", "--prune"])?;
    Ok(())
}

pub fn pull(path: &str) -> Result<()> {
    let repo_path = resolve_repo_path(path)?;
    run_git_success(&repo_path, &["pull", "--ff-only"])?;
    Ok(())
}

pub fn create_branch(path: &str, name: &str, base_branch: &str) -> Result<()> {
    let repo_path = resolve_repo_path(path)?;
    let branch_name = require_nonempty(name, "Branch name")?;
    let base_branch = require_nonempty(base_branch, "Base branch")?;
    run_git_success(
        &repo_path,
        &["switch", "-c", branch_name.as_str(), base_branch.as_str()],
    )?;
    Ok(())
}

pub fn create_worktree(
    path: &str,
    name: &str,
    branch: &str,
    create_branch: bool,
    base_branch: Option<&str>,
) -> Result<CreatedWorktree> {
    let repo_path = resolve_repo_path(path)?;
    let worktree_name = validate_worktree_name(name)?;
    let branch_name = require_nonempty(branch, "Branch name")?;
    let (_, main_worktree_path) = git_repo_context(&repo_path)?;
    let target_path = derive_worktree_path(
        main_worktree_path.as_deref().unwrap_or(path),
        &worktree_name,
    )?;

    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create worktree directory")?;
    }

    let target_path_string = target_path.to_string_lossy().to_string();

    if create_branch {
        let base_branch = require_nonempty(base_branch.unwrap_or_default(), "Base branch")?;
        run_git_success(
            &repo_path,
            &[
                "worktree",
                "add",
                "-b",
                branch_name.as_str(),
                target_path_string.as_str(),
                base_branch.as_str(),
            ],
        )?;
    } else {
        run_git_success(
            &repo_path,
            &[
                "worktree",
                "add",
                target_path_string.as_str(),
                branch_name.as_str(),
            ],
        )?;
    }

    Ok(CreatedWorktree {
        path: normalize_path_string(&target_path_string),
        branch: branch_name,
    })
}

pub fn get_changed_files(path: &str) -> Result<Vec<ChangedFile>> {
    let repo_path = resolve_repo_path(path)?;

    if !is_git_repo(&repo_path)? {
        return Ok(Vec::new());
    }

    let status_output = run_git_success(
        &repo_path,
        &["status", "--porcelain", "--untracked-files=all"],
    )?;
    if status_output.trim().is_empty() {
        return Ok(Vec::new());
    }

    let head_numstat =
        run_git_success(&repo_path, &["diff", "HEAD", "--numstat"]).unwrap_or_default();
    let head_stats = parse_numstat(&head_numstat);

    let mut files: Vec<ChangedFile> = Vec::new();

    for line in status_output.lines() {
        if line.len() < 4 {
            continue;
        }

        let index_status = line.as_bytes()[0];
        let worktree_status = line.as_bytes()[1];
        let file_path = unquote_porcelain(line[3..].trim());
        let file_path = if file_path.contains(" -> ") {
            file_path
                .split(" -> ")
                .last()
                .unwrap_or(&file_path)
                .to_string()
        } else {
            file_path
        };

        let status = parse_status_codes(index_status, worktree_status);

        let (additions, deletions) = head_stats
            .get(&file_path)
            .copied()
            .unwrap_or_else(|| count_file_lines(&repo_path, &file_path));

        files.push(ChangedFile {
            path: file_path,
            status,
            additions,
            deletions,
        });
    }

    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

fn is_git_repo(path: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .current_dir(path)
        .output()
        .context("Failed to run git")?;

    Ok(output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true")
}

fn resolve_repo_path(path: &str) -> Result<PathBuf> {
    let repo_path = PathBuf::from(path);
    if !repo_path.exists() {
        bail!("Path does not exist: {}", path);
    }
    Ok(repo_path)
}

fn run_git_success(path: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .context("Failed to run git")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if !stderr.is_empty() { stderr } else { stdout };
        let rendered_args = args.join(" ");
        bail!("git {} failed: {}", rendered_args, message);
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn trim_to_option(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn require_nonempty(value: &str, label: &str) -> Result<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        bail!("{} cannot be empty", label);
    }
    Ok(trimmed.to_string())
}

fn count_lines(value: &str) -> u32 {
    value
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count()
        .try_into()
        .unwrap_or(u32::MAX)
}

fn count_incoming_commits(path: &Path) -> Result<u32> {
    let has_upstream = Command::new("git")
        .args([
            "rev-parse",
            "--abbrev-ref",
            "--symbolic-full-name",
            "@{upstream}",
        ])
        .current_dir(path)
        .output()
        .context("Failed to run git")?;

    if !has_upstream.status.success() {
        return Ok(0);
    }

    let output = run_git_success(path, &["rev-list", "--count", "HEAD..@{upstream}"])?;
    let count = output
        .trim()
        .parse::<u32>()
        .context("Failed to parse incoming commit count")?;
    Ok(count)
}

fn resolve_main_worktree_path(git_common_dir: &str, current_root: &str) -> Option<String> {
    let path = PathBuf::from(git_common_dir);
    let absolute = if path.is_absolute() {
        path
    } else {
        PathBuf::from(current_root).join(path)
    };

    if absolute.file_name().is_some_and(|name| name == ".git") {
        absolute
            .parent()
            .map(|parent| parent.to_string_lossy().into_owned())
    } else {
        None
    }
}

fn git_repo_context(path: &Path) -> Result<(String, Option<String>)> {
    let current_root = trim_to_option(run_git_success(path, &["rev-parse", "--show-toplevel"])?)
        .ok_or_else(|| anyhow!("Could not determine repository root"))?;
    let git_common_dir = trim_to_option(run_git_success(path, &["rev-parse", "--git-common-dir"])?);
    let main_worktree_path = git_common_dir
        .as_deref()
        .and_then(|git_common_dir| resolve_main_worktree_path(git_common_dir, &current_root))
        .as_deref()
        .map(normalize_path_string);

    Ok((current_root, main_worktree_path))
}

fn validate_worktree_name(value: &str) -> Result<String> {
    let worktree_name = require_nonempty(value, "Worktree name")?;
    if worktree_name == "." || worktree_name == ".." {
        bail!("Worktree name must be a real folder name");
    }
    if worktree_name.contains('/') || worktree_name.contains('\\') {
        bail!("Worktree name cannot contain path separators");
    }
    Ok(worktree_name)
}

fn derive_worktree_path(main_worktree_path: &str, worktree_name: &str) -> Result<PathBuf> {
    let main_root = PathBuf::from(main_worktree_path);
    let repo_name = main_root
        .file_name()
        .ok_or_else(|| anyhow!("Could not determine repository name"))?
        .to_string_lossy()
        .to_string();
    let repo_parent = main_root
        .parent()
        .ok_or_else(|| anyhow!("Could not determine repository parent"))?;
    let target_path = repo_parent
        .join(format!("{}-worktrees", repo_name))
        .join(worktree_name);

    if target_path.exists() {
        bail!(
            "Worktree path already exists: {}",
            target_path.to_string_lossy()
        );
    }

    Ok(target_path)
}

fn parse_worktrees(output: &str, main_worktree_path: Option<&str>) -> Vec<WorktreeInfo> {
    let mut worktrees = Vec::new();
    let mut current_path: Option<String> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(path) = current_path.take() {
                worktrees.push(build_worktree(
                    path,
                    current_branch.take(),
                    main_worktree_path,
                ));
            }
            current_path = Some(path.to_string());
            current_branch = None;
            continue;
        }

        if let Some(branch) = line.strip_prefix("branch ") {
            current_branch = Some(branch_name(branch));
        }
    }

    if let Some(path) = current_path {
        worktrees.push(build_worktree(path, current_branch, main_worktree_path));
    }

    worktrees
}

fn build_worktree(
    path: String,
    branch: Option<String>,
    main_worktree_path: Option<&str>,
) -> WorktreeInfo {
    let normalized_path = normalize_path_string(&path);
    let is_main = main_worktree_path
        .map(|main_path| normalized_path == main_path)
        .unwrap_or(false);

    WorktreeInfo {
        path: normalized_path,
        branch,
        is_main,
    }
}

fn branch_name(branch_ref: &str) -> String {
    branch_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(branch_ref)
        .to_string()
}

fn normalize_path_string(path: &str) -> String {
    path.replace('\\', "/").trim_end_matches('/').to_string()
}

fn list_local_branches(path: &Path) -> Result<Vec<String>> {
    let output = run_git_success(
        path,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname:short)",
            "refs/heads",
        ],
    )?;
    Ok(output
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

fn parse_status_codes(index: u8, worktree: u8) -> String {
    if index == b'?' && worktree == b'?' {
        return "untracked".to_string();
    }
    if index == b'A' || (index == b'?' && worktree != b'?') {
        return "added".to_string();
    }
    if index == b'D' || worktree == b'D' {
        return "deleted".to_string();
    }
    if index == b'R' {
        return "renamed".to_string();
    }
    if index == b'C' {
        return "copied".to_string();
    }
    "modified".to_string()
}

fn parse_numstat(output: &str) -> std::collections::HashMap<String, (u32, u32)> {
    let mut map = std::collections::HashMap::new();
    for line in output.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            let additions = parts[0].parse::<u32>().unwrap_or(0);
            let deletions = parts[1].parse::<u32>().unwrap_or(0);
            let path = parts[2..].join("\t");
            let path = expand_rename_path(&path);
            map.insert(path, (additions, deletions));
        }
    }
    map
}

fn unquote_porcelain(s: &str) -> String {
    if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

fn expand_rename_path(path: &str) -> String {
    if let Some(brace_start) = path.find('{') {
        if let Some(brace_end) = path.find('}') {
            let prefix = &path[..brace_start];
            let inner = &path[brace_start + 1..brace_end];
            let suffix = &path[brace_end + 1..];
            let new_name = inner.split(" => ").last().unwrap_or(inner);
            return format!("{}{}{}", prefix, new_name, suffix);
        }
    }
    if path.contains(" => ") {
        path.split(" => ").last().unwrap_or(path).to_string()
    } else {
        path.to_string()
    }
}

const MAX_LINE_COUNT_SIZE: u64 = 1024 * 1024;

fn count_file_lines(repo_path: &Path, file_path: &str) -> (u32, u32) {
    let full = repo_path.join(file_path);
    let meta = match std::fs::metadata(&full) {
        Ok(m) => m,
        Err(_) => return (0, 0),
    };
    if meta.len() > MAX_LINE_COUNT_SIZE {
        return (0, 0);
    }
    match std::fs::read_to_string(&full) {
        Ok(contents) => {
            let count = contents.lines().count() as u32;
            (count, 0)
        }
        Err(_) => (0, 0),
    }
}
