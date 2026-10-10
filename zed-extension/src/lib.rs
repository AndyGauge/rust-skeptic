use zed_extension_api::{self as zed, serde_json, settings::LspSettings, LanguageServerId, Result};

struct SkepticExtension;

impl zed::Extension for SkepticExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let settings = LspSettings::for_worktree(language_server_id.as_ref(), worktree).ok();
        let configured = settings
            .as_ref()
            .and_then(|s| s.binary.as_ref())
            .and_then(|b| b.path.clone());

        let command = configured
            .or_else(|| worktree.which("skeptic-lsp"))
            .ok_or_else(|| {
                "`skeptic-lsp` not found on PATH; install it with \
                 `cargo install --git https://github.com/budziq/rust-skeptic skeptic-lsp`"
                    .to_string()
            })?;

        let args = settings
            .and_then(|s| s.binary)
            .and_then(|b| b.arguments)
            .unwrap_or_default();

        Ok(zed::Command {
            command,
            args,
            env: worktree.shell_env(),
        })
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        Ok(LspSettings::for_worktree(language_server_id.as_ref(), worktree)
            .ok()
            .and_then(|s| s.initialization_options))
    }
}

zed::register_extension!(SkepticExtension);
