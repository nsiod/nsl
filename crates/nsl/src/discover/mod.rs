mod worktree;

pub use worktree::detect_worktree_prefix;

use std::fs;
use std::path::Path;

use crate::utils::sanitize_for_hostname;

/// Infer a project name from the current directory, including worktree prefix
/// when in a multi-worktree git repo on a non-default branch.
///
/// Priority:
/// 1. package.json "name" field (walk up from cwd)
/// 2. Git repo root directory name
/// 3. Current directory name
///
/// If a worktree prefix is detected, the result becomes `<prefix>-<name>`.
pub fn infer_project_name(cwd: &Path) -> String {
    let base_name = infer_base_project_name(cwd);
    if let Some(wt) = detect_worktree_prefix(cwd) {
        format!("{}-{}", wt.prefix, base_name)
    } else {
        base_name
    }
}

/// Infer the base project name without worktree prefix.
fn infer_base_project_name(cwd: &Path) -> String {
    if let Some(name) = name_from_package_json(cwd) {
        return sanitize_for_hostname(&name);
    }
    if let Some(name) = name_from_git_root(cwd) {
        return sanitize_for_hostname(&name);
    }
    let dir_name = cwd.file_name().and_then(|n| n.to_str()).unwrap_or("app");
    sanitize_for_hostname(dir_name)
}

/// Walk up from `start` looking for package.json and extract "name".
fn name_from_package_json(start: &Path) -> Option<String> {
    let mut dir = start;
    loop {
        let candidate = dir.join("package.json");
        if candidate.is_file()
            && let Ok(content) = fs::read_to_string(&candidate)
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
            && let Some(name) = json.get("name").and_then(|v| v.as_str())
        {
            let clean = name
                .strip_prefix('@')
                .and_then(|s| s.split_once('/'))
                .map(|(_, n)| n)
                .unwrap_or(name);
            if !clean.is_empty() {
                return Some(clean.to_string());
            }
        }
        dir = dir.parent()?;
    }
}

/// Try to get the git repo root directory name.
fn name_from_git_root(start: &Path) -> Option<String> {
    if let Ok(output) = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()
        && output.status.success()
    {
        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let root_path = std::path::PathBuf::from(&root);
        if let Some(name) = root_path.file_name().and_then(|n| n.to_str()) {
            return Some(name.to_string());
        }
    }

    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return dir.file_name().and_then(|n| n.to_str()).map(String::from);
        }
        dir = dir.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_infer_from_package_json() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("package.json"),
            r#"{"name": "my-cool-app"}"#,
        )
        .unwrap();

        let name = infer_project_name(tmp.path());
        assert_eq!(name, "my-cool-app");
    }

    #[test]
    fn test_infer_from_scoped_package_json() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("package.json"),
            r#"{"name": "@scope/my-pkg"}"#,
        )
        .unwrap();

        let name = infer_project_name(tmp.path());
        assert_eq!(name, "my-pkg");
    }

    #[test]
    fn test_infer_from_nested_dir() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("packages").join("web");
        fs::create_dir_all(&sub).unwrap();
        fs::write(tmp.path().join("package.json"), r#"{"name": "monorepo"}"#).unwrap();

        let name = infer_project_name(&sub);
        assert_eq!(name, "monorepo");
    }

    #[test]
    fn test_infer_fallback_to_dir_name() {
        let tmp = TempDir::new().unwrap();
        let name = infer_project_name(tmp.path());
        assert!(!name.is_empty());
    }
}
