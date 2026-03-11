use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::RwLock;
use tower_lsp::{
    Client, LanguageServer,
    jsonrpc::{Error, Result},
    lsp_types::{
        DidChangeTextDocumentParams, DidChangeWorkspaceFoldersParams, DidCloseTextDocumentParams,
        DidOpenTextDocumentParams, DocumentFormattingParams, InitializeParams, InitializeResult,
        InitializedParams, MessageType, OneOf, Position, Range, ServerCapabilities,
        TextDocumentSyncCapability, TextDocumentSyncKind, TextEdit, Url,
        WorkspaceFoldersServerCapabilities, WorkspaceServerCapabilities,
    },
};
use tracing::{error, info};

use crate::formatter::{FormatOutcome, Formatter};

pub struct Backend {
    client: Client,
    formatter: Arc<dyn Formatter>,
    documents: RwLock<HashMap<Url, String>>,
    workspace_roots: RwLock<Vec<PathBuf>>,
}

impl Backend {
    pub fn new(client: Client, formatter: Arc<dyn Formatter>) -> Self {
        Self {
            client,
            formatter,
            documents: RwLock::new(HashMap::new()),
            workspace_roots: RwLock::new(Vec::new()),
        }
    }

    async fn document_text(&self, uri: &Url, file_path: &PathBuf) -> Result<String> {
        if let Some(text) = self.documents.read().await.get(uri).cloned() {
            return Ok(text);
        }

        tokio::fs::read_to_string(file_path).await.map_err(|error| {
            error!("failed reading {}: {error}", file_path.display());
            Error::internal_error()
        })
    }

