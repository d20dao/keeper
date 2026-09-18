//! Local-only EVM -> disposable PostgreSQL integration runner. Never reads NEON_DB or key files.
use anyhow::{Context, Result, ensure};
use d20dao_keeper::{explorer, proxy::RuntimePins, rpc::Rpc};
use serde::Deserialize;
#[derive(Deserialize)]
struct Fixture {
    rpc_url: String,
    chain_id: u64,
    pins: RuntimePins,
}
#[tokio::main]
async fn main() -> Result<()> {
    let path = std::env::args()
        .nth(1)
        .context("Pass a public local fixture JSON path")?;
    let text = std::fs::read_to_string(path)?;
    ensure!(text.len() < 65536, "Fixture too large");
    let f: Fixture = serde_json::from_str(&text)?;
    ensure!(
        f.chain_id == 31337 && f.rpc_url.starts_with("http://127.0.0.1:"),
        "Local fixture only"
    );
    let rpc = Rpc::new(vec![f.rpc_url])?;
    let (mut pool, connection) = tokio_postgres::connect(
        "host=127.0.0.1 port=55439 user=postgres password=public-local-test dbname=explorer_test",
        tokio_postgres::NoTls,
    )
    .await?;
    let driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    for _ in 0..4 {
        explorer::index_once(&mut pool, &rpc, f.pins, f.chain_id).await?;
    }
    drop(pool);
    driver.abort();
    println!("Local explorer batches completed");
    Ok(())
}
