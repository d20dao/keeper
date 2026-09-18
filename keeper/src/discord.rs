//! Public proof notifications only. No Gateway, incoming commands, operational alerts or tick HTTP.
use alloy_primitives::{Address, B256, U256, keccak256};
use reqwest::{Client, Url, header};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

const CAPACITY: usize = 128;
const BODY_LIMIT: usize = 65_536;

pub struct Settings {
    authorization: header::HeaderValue,
    channel: u64,
    links: Links,
}
struct Links {
    chain: u64,
    coordinator: Address,
    explorer: String,
    public_explorer: Option<String>,
}
#[derive(Debug)]
pub struct ConfigurationError;
impl std::fmt::Display for ConfigurationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Invalid optional Discord configuration")
    }
}
impl std::error::Error for ConfigurationError {}
fn public_url(value: String) -> Result<String, ConfigurationError> {
    let url = Url::parse(&value).map_err(|_| ConfigurationError)?;
    if value.len() > 256
        || url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigurationError);
    }
    Ok(url.to_string().trim_end_matches('/').to_owned())
}
impl Settings {
    pub fn from_env(chain: u64, coordinator: Address) -> Result<Option<Self>, ConfigurationError> {
        let read = |name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(_) => Err(ConfigurationError),
        };
        Self::parse(
            read("DISCORD_BOT_TOKEN")?,
            read("DISCORD_PROTOCOL_CHANNEL_ID")?,
            read("EXPLORER_URL")?,
            read("DISCORD_PUBLIC_EXPLORER_URL")?,
            chain,
            coordinator,
        )
    }
    fn parse(
        token: Option<String>,
        channel: Option<String>,
        explorer: Option<String>,
        public_explorer: Option<String>,
        chain: u64,
        coordinator: Address,
    ) -> Result<Option<Self>, ConfigurationError> {
        let (token, channel) = match (token, channel) {
            (None, None) => return Ok(None),
            (Some(token), Some(channel)) => (zeroize::Zeroizing::new(token), channel),
            _ => return Err(ConfigurationError),
        };
        if token.len() < 16 || token.len() > 256 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(ConfigurationError);
        }
        let parsed = channel.parse::<u64>().map_err(|_| ConfigurationError)?;
        if parsed == 0 || parsed.to_string() != channel {
            return Err(ConfigurationError);
        }
        let mut authorization = header::HeaderValue::from_str(&format!("Bot {}", &*token))
            .map_err(|_| ConfigurationError)?;
        authorization.set_sensitive(true);
        Ok(Some(Self {
            authorization,
            channel: parsed,
            links: Links {
                chain,
                coordinator,
                explorer: public_url(explorer.ok_or(ConfigurationError)?)?,
                public_explorer: public_explorer.map(public_url).transpose()?,
            },
        }))
    }
}

