use std::sync::Arc;

use prettier_lsp::{Backend, NodePrettierFormatter};
use tower_lsp::{LspService, Server};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    init_tracing();

    let formatter = Arc::new(NodePrettierFormatter::default());
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) =
        LspService::build(|client| Backend::new(client, formatter.clone())).finish();

    Server::new(stdin, stdout, socket).serve(service).await;
}

fn init_tracing() {
    let Ok(filter) = EnvFilter::try_from_default_env() else {
        return;
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .try_init();
}
