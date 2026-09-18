//! Explicit schema-only setup. The caller supplies NEON_DB in process environment; no dotenv or keys are read.
#[tokio::main]
async fn main() {
    let result = async {
        let settings = d20dao_keeper::explorer::Settings::from_env()?
            .ok_or_else(|| anyhow::anyhow!("Explorer database is not configured"))?;
        settings.initialize().await
    }
    .await;
    match result {
        Ok(()) => {
            println!("Explorer schema initialized with verified TLS and required channel binding")
        }
        Err(_) => {
            eprintln!("Explorer schema initialization failed; connection details suppressed");
            std::process::exit(1);
        }
    }
}
