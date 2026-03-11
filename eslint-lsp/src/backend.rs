use std::{collections::HashMap, sync::Arc, time::Duration};

use serde_json::Value;
use tower_lsp::{
    Client, LanguageServer,
    jsonrpc::Result,
    lsp_types::{
        CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams,
        CodeActionProviderCapability, Command, Diagnostic, DiagnosticSeverity,
        DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
        DidSaveTextDocumentParams, DocumentFormattingParams, ExecuteCommandOptions,
        ExecuteCommandParams, InitializeParams, InitializeResult, InitializedParams, MessageType,
        OneOf, Position, Range, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
        TextDocumentSyncKind, TextDocumentSyncOptions, TextEdit, Url, WorkspaceEdit,
    },
};
use tracing::warn;

use tokio::{sync::Mutex, time::sleep};

use crate::eslint::{LintMessage, LintRequest, Resolver};

const FIX_ALL_ESLINT_KIND: &str = "source.fixAll.eslint";
const APPLY_FIX_ALL_ESLINT_COMMAND: &str = "eslint.applyFixAll";

#[derive(Clone, Debug)]
struct Document {
    version: i32,
    text: String,
}

#[derive(Default)]
struct State {
    documents: Mutex<HashMap<Url, Document>>,
    lint_generations: Mutex<HashMap<Url, u64>>,
    resolver: Resolver,
}

#[derive(Clone)]
pub struct Backend {
    client: Client,
    state: Arc<State>,
}

impl Backend {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            state: Arc::new(State::default()),
        }
    }

    async fn schedule_lint(&self, uri: Url) {
        let generation = {
            let mut generations = self.state.lint_generations.lock().await;
            let next = generations.get(&uri).copied().unwrap_or(0) + 1;
            generations.insert(uri.clone(), next);
            next
        };

        let backend = self.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(250)).await;

            let current = {
                let generations = backend.state.lint_generations.lock().await;
                generations.get(&uri).copied()
            };

            if current != Some(generation) {
                return;
            }

            backend.publish_lint(uri, generation).await;
        });
    }

    async fn publish_lint(&self, uri: Url, generation: u64) {
        let Some(document) = self.get_document(&uri).await else {
            return;
        };

        let file_path = match uri.to_file_path() {
            Ok(path) => path,
            Err(_) => {
                warn!("skipping non-file URI: {uri}");
                return;
            }
        };

        match self
            .state
            .resolver
            .lint(LintRequest {
                file_path,
                text: document.text,
                fix: false,
            })
            .await
        {
            Ok(response) => {
                if !self.is_current_generation(&uri, generation).await {
                    return;
                }

                let diagnostics = response
                    .diagnostics
                    .into_iter()
                    .map(to_lsp_diagnostic)
                    .collect::<Vec<_>>();

                self.client
                    .publish_diagnostics(uri, diagnostics, Some(document.version))
                    .await;
            }
            Err(error) => {
                if !self.is_current_generation(&uri, generation).await {
                    return;
                }

                let message = error.to_string();
                warn!("{message}");

                self.client
                    .publish_diagnostics(
                        uri,
                        vec![synthetic_diagnostic(message.clone())],
                        Some(document.version),
                    )
                    .await;

                self.client.log_message(MessageType::WARNING, message).await;
            }
        }
    }

    async fn get_document(&self, uri: &Url) -> Option<Document> {
        self.state.documents.lock().await.get(uri).cloned()
    }

    async fn set_document(&self, uri: Url, document: Document) {
        self.state.documents.lock().await.insert(uri, document);
    }

    async fn remove_document(&self, uri: &Url) {
        self.state.documents.lock().await.remove(uri);
        self.state.lint_generations.lock().await.remove(uri);
    }

    async fn is_current_generation(&self, uri: &Url, generation: u64) -> bool {
        self.state.lint_generations.lock().await.get(uri).copied() == Some(generation)
    }

    async fn compute_fix_edit(&self, uri: &Url) -> Option<TextEdit> {
        let document = self.get_document(uri).await?;
        let file_path = uri.to_file_path().ok()?;

        let response = self
            .state
            .resolver
            .lint(LintRequest {
                file_path,
                text: document.text.clone(),
                fix: true,
            })
            .await
            .ok()?;

        let fixed_text = response.fixed_text?;
        if fixed_text == document.text {
            return None;
        }

        Some(TextEdit {
            range: full_document_range(&document.text),
            new_text: fixed_text,
        })
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            server_info: Some(ServerInfo {
                name: "eslint-lsp".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(
                            tower_lsp::lsp_types::TextDocumentSyncSaveOptions::Supported(true),
                        ),
                        ..TextDocumentSyncOptions::default()
                    },
                )),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec![APPLY_FIX_ALL_ESLINT_COMMAND.to_owned()],
                    work_done_progress_options: Default::default(),
                }),
                document_formatting_provider: Some(OneOf::Left(true)),
                ..ServerCapabilities::default()
            },
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "eslint-lsp initialized")
            .await;
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let item = params.text_document;
        let document = Document {
            version: item.version,
            text: item.text,
        };
        let uri = item.uri;

        self.set_document(uri.clone(), document).await;
        self.schedule_lint(uri).await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let latest_text = match params.content_changes.last() {
            Some(change) => change.text.clone(),
            None => return,
        };

        let uri = params.text_document.uri;
        let document = Document {
            version: params.text_document.version,
            text: latest_text,
        };

        self.set_document(uri.clone(), document).await;
        self.schedule_lint(uri).await;
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        self.schedule_lint(params.text_document.uri).await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri;
        self.remove_document(&uri).await;
        self.client.publish_diagnostics(uri, Vec::new(), None).await;
    }

    async fn code_action(
        &self,
        params: CodeActionParams,
    ) -> Result<Option<Vec<CodeActionOrCommand>>> {
        if !allows_fix_all(&params) {
            return Ok(None);
        }

        let uri = params.text_document.uri;
        if self.get_document(&uri).await.is_none() {
            return Ok(None);
        }

        let action = CodeAction {
            title: "Fix all auto-fixable ESLint problems".to_owned(),
            kind: Some(CodeActionKind::new(FIX_ALL_ESLINT_KIND)),
            command: Some(fix_all_command(&uri)),
            is_preferred: Some(true),
            ..CodeAction::default()
        };

        Ok(Some(vec![CodeActionOrCommand::CodeAction(action)]))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<Value>> {
        if params.command != APPLY_FIX_ALL_ESLINT_COMMAND {
            return Ok(None);
        }

        let Some(uri) = params
            .arguments
            .first()
            .and_then(Value::as_str)
            .and_then(|uri| Url::parse(uri).ok())
        else {
            self.client
                .log_message(
                    MessageType::ERROR,
                    "eslint.applyFixAll was called without a valid document URI",
                )
                .await;
            return Ok(None);
        };

        let Some(edit) = self.compute_fix_edit(&uri).await else {
            return Ok(None);
        };

        match self
            .client
            .apply_edit(WorkspaceEdit {
                changes: Some(HashMap::from([(uri, vec![edit])])),
                ..WorkspaceEdit::default()
            })
            .await
        {
            Ok(response) if response.applied => {}
            Ok(_) => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        "the client rejected the ESLint fix-all workspace edit",
                    )
                    .await;
            }
            Err(error) => {
                self.client.log_message(MessageType::ERROR, error).await;
            }
        }

        Ok(None)
    }

    async fn formatting(&self, params: DocumentFormattingParams) -> Result<Option<Vec<TextEdit>>> {
        Ok(self
            .compute_fix_edit(&params.text_document.uri)
            .await
            .map(|edit| vec![edit]))
    }
}

