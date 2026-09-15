//! How the extension names, finds and caches the companion binary.
//!
//! The extension itself compiles to `wasm32-wasip2` against `zed_extension_api`,
//! which cannot be unit-tested on the host. Everything here is plain string and
//! path logic with no dependency on that API, so it can be.

/// Operating systems the release workflow builds for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Mac,
    Linux,
    Windows,
}

/// CPU architectures the release workflow builds for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Aarch64,
    X86_64,
    X86,
}

/// Release asset name for a platform, without a version suffix.
///
/// These strings are a contract with `.github/workflows/release.yml`: the
/// extension looks an asset up by exact name, so a rename on either side
/// silently breaks installation on that platform. A test below reads the
/// workflow and checks the two agree.
pub fn asset_name(os: Os, arch: Arch) -> Result<&'static str, String> {
    match (os, arch) {
        (Os::Mac, Arch::Aarch64) => Ok("zed-claude-ide-server-macos-aarch64"),
        (Os::Mac, Arch::X86_64) => Ok("zed-claude-ide-server-macos-x86_64"),
        (Os::Linux, Arch::X86_64) => Ok("zed-claude-ide-server-linux-x86_64"),
        (Os::Linux, Arch::Aarch64) => Ok("zed-claude-ide-server-linux-aarch64"),
        (Os::Windows, _) => Err("Windows is not currently supported".to_string()),
        (os, arch) => Err(format!("Unsupported platform: {os:?}-{arch:?}")),
    }
}

/// Suffix of a download in progress. A file with this suffix is never a binary
/// to run, however much the rest of its name looks like one.
pub const DOWNLOAD_SUFFIX: &str = ".downloading";

/// Name a downloaded release is stored under: the asset name plus its version,
/// e.g. `zed-claude-ide-server-macos-aarch64-v0.1.0`.
pub fn versioned_name(prefix: &str, version: &str) -> String {
    format!("{prefix}-{version}")
}

/// Where a download is written before it is renamed into place.
pub fn download_temp_name(versioned: &str) -> String {
    format!("{versioned}{DOWNLOAD_SUFFIX}")
}

/// Whether `name` is a versioned build of `prefix` that is safe to run.
pub fn is_versioned_binary(prefix: &str, name: &str) -> bool {
    match name.strip_prefix(prefix) {
        // A "-v" suffix alone used to be enough, which also matched
        // "<prefix>-v0.1.0.downloading": a download interrupted by Zed quitting
        // was picked up and run as the server on the next start.
        Some(suffix) => suffix.starts_with("-v") && !suffix.ends_with(DOWNLOAD_SUFFIX),
        None => false,
    }
}

/// Binaries for `prefix` among the entries of the extension work directory:
/// the legacy unversioned name if present, then every versioned build.
pub fn existing_binaries(prefix: &str, names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut found = Vec::new();
    for name in names {
        if name == prefix || is_versioned_binary(prefix, &name) {
            found.push(name);
        }
    }
    found
}

/// The build to fall back on when a download is impossible: the newest
/// versioned one if any, otherwise the legacy unversioned binary.
pub fn fallback_binary(prefix: &str, names: impl IntoIterator<Item = String>) -> Option<String> {
    let mut candidates = existing_binaries(prefix, names);
    candidates.sort();
    candidates
        .iter()
        .rev()
        .find(|n| is_versioned_binary(prefix, n))
        .cloned()
        .or_else(|| candidates.into_iter().find(|n| n == prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PREFIX: &str = "zed-claude-ide-server-macos-aarch64";

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn asset_names_match_the_release_workflow() {
        let workflow = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../.github/workflows/release.yml"
        ))
        .expect("release workflow");
        let mut published: Vec<&str> = workflow
            .lines()
            .filter_map(|l| l.trim().strip_prefix("asset:"))
            .map(str::trim)
            .collect();
        published.sort_unstable();

        let mut ours: Vec<&str> = [
            (Os::Mac, Arch::Aarch64),
            (Os::Mac, Arch::X86_64),
            (Os::Linux, Arch::X86_64),
            (Os::Linux, Arch::Aarch64),
        ]
        .into_iter()
        .map(|(os, arch)| asset_name(os, arch).unwrap())
        .collect();
        ours.sort_unstable();

        assert_eq!(
            ours, published,
            "the extension looks assets up by exact name; a mismatch breaks install on that platform"
        );
    }

    #[test]
    fn windows_and_unknown_platforms_are_refused_with_a_reason() {
        assert!(asset_name(Os::Windows, Arch::X86_64).is_err());
        assert!(asset_name(Os::Linux, Arch::X86).is_err());
    }

    #[test]
    fn an_interrupted_download_is_never_treated_as_a_binary() {
        let versioned = versioned_name(PREFIX, "v0.1.0");
        let temp = download_temp_name(&versioned);
        assert!(
            !is_versioned_binary(PREFIX, &temp),
            "{temp} is a truncated download, not something to run"
        );
        assert!(existing_binaries(PREFIX, names(&[&temp])).is_empty());
    }

    #[test]
    fn versioned_and_legacy_binaries_are_both_found() {
        let got = existing_binaries(
            PREFIX,
            names(&[
                PREFIX,
                "zed-claude-ide-server-macos-aarch64-v0.1.0",
                "zed-claude-ide-server-macos-x86_64-v0.1.0",
                "unrelated",
            ]),
        );
        assert_eq!(
            got,
            names(&[PREFIX, "zed-claude-ide-server-macos-aarch64-v0.1.0"])
        );
    }

    #[test]
    fn a_sibling_platform_is_not_a_match() {
        // "-macos-aarch64" is not a prefix of "-macos-aarch64-foo" in the sense
        // that matters: only a "-v" version suffix counts.
        assert!(!is_versioned_binary(
            PREFIX,
            "zed-claude-ide-server-macos-aarch64-foo"
        ));
    }

    #[test]
    fn fallback_prefers_the_newest_versioned_build_over_the_legacy_name() {
        let got = fallback_binary(
            PREFIX,
            names(&[
                PREFIX,
                "zed-claude-ide-server-macos-aarch64-v0.1.0",
                "zed-claude-ide-server-macos-aarch64-v0.2.0",
            ]),
        );
        assert_eq!(
            got.as_deref(),
            Some("zed-claude-ide-server-macos-aarch64-v0.2.0")
        );
        assert_eq!(
            fallback_binary(PREFIX, names(&[PREFIX])).as_deref(),
            Some(PREFIX)
        );
        assert_eq!(fallback_binary(PREFIX, names(&[])), None);
    }
}
