fn main() {
    if let Err(error) = tsgo_lsp::run() {
        eprintln!("{error:?}");
        std::process::exit(1);
    }
}
