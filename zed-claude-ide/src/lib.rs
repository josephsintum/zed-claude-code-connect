//! The Zed side of the bridge.
//!
//! A WASM extension can neither open a socket nor see the editor, so this one does
//! exactly one thing: tell Zed which binary to spawn as the "language server" for
//! this worktree, and with which arguments. Everything else happens in that binary.

use std::fs;
use std::path::Path;

use zed_claude_ide_resolve as resolve;
use zed_extension_api::{
    current_platform, download_file, latest_github_release, make_file_executable,
    set_language_server_installation_status, settings::LspSettings, Architecture, Command,
    DownloadedFileType, Extension, GithubReleaseOptions, LanguageServerId,
    LanguageServerInstallationStatus, Os, Result, Worktree,
};

/// Must match the `[language_servers.*]` key in extension.toml, and is the
/// key users put under `"lsp"` in settings.json to override the binary.
const SERVER_ID: &str = "zed-claude-ide-server";

/// Releases are downloaded from here. Still a placeholder: until it names a real
/// repository, only the settings-path and PATH tiers can succeed.
const GITHUB_REPO: &str = "josephsintum/zed-claude-ide";

struct ClaudeCodeExtension {
    /// The binary resolved earlier in this session.
    ///
    /// Zed calls `language_server_command` once per worktree. Without this, every
    /// call asked the GitHub API for the latest release before looking at what
    /// was already on disk -- 60 unauthenticated requests an hour, and running
    /// out of them dropped straight into the fallback tiers.
    cached_binary_path: Option<String>,
}

