use kwms::cli;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match cli::run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // Logged like everything else (credentials removed), not printed.
            kwms::logging::fatal(&e);
            std::process::ExitCode::FAILURE
        }
    }
}