fn allows_fix_all(params: &CodeActionParams) -> bool {
    params.context.only.as_ref().is_none_or(|requested| {
        requested.iter().any(|kind| match kind.as_str() {
            kind if kind == CodeActionKind::SOURCE.as_str() => true,
            kind if kind == CodeActionKind::SOURCE_FIX_ALL.as_str() => true,
            kind if kind == FIX_ALL_ESLINT_KIND => true,
            _ => false,
        })
    })
}

fn fix_all_command(uri: &Url) -> Command {
    Command::new(
        "Fix all auto-fixable ESLint problems".to_owned(),
        APPLY_FIX_ALL_ESLINT_COMMAND.to_owned(),
        Some(vec![Value::String(uri.to_string())]),
    )
}

fn synthetic_diagnostic(message: String) -> Diagnostic {
    Diagnostic {
        range: Range::new(Position::new(0, 0), Position::new(0, 1)),
        severity: Some(DiagnosticSeverity::ERROR),
        source: Some("eslint".to_owned()),
        message,
        ..Diagnostic::default()
    }
}

fn to_lsp_diagnostic(message: LintMessage) -> Diagnostic {
    Diagnostic {
        range: Range::new(
            Position::new(
                message.line.saturating_sub(1),
                message.column.saturating_sub(1),
            ),
            Position::new(
                message.end_line.unwrap_or(message.line).saturating_sub(1),
                message
                    .end_column
                    .unwrap_or(message.column.saturating_add(1))
                    .saturating_sub(1),
            ),
        ),
        severity: Some(match message.severity {
            2 => DiagnosticSeverity::ERROR,
            _ => DiagnosticSeverity::WARNING,
        }),
        source: Some("eslint".to_owned()),
        code: message
            .rule_id
            .map(tower_lsp::lsp_types::NumberOrString::String),
        message: message.message,
        ..Diagnostic::default()
    }
}

fn full_document_range(text: &str) -> Range {
    let mut line = 0u32;
    let mut column = 0u32;

    for ch in text.chars() {
        if ch == '\n' {
            line += 1;
            column = 0;
        } else {
            column += ch.len_utf16() as u32;
        }
    }

    Range::new(Position::new(0, 0), Position::new(line, column))
}

#[cfg(test)]
mod tests {
    use tower_lsp::lsp_types::{CodeActionContext, TextDocumentIdentifier, WorkDoneProgressParams};

    use super::*;

    #[test]
    fn fix_all_filter_accepts_missing_only() {
        let params = CodeActionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::parse("file:///tmp/example.js").unwrap(),
            },
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            context: CodeActionContext {
                diagnostics: Vec::new(),
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: Default::default(),
        };

        assert!(allows_fix_all(&params));
    }

    #[test]
    fn range_covers_entire_single_line_document() {
        let range = full_document_range("const value = 1");
        assert_eq!(range.end, Position::new(0, 15));
    }

    #[test]
    fn range_covers_document_with_trailing_newline() {
        let range = full_document_range("const value = 1;\n");
        assert_eq!(range.end, Position::new(1, 0));
    }

    #[test]
    fn fix_all_filter_rejects_other_source_actions() {
        let params = CodeActionParams {
            text_document: TextDocumentIdentifier {
                uri: Url::parse("file:///tmp/example.js").unwrap(),
            },
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            context: CodeActionContext {
                diagnostics: Vec::new(),
                only: Some(vec![CodeActionKind::SOURCE_ORGANIZE_IMPORTS]),
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: Default::default(),
        };

        assert!(!allows_fix_all(&params));
    }
}
