//! The About line: the launcher's version and the git commit it was built from (embedded by
//! `build.rs`).

/// The crate version, from Cargo.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The short commit hash of the build, `unknown` outside a git checkout.
pub const COMMIT: &str = env!("ANYXBOXX_GIT_COMMIT");

/// `anyXboxx-diffbase launcher 0.1.0, commit 1a2b3c4d`.
pub fn line() -> String {
    format!("anyXboxx-diffbase launcher {VERSION}, commit {COMMIT}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_about_line_names_the_version_and_the_commit() {
        assert!(!VERSION.is_empty() && !COMMIT.is_empty());
        let line = line();
        assert!(line.contains(VERSION) && line.contains(COMMIT));
        assert!(COMMIT == "unknown" || COMMIT.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
