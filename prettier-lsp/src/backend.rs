use std::{collections::HashMap, path::PathBuf, sync::Arc};

use tokio::sync::RwLock;
use tower_lsp::{
    Client, LanguageServer,
    jsonrpc::{Error, Result},
    lsp_types::{
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DocumentFormattingParams, InitializeParams, InitializeResult, InitializedParams,
        MessageType, OneOf, Position, Range, ServerCapabilities, TextDocumentSyncCapability,
        TextDocumentSyncKind, TextEdit, Url,
    },
};
use tracing::{error, info};

use crate::formatter::{FormatOutcome, Formatter};

pub struct Backend {
    client: Client,
    formatter: Arc<dyn Formatter>,
    documents: RwLock<HashMap<Url, String>>,
}

impl Backend {
    pub fn new(client: Client, formatter: Arc<dyn Formatter>) -> Self {
        Self {
            client,
            formatter,
            documents: RwLock::new(HashMap::new()),
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
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                document_formatting_provider: Some(OneOf::Left(true)),
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

        match self.formatter.format(&file_path, &source).await {
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
                let message = error.to_string();
                error!("formatting failed for {}: {message}", file_path.display());
                self.client.show_message(MessageType::ERROR, message).await;
                Err(Error::internal_error())
            }
        }
    }
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
    use super::full_document_range;
    use tower_lsp::lsp_types::{Position, Range};

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
}
