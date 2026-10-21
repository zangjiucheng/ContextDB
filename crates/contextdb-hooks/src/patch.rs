//! Paths touched by a Codex `apply_patch` (ADR 0010 §3.3): the patch text,
//! or a shell command that pipes a heredoc into `apply_patch`.

/// Paths named by `*** Add File:`, `*** Update File:`, `*** Delete File:`
/// and `*** Move to:` lines, in order, without duplicates.
pub fn paths(text: &str) -> Vec<String> {
    const HEADERS: [&str; 4] = [
        "*** Add File: ",
        "*** Update File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_start();
        for h in HEADERS {
            if let Some(p) = line.strip_prefix(h) {
                let p = p.trim_end();
                if !p.is_empty() && !out.iter().any(|o| o == p) {
                    out.push(p.to_owned());
                }
            }
        }
    }
    out
}
/// Whether a shell command carries an `apply_patch` invocation.
pub fn is_patch_command(command: &str) -> bool {
    command.contains("apply_patch") && command.contains("*** Begin Patch")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn headers_in_patches_and_heredocs() {
        let patch = "*** Begin Patch\n*** Add File: a/new.rs\n+fn x() {}\n*** Update File: b.rs\n@@\n-x\n+y\n*** Move to: c.rs\n*** Delete File: d.rs\n*** Update File: b.rs\n*** End Patch\n";
        assert_eq!(paths(patch), ["a/new.rs", "b.rs", "c.rs", "d.rs"]);
        let cmd = format!("apply_patch <<'EOF'\n{patch}EOF\n");
        assert!(is_patch_command(&cmd));
        assert_eq!(paths(&cmd), paths(patch));
        assert!(paths("echo hi").is_empty());
        assert!(paths("*** Add File: \n").is_empty());
        // A body line that merely contains a header is not one.
        assert!(paths("+let s = \"x *** Add File: y\";").is_empty());
        assert!(!is_patch_command("cargo test"));
    }
}
