//! The checked-out git branch of a checkout, read from `HEAD` without
//! running git: sessions and the observer bind to the ContextDB branch of
//! the same name (ADR 0013 §7, D1).
use std::path::Path;

/// The branch `dir`'s checkout has checked out: `None` when detached, not a
/// git checkout, or unreadable. `dir/.git` is a directory, or (a linked
/// worktree or submodule) a file naming the git directory.
pub fn checked_out_branch(dir: &Path) -> Option<String> {
    let dot_git = dir.join(".git");
    let git_dir = if dot_git.is_dir() {
        dot_git
    } else {
        let text = std::fs::read_to_string(&dot_git).ok()?;
        let target = text.strip_prefix("gitdir:")?.trim();
        let target = Path::new(target);
        if target.is_absolute() {
            target.to_path_buf()
        } else {
            dir.join(target)
        }
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    let name = head.trim().strip_prefix("ref: refs/heads/")?;
    (!name.is_empty()).then(|| name.to_owned())
}

/// As [`checked_out_branch`] for the nearest checkout at or above `dir`
/// (a working directory inside a repository).
pub fn checked_out_branch_above(dir: &Path) -> Option<String> {
    let top = dir.ancestors().find(|d| d.join(".git").exists())?;
    checked_out_branch(top)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_branches_of_checkouts_and_worktrees() {
        let root = std::env::temp_dir().join(format!("ctxdb-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let main = root.join("main");
        std::fs::create_dir_all(main.join(".git/worktrees/w")).unwrap();
        std::fs::write(main.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        assert_eq!(checked_out_branch(&main).as_deref(), Some("feature/x"));
        std::fs::create_dir_all(main.join("src/deep")).unwrap();
        assert_eq!(
            checked_out_branch_above(&main.join("src/deep")).as_deref(),
            Some("feature/x")
        );
        // A linked worktree: `.git` is a file naming its git directory.
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let gitdir = main.join(".git/worktrees/w");
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", gitdir.display())).unwrap();
        std::fs::write(gitdir.join("HEAD"), "ref: refs/heads/topic\n").unwrap();
        assert_eq!(checked_out_branch(&wt).as_deref(), Some("topic"));
        // Detached, and not a checkout.
        std::fs::write(
            gitdir.join("HEAD"),
            "0123456789abcdef0123456789abcdef01234567\n",
        )
        .unwrap();
        assert_eq!(checked_out_branch(&wt), None);
        assert_eq!(checked_out_branch(&root), None);
        let _ = std::fs::remove_dir_all(&root);
    }
}
