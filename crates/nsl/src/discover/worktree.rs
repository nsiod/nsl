use std::fs;
use std::path::{Path, PathBuf};

use crate::utils::sanitize_for_hostname;

/// Result of worktree prefix detection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreePrefix {
    /// Sanitized branch name segment to prepend to the project hostname.
    pub prefix: String,
    /// How the prefix was detected (e.g. "git branch").
    pub source: String,
}

/// Branch names that represent the default/primary checkout -- no prefix needed.
const DEFAULT_BRANCHES: &[&str] = &["main", "master"];

/// Detect if the current directory is inside a multi-worktree git repo and
/// return the current branch name as a prefix for hostname composition.
///
/// Primary path: uses `git worktree list --porcelain` + `git rev-parse --abbrev-ref HEAD`.
/// Fallback path: parses `.git` file and HEAD when git CLI is unavailable.
///
/// Returns `None` when:
/// - Not in a worktree setup (single worktree)
/// - On a default branch (main/master)
/// - Git is unavailable and no `.git` file found
pub fn detect_worktree_prefix(cwd: &Path) -> Option<WorktreePrefix> {
    match detect_worktree_via_cli(cwd) {
        Some(result) => result,
        None => detect_worktree_via_filesystem(cwd),
    }
}

/// Use git CLI to detect worktree prefix.
fn detect_worktree_via_cli(cwd: &Path) -> Option<Option<WorktreePrefix>> {
    let list_output = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !list_output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&list_output.stdout);

    let worktree_count = stdout
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .count();

    if worktree_count <= 1 {
        return Some(None);
    }

    let branch_output = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !branch_output.status.success() {
        return Some(None);
    }

    let branch = String::from_utf8_lossy(&branch_output.stdout)
        .trim()
        .to_string();

    Some(branch_to_prefix(&branch))
}

/// Fallback worktree detection when git CLI is unavailable.
fn detect_worktree_via_filesystem(start_dir: &Path) -> Option<WorktreePrefix> {
    let mut dir = start_dir.to_path_buf();
    loop {
        let git_path = dir.join(".git");
        if let Ok(metadata) = fs::metadata(&git_path) {
            if metadata.is_dir() {
                return None;
            }
            if metadata.is_file() {
                let content = fs::read_to_string(&git_path).ok()?.trim().to_string();
                let gitdir = content.strip_prefix("gitdir: ")?;

                if !gitdir_is_worktree(gitdir) {
                    return None;
                }

                let resolved_gitdir = if Path::new(gitdir).is_absolute() {
                    PathBuf::from(gitdir)
                } else {
                    dir.join(gitdir)
                };

                let branch = read_branch_from_head(&resolved_gitdir)?;
                return branch_to_prefix(&branch);
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Check if a gitdir path points to a worktree (contains `/worktrees/`).
fn gitdir_is_worktree(gitdir: &str) -> bool {
    let normalized = gitdir.replace('\\', "/");
    if let Some(pos) = normalized.rfind("/worktrees/") {
        let after = &normalized[pos + "/worktrees/".len()..];
        !after.is_empty() && !after.contains('/')
    } else {
        false
    }
}

/// Read the current branch name from a gitdir's HEAD file.
fn read_branch_from_head(gitdir: &Path) -> Option<String> {
    let head_content = fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let head = head_content.trim();
    let branch = head.strip_prefix("ref: refs/heads/")?;
    if branch.is_empty() {
        None
    } else {
        Some(branch.to_string())
    }
}

/// Convert a branch name to a worktree prefix.
fn branch_to_prefix(branch: &str) -> Option<WorktreePrefix> {
    if branch.is_empty() || branch == "HEAD" || DEFAULT_BRANCHES.contains(&branch) {
        return None;
    }
    let last_segment = branch.rsplit('/').next().unwrap_or(branch);
    let prefix = sanitize_for_hostname(last_segment);
    if prefix.is_empty() {
        return None;
    }
    Some(WorktreePrefix {
        prefix,
        source: "git branch".to_string(),
    })
}

#[cfg(test)]
#[path = "worktree_tests.rs"]
mod tests;
