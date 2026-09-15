//! How the extension names, finds and caches the companion binary.
//!
//! The extension itself compiles to `wasm32-wasip2` against `zed_extension_api`,
//! which cannot be unit-tested on the host. Everything here is plain string and
//! path logic with no dependency on that API, so it can be.

/// Operating systems the release workflow builds for.

#[derive(Debug)]
pub enum Os {
    Mac,
    Linux,
    Windows,
}

/// CPU architectures the release workflow builds for.
#[derive(Debug)]
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
        (Os::Linux, Arch::Aarch64) => Ok("zed-claude-ide-server-linux-aarch64"),
        (Os::Linux, Arch::X86_64) => Ok("zed-claude-ide-server-linux-x86_64"),
        (Os::Windows, _) => Err("Windows is not currently supported".to_string()),
        (os, arch) => Err(format!("Unsupported platform: {os:?}-{arch:?}")),
    }
}

pub const DOWNLOAD_SUFFIX: &str = ".download";

pub fn versioned_name(prefix: &str, version: &str) -> String {
    format!("{prefix}-{version}")
}

pub fn download_temp_name(name: &str) -> String {
    format!("{name}{DOWNLOAD_SUFFIX}")
}

pub fn is_versioned_binary(prefix: &str, name: &str) -> bool {
    match name.strip_prefix(prefix) {
        Some(suffix) => suffix.starts_with("-v") && !suffix.ends_with(DOWNLOAD_SUFFIX),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const PREFIX: &str = "zed-claude-ide-server-macos-aarch64";

    #[test]
    fn a_versioned_build_is_something_to_run() {
        let versioned = versioned_name(PREFIX, "v0.1.0");
        assert_eq!(versioned, "zed-claude-ide-server-macos-aarch64-v0.1.0");
        assert!(is_versioned_binary(PREFIX, &versioned));
    }

    #[test]
    fn an_interrupted_download_is_never_treated_as_a_binary() {
        let versioned = versioned_name(PREFIX, "v0.1.0");
        let temp = download_temp_name(&versioned);
        assert!(
            !is_versioned_binary(PREFIX, &temp),
            "{temp} is a truncated download, not something to run"
        );
    }

    #[test]
    fn a_sibling_platform_is_not_a_match() {
        assert!(!is_versioned_binary(
            PREFIX,
            "zed-claude-ide-server-macos-aarch64-foo"
        ));
    }

    #[test]
    fn each_supported_platform_has_an_asset_name() {
        assert_eq!(
            asset_name(Os::Mac, Arch::Aarch64),
            Ok("zed-claude-ide-server-macos-aarch64")
        );
        assert_eq!(
            asset_name(Os::Linux, Arch::Aarch64),
            Ok("zed-claude-ide-server-linux-aarch64")
        );
        assert_eq!(
            asset_name(Os::Linux, Arch::X86_64),
            Ok("zed-claude-ide-server-linux-x86_64")
        );
    }

    #[test]
    fn windows_and_unknown_platforms_are_refused_with_a_reason() {
        assert!(asset_name(Os::Windows, Arch::X86_64).is_err());
        assert!(asset_name(Os::Linux, Arch::X86).is_err());
    }
}