impl Extension for ClaudeCodeExtension {
    fn new() -> Self {
        Self {
            cached_binary_path: None,
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<Command> {
        if language_server_id.as_ref() != SERVER_ID {
            return Err(format!("Unknown language server: {language_server_id}"));
        }

        let command = self.server_binary(language_server_id, worktree)?;

        let args = LspSettings::for_worktree(SERVER_ID, worktree)
            .ok()
            .and_then(|s| s.binary.and_then(|b| b.arguments))
            .unwrap_or_else(|| {
                vec![
                    "--worktree".to_string(),
                    worktree.root_path(),
                    "serve".to_string(),
                ]
            });

        eprintln!(
            "[zed-claude-ide] starting {command} for {}",
            worktree.root_path()
        );
        Ok(Command {
            command,
            args,
            env: Default::default(),
        })
    }
}

impl ClaudeCodeExtension {
    /// Resolve the companion binary, in priority order:
    ///   1. An explicit path in Zed's `lsp` settings -- the development loop.
    ///   2. The path resolved earlier in this session.
    ///   3. A cached or freshly downloaded GitHub release asset.
    ///   4. `zed-claude-ide-server` on PATH, as an absolute path from `which`.
    fn server_binary(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> Result<String> {
        if let Some(path) = LspSettings::for_worktree(SERVER_ID, worktree)
            .ok()
            .and_then(|s| s.binary.and_then(|b| b.path))
        {
            eprintln!("[zed-claude-ide] using binary from Zed settings: {path}");
            return Ok(path);
        }

        if let Some(path) = &self.cached_binary_path {
            if Path::new(path).exists() {
                return Ok(path.clone());
            }
        }

        let path = match release_binary(language_server_id) {
            Ok(path) => path,
            Err(release_error) => {
                // `which` yields an absolute path, which is the only kind Zed can
                // run from here: a relative command is joined onto the extension's
                // work directory, so a bare "zed-claude-ide-server" never resolved.
                match worktree.which(SERVER_ID) {
                    Some(path) => {
                        eprintln!(
                            "[zed-claude-ide] {release_error}; using {path} from PATH instead"
                        );
                        path
                    }
                    None => {
                        let message = format!("{release_error}, and {SERVER_ID} is not on PATH");
                        set_language_server_installation_status(
                            language_server_id,
                            &LanguageServerInstallationStatus::Failed(message.clone()),
                        );
                        return Err(message);
                    }
                }
            }
        };

        self.cached_binary_path = Some(path.clone());
        Ok(path)
    }
}

/// The current release from GitHub, downloaded if this version is not already in
/// the work directory. Falls back to whatever versioned build is on disk when the
/// network is unavailable, so an offline start still works.
fn release_binary(language_server_id: &LanguageServerId) -> Result<String> {
    let prefix = asset_prefix()?;

    set_language_server_installation_status(
        language_server_id,
        &LanguageServerInstallationStatus::CheckingForUpdate,
    );
    let release = match latest_github_release(
        GITHUB_REPO,
        GithubReleaseOptions {
            require_assets: true,
            pre_release: false,
        },
    ) {
        Ok(release) => release,
        Err(e) => {
            return fallback_binary(prefix).ok_or_else(|| {
                format!("could not fetch the latest release from {GITHUB_REPO} ({e}) and no cached binary exists")
            });
        }
    };

    let versioned = resolve::versioned_name(prefix, &release.version);
    if Path::new(&versioned).exists() {
        set_language_server_installation_status(
            language_server_id,
            &LanguageServerInstallationStatus::None,
        );
        make_file_executable(&versioned)?;
        return Ok(versioned);
    }

    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == prefix)
        .ok_or_else(|| format!("release {} has no asset named {prefix}", release.version))?;

    set_language_server_installation_status(
        language_server_id,
        &LanguageServerInstallationStatus::Downloading,
    );

    // Download to a temporary name and rename into place, so an interrupted
    // download never leaves a truncated file under the name that gets executed.
    let temp = resolve::download_temp_name(&versioned);
    let _ = fs::remove_file(&temp);
    let downloaded = download_file(&asset.download_url, &temp, DownloadedFileType::Uncompressed)
        .and_then(|()| make_file_executable(&temp))
        .and_then(|()| fs::rename(&temp, &versioned).map_err(|e| e.to_string()));

    match downloaded {
        Ok(()) => {
            for old in existing_binaries(prefix) {
                if old != versioned {
                    let _ = fs::remove_file(&old);
                }
            }
            set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::None,
            );
            Ok(versioned)
        }
        Err(e) => {
            let _ = fs::remove_file(&temp);
            set_language_server_installation_status(
                language_server_id,
                &LanguageServerInstallationStatus::Failed(e.clone()),
            );
            fallback_binary(prefix).ok_or_else(|| {
                format!(
                    "download of {} failed ({e}) and no cached binary exists",
                    asset.name
                )
            })
        }
    }
}

/// Release asset name for this machine, e.g. `zed-claude-ide-server-macos-aarch64`.
fn asset_prefix() -> Result<&'static str> {
    // Zed's platform detection, not env::consts, which would say wasm32.
    let (os, arch) = current_platform();
    let os = match os {
        Os::Mac => resolve::Os::Mac,
        Os::Linux => resolve::Os::Linux,
        Os::Windows => resolve::Os::Windows,
    };
    let arch = match arch {
        Architecture::Aarch64 => resolve::Arch::Aarch64,
        Architecture::X8664 => resolve::Arch::X86_64,
        Architecture::X86 => resolve::Arch::X86,
    };
    resolve::asset_name(os, arch)
}

/// Names in the extension work directory (the process's cwd) that are builds of
/// `prefix`, excluding interrupted downloads.
fn existing_binaries(prefix: &str) -> Vec<String> {
    let names = fs::read_dir(".")
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    resolve::existing_binaries(prefix, names)
}

fn fallback_binary(prefix: &str) -> Option<String> {
    let path = resolve::fallback_binary(prefix, existing_binaries(prefix))?;
    eprintln!("[zed-claude-ide] using cached binary {path}");
    if let Err(e) = make_file_executable(&path) {
        eprintln!("[zed-claude-ide] could not make {path} executable: {e}");
    }
    Some(path)
}

zed_extension_api::register_extension!(ClaudeCodeExtension);
