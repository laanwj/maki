//! Git branch detection, shared by the single-process status bar and the
//! executor's `maki://git/branch` resource (what the split-mode status bar
//! reads instead; the brain has no `.git`).

use std::path::{Path, PathBuf};

/// URI the executor's builtin git plugin serves the workspace branch under.
pub const BRANCH_RESOURCE_URI: &str = "maki://git/branch";

/// The checked-out branch, or the short HEAD hash when detached. `None` when
/// no repository contains `cwd`.
pub fn detect_branch(cwd: &Path) -> Option<String> {
    let head = std::fs::read_to_string(find_git_dir(cwd)?.join("HEAD")).ok()?;
    let head = head.trim();
    head.strip_prefix("ref: refs/heads/")
        .map(str::to_string)
        .or_else(|| Some(head.get(..7)?.to_string()))
}

/// The nearest `.git` directory for `cwd`, walking up to the filesystem root.
/// A worktree's `.git` file is followed to the real gitdir.
pub fn find_git_dir(cwd: &Path) -> Option<PathBuf> {
    let mut dir = cwd;
    loop {
        let git = dir.join(".git");
        if git.is_dir() {
            return Some(git);
        }
        if let Ok(contents) = std::fs::read_to_string(&git) {
            let path = contents.trim().strip_prefix("gitdir: ")?.trim_start();
            let path = Path::new(path);
            return Some(if path.is_absolute() {
                path.to_path_buf()
            } else {
                dir.join(path)
            });
        }
        dir = dir.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use tempfile::TempDir;
    use test_case::test_case;

    fn tmp_with_head(content: Option<&str>) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        if let Some(head) = content {
            let git = dir.path().join(".git");
            fs::create_dir(&git).unwrap();
            fs::write(git.join("HEAD"), head).unwrap();
        }
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test_case(Some("ref: refs/heads/feature/foo\n"), Some("feature/foo") ; "regular_ref")]
    #[test_case(Some("abc1234deadbeef\n"),            Some("abc1234")      ; "detached_head")]
    #[test_case(None,                                 None                 ; "no_git_dir")]
    fn detect_branch_cases(head: Option<&str>, expected: Option<&str>) {
        let (_dir, path) = tmp_with_head(head);
        assert_eq!(detect_branch(&path).as_deref(), expected);
    }

    #[test]
    fn detect_branch_from_worktree() {
        let dir = TempDir::new().unwrap();
        let wt_head = dir.path().join("main/.git/worktrees/wt");
        fs::create_dir_all(&wt_head).unwrap();
        fs::write(dir.path().join("main/.git/HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(wt_head.join("HEAD"), "ref: refs/heads/db/worktree-branch\n").unwrap();
        let wt = dir.path().join("wt");
        fs::create_dir(&wt).unwrap();
        fs::write(wt.join(".git"), format!("gitdir: {}\n", wt_head.display())).unwrap();
        assert_eq!(detect_branch(&wt).as_deref(), Some("db/worktree-branch"));
    }

    #[test]
    fn detect_branch_from_subdirectory() {
        let (_dir, path) = tmp_with_head(Some("ref: refs/heads/main\n"));
        let sub = path.join("sub");
        fs::create_dir(&sub).unwrap();
        assert_eq!(detect_branch(&sub).as_deref(), Some("main"));
    }
}