#[derive(Clone)]
pub struct ProofAccepted {
    pub request_id: U256,
    pub epoch_id: u64,
    pub tx_hash: B256,
    pub proof_hash: B256,
    pub randomness: B256,
}
impl ProofAccepted {
    pub fn from_receipt(
        status: u64,
        kind: &str,
        request_id: U256,
        tx_hash: B256,
        request: &crate::abi::Request,
    ) -> Option<Self> {
        (status == 1 && (kind == "fulfill" || kind == "fulfill_batch") && request.fulfilled)
            .then_some(Self {
                request_id,
                epoch_id: request.epochId,
                tx_hash,
                proof_hash: request.proofHash,
                randomness: request.randomness,
            })
    }
    fn payload(&self, links: &Links) -> Value {
        let mut content = format!(
            "**Proof verified · Request #{}**\nChain `{}` · Epoch `{}`\n\nRandomness\n`{}`\nProof hash\n`{}`\n\n[View transaction]({}/tx/{})",
            self.request_id,
            links.chain,
            self.epoch_id,
            self.randomness,
            self.proof_hash,
            links.explorer,
            self.tx_hash
        );
        if let Some(base) = &links.public_explorer {
            content.push_str(&format!(
                "\n[Replay proof]({base}/explorer/request/{}/{}/{})",
                links.chain, links.coordinator, self.request_id
            ));
        }
        let identity = keccak256(format!(
            "{}:{}:{}:{}",
            links.chain, links.coordinator, self.request_id, self.tx_hash
        ));
        json!({"content":content,"allowed_mentions":{"parse":[]},"flags":4,
            "nonce":hex::encode(&identity[..12]),"enforce_nonce":true})
    }
}
struct Task(tokio::task::AbortHandle);
impl Drop for Task {
    fn drop(&mut self) {
        self.0.abort();
    }
}
#[derive(Clone)]
pub struct Notifier {
    tx: mpsc::Sender<ProofAccepted>,
    _task: Arc<Task>,
}
impl Notifier {
    pub fn start(settings: Settings) -> Result<Self, ConfigurationError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| ConfigurationError)?;
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| ConfigurationError)?;
        let (tx, rx) = mpsc::channel(CAPACITY);
        let endpoint = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            settings.channel
        );
        let task = runtime.spawn(send_loop(client, endpoint, settings, rx));
        tracing::info!("Optional Discord proof feed enabled");
        Ok(Self {
            tx,
            _task: Arc::new(Task(task.abort_handle())),
        })
    }
    pub fn notify(&self, proof: ProofAccepted) {
        // Best effort, just like Telegram: never block the request/nonce lane.
        let _ = self.tx.try_send(proof);
    }
}
enum Outcome {
    Sent,
    RateLimited(Duration),
    Retryable,
    Failed,
}
async fn deliver(
    client: &Client,
    endpoint: &str,
    auth: &header::HeaderValue,
    payload: &Value,
) -> Outcome {
    let result = client
        .post(endpoint)
        .header(header::AUTHORIZATION, auth.clone())
        .header(
            header::USER_AGENT,
            "DiscordBot (https://github.com/d20dao/keeper, 0.1)",
        )
        .json(payload)
        .send()
        .await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                timeout = error.is_timeout(),
                connect = error.is_connect(),
                "Discord transport did not confirm delivery"
            );
            return Outcome::Retryable;
        }
    };
    if response.status().is_success() {
        return Outcome::Sent;
    }
    if response.status().as_u16() != 429 {
        tracing::warn!(
            status = response.status().as_u16(),
            "Discord rejected proof notice"
        );
        if response.status().is_server_error() {
            return Outcome::Retryable;
        }
        return Outcome::Failed;
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= BODY_LIMIT => {
                body.extend_from_slice(&chunk)
            }
            Ok(None) => break,
            _ => return Outcome::RateLimited(Duration::from_secs(60)),
        }
    }
    let seconds = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|v| v["retry_after"].as_f64())
        .filter(|s| s.is_finite() && *s >= 0.0);
    match seconds {
        Some(s) if s <= 86_400.0 => Outcome::RateLimited(Duration::from_secs_f64(s.max(1.0))),
        _ => Outcome::RateLimited(Duration::from_secs(86_400)),
    }
}
async fn send_loop(
    client: Client,
    endpoint: String,
    settings: Settings,
    mut rx: mpsc::Receiver<ProofAccepted>,
) {
    while let Some(proof) = rx.recv().await {
        let payload = proof.payload(&settings.links);
        let started = tokio::time::Instant::now();
        for attempt in 0..3 {
            match deliver(&client, &endpoint, &settings.authorization, &payload).await {
                Outcome::RateLimited(delay) => {
                    tokio::time::sleep(delay).await;
                    if attempt == 2 || started.elapsed() >= Duration::from_secs(60) {
                        break;
                    }
                }
                Outcome::Retryable
                    if attempt < 2 && started.elapsed() < Duration::from_secs(60) =>
                {
                    tokio::time::sleep(Duration::from_secs(1)).await
                }
                Outcome::Sent => {
                    tracing::info!(request_id=%proof.request_id,"Discord proof notice sent");
                    break;
                }
                Outcome::Retryable | Outcome::Failed => break,
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings() -> Settings {
        Settings::parse(
            Some("public-fixture-token".into()),
            Some("42".into()),
            Some("https://explorer.example".into()),
            Some("https://d20.example".into()),
            31337,
            Address::repeat_byte(1),
        )
        .unwrap()
        .unwrap()
    }
    #[test]
    fn optional_configuration_rejects_partial_secret_urls_and_invalid_channels() {
        assert!(
            Settings::parse(None, None, None, None, 31337, Address::ZERO)
                .unwrap()
                .is_none()
        );
        assert!(
            Settings::parse(
                Some("public-fixture-token".into()),
                None,
                None,
                None,
                31337,
                Address::ZERO
            )
            .is_err()
        );
        for url in [
            "http://example.test",
            "https://user:secret@example.test",
            "https://example.test/?key=secret",
        ] {
            assert!(public_url(url.into()).is_err());
        }
        assert!(
            Settings::parse(
                Some("public-fixture-token".into()),
                Some("0".into()),
                Some("https://example.test".into()),
                None,
                31337,
                Address::ZERO
            )
            .is_err()
        );
    }
    #[test]
    fn only_successful_accepted_proofs_create_public_messages() {
        let mut request = crate::abi::Request {
            fulfilled: true,
            ..Default::default()
        };
        for (status, kind) in [
            (0, "fulfill"),
            (0, "fulfill_batch"),
            (1, "cancel"),
            (1, "epoch"),
        ] {
            assert!(
                ProofAccepted::from_receipt(status, kind, U256::from(7), B256::ZERO, &request)
                    .is_none()
            );
        }
        assert!(
            ProofAccepted::from_receipt(1, "fulfill_batch", U256::from(7), B256::ZERO, &request)
                .is_some()
        );
        request.fulfilled = false;
        assert!(
            ProofAccepted::from_receipt(1, "fulfill", U256::from(7), B256::ZERO, &request)
                .is_none()
        );
        request.fulfilled = true;
        let proof = ProofAccepted::from_receipt(
            1,
            "fulfill",
            U256::from(7),
            B256::repeat_byte(2),
            &request,
        )
        .unwrap();
        let payload = proof.payload(&settings().links);
        let text = payload["content"].as_str().unwrap();
        assert!(text.contains("Proof verified"));
        assert!(!text.contains("delivered")); // A paid accepted proof need not have a successful app callback.
        assert!(text.contains("/explorer/request/31337/"));
        assert!(text.len() < 2000);
        assert_eq!(payload["allowed_mentions"]["parse"], json!([]));
        assert_eq!(payload["nonce"].as_str().unwrap().len(), 24);
        assert_eq!(payload, proof.payload(&settings().links));
    }
    #[tokio::test]
    async fn bounded_queue_never_waits_for_a_sender() {
        let (tx, mut rx) = mpsc::channel(CAPACITY);
        let task = tokio::spawn(std::future::pending::<()>());
        let notifier = Notifier {
            tx,
            _task: Arc::new(Task(task.abort_handle())),
        };
        let request = crate::abi::Request {
            fulfilled: true,
            ..Default::default()
        };
        let proof =
            ProofAccepted::from_receipt(1, "fulfill", U256::from(1), B256::ZERO, &request).unwrap();
        for _ in 0..1000 {
            notifier.notify(proof.clone());
        }
        assert_eq!(rx.len(), CAPACITY);
        assert!(rx.recv().await.is_some());
    }
    #[tokio::test]
    async fn rate_limit_retries_the_identical_public_payload_without_mentions() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/messages", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut payloads = Vec::new();
            for index in 0..2 {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let count = socket.read(&mut buffer).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    if let Some(at) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&bytes[..at]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap();
                        if bytes.len() >= at + 4 + length {
                            assert!(
                                headers
                                    .to_lowercase()
                                    .contains("authorization: bot public-fixture-token")
                            );
                            payloads.push(
                                serde_json::from_slice::<Value>(&bytes[at + 4..at + 4 + length])
                                    .unwrap(),
                            );
                            break;
                        }
                    }
                }
                let (status, body) = if index == 0 {
                    ("429 Too Many Requests", r#"{"retry_after":0.01}"#)
                } else {
                    ("200 OK", r#"{"id":"1"}"#)
                };
                write!(socket,"HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
            payloads
        });
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let (tx, rx) = mpsc::channel(1);
        let request = crate::abi::Request {
            fulfilled: true,
            ..Default::default()
        };
        tx.send(
            ProofAccepted::from_receipt(
                1,
                "fulfill",
                U256::from(9),
                B256::repeat_byte(3),
                &request,
            )
            .unwrap(),
        )
        .await
        .unwrap();
        drop(tx);
        tokio::time::timeout(
            Duration::from_secs(5),
            send_loop(client, endpoint, settings(), rx),
        )
        .await
        .unwrap();
        let payloads = server.join().unwrap();
        assert_eq!(payloads.len(), 2);
        assert_eq!(payloads[0], payloads[1]);
        assert_eq!(payloads[0]["allowed_mentions"]["parse"], json!([]));
        assert_eq!(payloads[0]["enforce_nonce"], true);
    }
}
