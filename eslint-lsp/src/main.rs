mod backend;
mod eslint;

use tower_lsp::{LspService, Server};
use tracing_subscriber::{EnvFilter, fmt};

use crate::backend::Backend;

#[tokio::main]
async fn main() {
    if std::env::var_os("RUST_LOG").is_some() {
        let _ = fmt()
            .with_env_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| EnvFilter::new("warn,eslint_lsp=info")),
            )
            .with_writer(std::io::stderr)
            .try_init();
    }

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(Backend::new);

    Server::new(stdin, stdout, socket).serve(service).await;
}
