//! Path normalisation (ADR 0010 §3.2): repository-relative paths, lexical
//! and idempotent; paths outside the repository stay absolute.
use std::path::{Component, Path, PathBuf};

/// Lexical normalisation: removes `.` and resolves `..` without touching the
/// filesystem (a path may not exist yet, or be about to be deleted).
pub fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}
/// `path` (absolute, or relative to `cwd`) relative to `root`; absolute when
/// it lies outside `root`.
pub fn normalize(root: &Path, cwd: &Path, path: &str) -> String {
    let p = Path::new(path);
    let abs = lexical(&if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    });
    match abs.strip_prefix(lexical(root)) {
        Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => abs.to_string_lossy().into_owned(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_inside_absolute_outside_and_idempotent() {
        let root = Path::new("/r/repo");
        let cwd = Path::new("/r/repo/src");
        assert_eq!(normalize(root, cwd, "/r/repo/src/a.rs"), "src/a.rs");
        assert_eq!(normalize(root, cwd, "b.rs"), "src/b.rs");
        assert_eq!(normalize(root, cwd, "../docs/./x.md"), "docs/x.md");
        assert_eq!(normalize(root, cwd, "/etc/hosts"), "/etc/hosts");
        assert_eq!(normalize(root, cwd, "../../other/y"), "/r/other/y");
        assert_eq!(normalize(root, Path::new("/r/repo2"), "z"), "/r/repo2/z");
        for p in ["/r/repo/src/a.rs", "b.rs", "../docs/./x.md", "/etc/hosts"] {
            let once = normalize(root, cwd, p);
            assert_eq!(normalize(root, root, &once), once, "{p}");
        }
    }
}
