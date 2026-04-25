use super::*;
use std::fs;
use tempfile::TempDir;

// --- branch_to_prefix tests ---

#[test]
fn test_branch_to_prefix_feature_branch() {
    let result = branch_to_prefix("feature/auth");
    assert_eq!(
        result,
        Some(WorktreePrefix {
            prefix: "auth".to_string(),
            source: "git branch".to_string(),
        })
    );
}

#[test]
fn test_branch_to_prefix_nested_branch() {
    let result = branch_to_prefix("user/john/fix-login");
    assert_eq!(
        result,
        Some(WorktreePrefix {
            prefix: "fix-login".to_string(),
            source: "git branch".to_string(),
        })
    );
}

#[test]
fn test_branch_to_prefix_simple_branch() {
    let result = branch_to_prefix("develop");
    assert_eq!(
        result,
        Some(WorktreePrefix {
            prefix: "develop".to_string(),
            source: "git branch".to_string(),
        })
    );
}

#[test]
fn test_branch_to_prefix_main_returns_none() {
    assert_eq!(branch_to_prefix("main"), None);
}

#[test]
fn test_branch_to_prefix_master_returns_none() {
    assert_eq!(branch_to_prefix("master"), None);
}

#[test]
fn test_branch_to_prefix_head_returns_none() {
    assert_eq!(branch_to_prefix("HEAD"), None);
}

#[test]
fn test_branch_to_prefix_empty_returns_none() {
    assert_eq!(branch_to_prefix(""), None);
}

#[test]
fn test_branch_to_prefix_sanitizes_special_chars() {
    let result = branch_to_prefix("feature/my_cool_branch");
    assert_eq!(
        result,
        Some(WorktreePrefix {
            prefix: "my-cool-branch".to_string(),
            source: "git branch".to_string(),
        })
    );
}

// --- gitdir_is_worktree tests ---

#[test]
fn test_gitdir_is_worktree_valid() {
    assert!(gitdir_is_worktree(
        "/home/user/repo/.git/worktrees/feature-auth"
    ));
}

#[test]
fn test_gitdir_is_worktree_relative() {
    assert!(gitdir_is_worktree("../../repo/.git/worktrees/my-branch"));
}

#[test]
fn test_gitdir_is_worktree_submodule() {
    assert!(!gitdir_is_worktree("/home/user/repo/.git/modules/submod"));
}

#[test]
fn test_gitdir_is_worktree_no_worktrees() {
    assert!(!gitdir_is_worktree("/home/user/repo/.git"));
}

#[test]
fn test_gitdir_is_worktree_nested_slash_rejected() {
    assert!(!gitdir_is_worktree("/repo/.git/worktrees/name/extra"));
}

// --- read_branch_from_head tests ---

#[test]
fn test_read_branch_from_head_normal() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("HEAD"), "ref: refs/heads/feature/auth\n").unwrap();

    let branch = read_branch_from_head(tmp.path());
    assert_eq!(branch, Some("feature/auth".to_string()));
}

#[test]
fn test_read_branch_from_head_detached() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("HEAD"), "abc123def456\n").unwrap();

    let branch = read_branch_from_head(tmp.path());
    assert_eq!(branch, None);
}

#[test]
fn test_read_branch_from_head_missing_file() {
    let tmp = TempDir::new().unwrap();
    let branch = read_branch_from_head(tmp.path());
    assert_eq!(branch, None);
}

// --- detect_worktree_via_filesystem tests ---

#[test]
fn test_filesystem_regular_git_dir_returns_none() {
    let tmp = TempDir::new().unwrap();
    fs::create_dir(tmp.path().join(".git")).unwrap();

    let result = detect_worktree_via_filesystem(tmp.path());
    assert_eq!(result, None);
}

