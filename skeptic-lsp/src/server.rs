use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::runner;

#[derive(Default)]
struct State {
    /// Extra arguments appended to `cargo test` (from `initializationOptions.args`).
    args: Vec<String>,
    /// In-flight run per workspace root; aborted when a newer save arrives.
    running: HashMap<PathBuf, JoinHandle<()>>,
    /// Files we published diagnostics for, per workspace root, so stale ones get cleared.
    published: HashMap<PathBuf, HashSet<PathBuf>>,
}

pub struct Backend {
    client: Client,
    state: Arc<Mutex<State>>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            state: Arc::default(),
        }
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        if let Some(args) = params
            .initialization_options
            .as_ref()
            .and_then(|o| o.get("args"))
            .and_then(|a| a.as_array())
        {
            self.state.lock().await.args = args
                .iter()
                .filter_map(|a| a.as_str().map(String::from))
                .collect();
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::NONE),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "skeptic-lsp".into(),
                version: Some(env!("CARGO_PKG_VERSION").into()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(
                MessageType::INFO,
                "skeptic-lsp ready; diagnostics run on save",
            )
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        for (_, handle) in self.state.lock().await.running.drain() {
            handle.abort();
        }
        Ok(())
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let Ok(file) = params.text_document.uri.to_file_path() else {
            return;
        };
        // Rust sources, manifests and markdown (skeptic doc tests) can change the outcome.
        let relevant = file.extension().is_some_and(|e| e == "rs" || e == "md")
            || file.file_name().is_some_and(|n| n == "Cargo.toml");
        if !relevant {
            return;
        }
        let Some(root) = runner::workspace_root(&file).await else {
            return;
        };

        // A markdown save only needs that file's skeptic snippets, not the whole suite.
        let markdown = file.extension().is_some_and(|e| e == "md");
        let filter = markdown
            .then(|| {
                file.file_stem()
                    .and_then(|s| s.to_str())
                    .map(crate::cargo_output::sanitize)
            })
            .flatten()
            .map(|stem| format!("{stem}_"));
        let scope = markdown.then(|| file.clone());

        let client = self.client.clone();
        let state = self.state.clone();
        let task_root = root.clone();
        let mut guard = self.state.lock().await;
        if let Some(old) = guard.running.remove(&root) {
            old.abort();
        }
        let args = guard.args.clone();
        let handle = tokio::spawn(async move {
            client
                .log_message(
                    MessageType::INFO,
                    format!("cargo test in {}", task_root.display()),
                )
                .await;
            match runner::check(&task_root, &args, filter.as_deref()).await {
                Ok(found) => publish(&client, &state, &task_root, found, scope).await,
                Err(e) => {
                    client
                        .show_message(
                            MessageType::ERROR,
                            format!("skeptic-lsp: failed to run cargo: {e}"),
                        )
                        .await
                }
            }
        });
        guard.running.insert(root, handle);
    }
}

/// Publishes `found` and clears files that are now clean. A `scope` marks a
/// filtered run (one markdown file): other files' earlier diagnostics are kept.
async fn publish(
    client: &Client,
    state: &Mutex<State>,
    root: &Path,
    found: crate::cargo_output::FileDiagnostics,
    scope: Option<PathBuf>,
) {
    let mut cleared = Vec::new();
    {
        let mut s = state.lock().await;
        let entry = s.published.entry(root.to_path_buf()).or_default();
        match &scope {
            Some(file) => {
                if !found.contains_key(file) && entry.remove(file) {
                    cleared.push(file.clone());
                }
                entry.extend(found.keys().cloned());
            }
            None => {
                cleared.extend(entry.iter().filter(|p| !found.contains_key(*p)).cloned());
                *entry = found.keys().cloned().collect();
            }
        }
    }
    for path in cleared {
        if let Ok(uri) = Url::from_file_path(path) {
            client.publish_diagnostics(uri, Vec::new(), None).await;
        }
    }
    for (path, diags) in found {
        if let Ok(uri) = Url::from_file_path(&path) {
            client.publish_diagnostics(uri, diags, None).await;
        }
    }
}