    async fn workspace_root_for(&self, file_path: &Path) -> Option<PathBuf> {
        self.workspace_roots
            .read()
            .await
            .iter()
            .filter(|root| file_path.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned()
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        *self.workspace_roots.write().await = extract_workspace_roots(&params);

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                document_formatting_provider: Some(OneOf::Left(true)),
                workspace: Some(WorkspaceServerCapabilities {
                    workspace_folders: Some(WorkspaceFoldersServerCapabilities {
                        supported: Some(true),
                        change_notifications: Some(OneOf::Left(true)),
                    }),
                    file_operations: None,
                }),
                ..ServerCapabilities::default()
            },
            ..InitializeResult::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        info!("prettier-lsp initialized");
        self.client
            .log_message(
                MessageType::INFO,
                "prettier-lsp is using the workspace's installed prettier",
            )
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_change_workspace_folders(&self, params: DidChangeWorkspaceFoldersParams) {
        let mut roots = self.workspace_roots.write().await;
        apply_workspace_folder_change(&mut roots, &params);
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let text_document = params.text_document;
        self.documents
            .write()
            .await
            .insert(text_document.uri, text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            self.documents
                .write()
                .await
                .insert(params.text_document.uri, change.text);
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        self.documents
            .write()
            .await
            .remove(&params.text_document.uri);
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        let uri = params.text_document.uri;
        let file_path = uri
            .to_file_path()
            .map_err(|()| Error::invalid_params("prettier-lsp only supports file URIs"))?;
        let source = self.document_text(&uri, &file_path).await?;
        let workspace_root = self.workspace_root_for(&file_path).await;

        match self
            .formatter
            .format(&file_path, &source, workspace_root.as_deref())
            .await
        {
            Ok(FormatOutcome::Formatted(formatted)) => {
                if formatted == source {
                    return Ok(Some(Vec::new()));
                }

                Ok(Some(vec![TextEdit {
                    range: full_document_range(&source),
                    new_text: formatted,
                }]))
            }
            Ok(FormatOutcome::Ignored | FormatOutcome::Unsupported) => Ok(Some(Vec::new())),
            Err(error) => {
                if error.is_unavailable() {
                    return Ok(Some(Vec::new()));
                }

                let message = error.to_string();
                error!("formatting failed for {}: {message}", file_path.display());
                self.client.show_message(MessageType::ERROR, message).await;
                Err(Error::internal_error())
            }
        }
    }
}

#[allow(deprecated)]
fn extract_workspace_roots(params: &InitializeParams) -> Vec<PathBuf> {
    let mut roots = params
        .workspace_folders
        .as_ref()
        .into_iter()
        .flat_map(|folders| folders.iter())
        .filter_map(|folder| folder.uri.to_file_path().ok())
        .collect::<Vec<_>>();

    if roots.is_empty() {
        if let Some(root_uri) = params
            .root_uri
            .as_ref()
            .and_then(|uri| uri.to_file_path().ok())
        {
            roots.push(root_uri);
        } else if let Some(root_path) = params.root_path.as_ref() {
            roots.push(PathBuf::from(root_path));
        }
    }

    normalize_workspace_roots(&mut roots);
    roots
}

fn apply_workspace_folder_change(
    roots: &mut Vec<PathBuf>,
    params: &DidChangeWorkspaceFoldersParams,
) {
    roots.retain(|root| {
        !params
            .event
            .removed
            .iter()
            .filter_map(|folder| folder.uri.to_file_path().ok())
            .any(|removed| &removed == root)
    });

    roots.extend(
        params
            .event
            .added
            .iter()
            .filter_map(|folder| folder.uri.to_file_path().ok()),
    );

    normalize_workspace_roots(roots);
}

fn normalize_workspace_roots(roots: &mut Vec<PathBuf>) {
    roots.sort();
    roots.dedup();
}

fn full_document_range(text: &str) -> Range {
    let line = text.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let last_line = text.rsplit('\n').next().unwrap_or_default();

    Range::new(
        Position::new(0, 0),
        Position::new(line, utf16_len(last_line)),
    )
}

fn utf16_len(input: &str) -> u32 {
    input.encode_utf16().count() as u32
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        apply_workspace_folder_change, extract_workspace_roots, full_document_range,
        normalize_workspace_roots,
    };
    use tower_lsp::lsp_types::{
        DidChangeWorkspaceFoldersParams, InitializeParams, Position, Range, Url, WorkspaceFolder,
        WorkspaceFoldersChangeEvent,
    };

    #[test]
    fn calculates_range_for_trailing_newline() {
        assert_eq!(
            full_document_range("const answer = 42;\n"),
            Range::new(Position::new(0, 0), Position::new(1, 0))
        );
    }

    #[test]
    fn calculates_range_for_multibyte_characters() {
        assert_eq!(
            full_document_range("const sushi = \"🍣\";"),
            Range::new(Position::new(0, 0), Position::new(0, 19))
        );
    }

    #[test]
    fn prefers_workspace_folders_over_root_uri() {
        let root = PathBuf::from("/workspace");
        let package = root.join("packages/app");
        let params = InitializeParams {
            root_uri: Some(Url::from_file_path(&root).unwrap()),
            workspace_folders: Some(vec![WorkspaceFolder {
                uri: Url::from_file_path(&package).unwrap(),
                name: "app".into(),
            }]),
            ..InitializeParams::default()
        };

        assert_eq!(extract_workspace_roots(&params), vec![package]);
    }

    #[test]
    fn applies_workspace_folder_additions_and_removals() {
        let mut roots = vec![PathBuf::from("/workspace-a"), PathBuf::from("/workspace-b")];
        let params = DidChangeWorkspaceFoldersParams {
            event: WorkspaceFoldersChangeEvent {
                added: vec![WorkspaceFolder {
                    uri: Url::from_file_path("/workspace-c").unwrap(),
                    name: "workspace-c".into(),
                }],
                removed: vec![WorkspaceFolder {
                    uri: Url::from_file_path("/workspace-a").unwrap(),
                    name: "workspace-a".into(),
                }],
            },
        };

        apply_workspace_folder_change(&mut roots, &params);

        assert_eq!(
            roots,
            vec![PathBuf::from("/workspace-b"), PathBuf::from("/workspace-c")]
        );
    }

    #[test]
    fn normalizes_workspace_roots() {
        let mut roots = vec![
            PathBuf::from("/workspace-b"),
            PathBuf::from("/workspace-a"),
            PathBuf::from("/workspace-a"),
        ];

        normalize_workspace_roots(&mut roots);

        assert_eq!(
            roots,
            vec![PathBuf::from("/workspace-a"), PathBuf::from("/workspace-b")]
        );
    }
}
