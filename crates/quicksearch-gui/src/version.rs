//! Which build this is: the release version and the commit it came from,
//! both compile-time literals.

/// The one string the user sees: `v0.9.1 (a46cbc2)`.
///
/// The version half is `[workspace.package] version` — the same value the
/// `.deb`, the Windows installer and the release tag carry. The commit half
/// is the short hash `build.rs` resolved, or `unknown` for a build made
/// outside a git checkout.
pub const BUILD_ID: &str = concat!("v", env!("CARGO_PKG_VERSION"), " (", env!("QS_COMMIT"), ")");

/// Hover text wherever [`BUILD_ID`] is shown.
pub const BUILD_ID_HINT: &str = "QuickSearch version and the commit it was built from";

#[cfg(test)]
mod tests {
    use super::*;

    /// Take the build id apart the way someone reading a bug report does.
    fn halves() -> (&'static str, &'static str) {
        let rest = BUILD_ID
            .strip_prefix('v')
            .unwrap_or_else(|| panic!("{BUILD_ID:?} should lead with a v, like the release tags"));
        let (version, commit) = rest
            .split_once(" (")
            .unwrap_or_else(|| panic!("{BUILD_ID:?} should be a version then a commit"));
        let commit = commit
            .strip_suffix(')')
            .unwrap_or_else(|| panic!("{BUILD_ID:?} has an unclosed commit"));
        (version, commit)
    }

    /// A screenshot of the status bar identifies a build, so the version
    /// half has to be the release version exactly.
    #[test]
    fn the_build_id_names_the_release_version() {
        let (version, _) = halves();
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
    }

    /// `unknown` is the documented fallback for a build with no git and no
    /// `QS_COMMIT`; anything else has to be an abbreviated lowercase hash.
    #[test]
    fn the_build_id_names_a_short_commit() {
        let (_, commit) = halves();
        if commit == "unknown" {
            return;
        }
        assert!(
            (1..=7).contains(&commit.len()),
            "commit {commit:?} is not an abbreviated hash"
        );
        assert!(
            commit.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')),
            "commit {commit:?} is not lowercase hex"
        );
    }
}