#[test]
fn test_filesystem_worktree_git_file() {
    let tmp = TempDir::new().unwrap();

    let main_git = tmp.path().join("main-repo").join(".git");
    fs::create_dir_all(&main_git).unwrap();
    let worktree_gitdir = main_git.join("worktrees").join("feature-auth");
    fs::create_dir_all(&worktree_gitdir).unwrap();
    fs::write(
        worktree_gitdir.join("HEAD"),
        "ref: refs/heads/feature/auth\n",
    )
    .unwrap();

    let worktree_dir = tmp.path().join("worktree-checkout");
    fs::create_dir_all(&worktree_dir).unwrap();
    fs::write(
        worktree_dir.join(".git"),
        format!("gitdir: {}", worktree_gitdir.display()),
    )
    .unwrap();

    let result = detect_worktree_via_filesystem(&worktree_dir);
    assert_eq!(
        result,
        Some(WorktreePrefix {
            prefix: "auth".to_string(),
            source: "git branch".to_string(),
        })
    );
}

#[test]
fn test_filesystem_submodule_git_file_returns_none() {
    let tmp = TempDir::new().unwrap();

    let main_git = tmp.path().join("main-repo").join(".git");
    let submodule_gitdir = main_git.join("modules").join("my-sub");
    fs::create_dir_all(&submodule_gitdir).unwrap();

    let submodule_dir = tmp.path().join("submodule-checkout");
    fs::create_dir_all(&submodule_dir).unwrap();
    fs::write(
        submodule_dir.join(".git"),
        format!("gitdir: {}", submodule_gitdir.display()),
    )
    .unwrap();

    let result = detect_worktree_via_filesystem(&submodule_dir);
    assert_eq!(result, None);
}

#[test]
fn test_filesystem_main_branch_returns_none() {
    let tmp = TempDir::new().unwrap();

    let main_git = tmp.path().join("main-repo").join(".git");
    let worktree_gitdir = main_git.join("worktrees").join("main-wt");
    fs::create_dir_all(&worktree_gitdir).unwrap();
    fs::write(worktree_gitdir.join("HEAD"), "ref: refs/heads/main\n").unwrap();

    let worktree_dir = tmp.path().join("worktree-main");
    fs::create_dir_all(&worktree_dir).unwrap();
    fs::write(
        worktree_dir.join(".git"),
        format!("gitdir: {}", worktree_gitdir.display()),
    )
    .unwrap();

    let result = detect_worktree_via_filesystem(&worktree_dir);
    assert_eq!(result, None);
}

#[test]
fn test_filesystem_walks_up() {
    let tmp = TempDir::new().unwrap();

    let main_git = tmp.path().join("main-repo").join(".git");
    let worktree_gitdir = main_git.join("worktrees").join("dev-wt");
    fs::create_dir_all(&worktree_gitdir).unwrap();
    fs::write(worktree_gitdir.join("HEAD"), "ref: refs/heads/develop\n").unwrap();

    let worktree_root = tmp.path().join("worktree-dev");
    fs::create_dir_all(&worktree_root).unwrap();
    fs::write(
        worktree_root.join(".git"),
        format!("gitdir: {}", worktree_gitdir.display()),
    )
    .unwrap();

    let nested = worktree_root.join("src").join("lib");
    fs::create_dir_all(&nested).unwrap();

    let result = detect_worktree_via_filesystem(&nested);
    assert_eq!(
        result,
        Some(WorktreePrefix {
            prefix: "develop".to_string(),
            source: "git branch".to_string(),
        })
    );
}

#[test]
fn test_filesystem_no_git_returns_none() {
    let tmp = TempDir::new().unwrap();
    let nested = tmp.path().join("a").join("b");
    fs::create_dir_all(&nested).unwrap();

    let result = detect_worktree_via_filesystem(&nested);
    assert_eq!(result, None);
}

// --- detect_worktree_prefix integration (CLI path) ---

#[test]
fn test_detect_worktree_prefix_single_worktree_returns_none() {
    let tmp = TempDir::new().unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();

    let result = detect_worktree_prefix(tmp.path());
    assert_eq!(result, None);
}
