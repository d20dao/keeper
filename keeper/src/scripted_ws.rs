//! A scripted WebSocket JSON-RPC endpoint for the keeper's chain event subscription. It answers the handshake (the
//! chain id and the two `eth_subscribe` calls), pushes the heads and logs a script tells it to, and drops the
//! connection when told. What the keeper sends it, and each connection, is recorded in the order it came.
use crate::scripted::Chain;
use alloy_primitives::{Address, B256, keccak256};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

enum Command {
    Push(Value),
    Drop,
}

pub struct Socket {
    pub url: String,
    lines: Arc<Mutex<Vec<String>>>,
    commands: mpsc::UnboundedSender<Command>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Socket {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Socket {
    /// An endpoint called `name` in the trace, which answers `eth_chainId` with `serves`.
    pub async fn start(name: &'static str, chain: Arc<Mutex<Chain>>, serves: u64) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let (commands, mut script) = mpsc::unbounded_channel();
        let log = lines.clone();
        let server = tokio::spawn(async move {
            let say = |line: String| log.lock().unwrap().push(line);
            let mut session = 0;
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                session += 1;
                say(format!("{name} session {session}: connected"));
                let Ok(mut socket) = tokio_tungstenite::accept_async(stream).await else {
                    continue;
                };
                let ended = loop {
                    tokio::select! {
                        message = socket.next() => {
                            let Some(Ok(Message::Text(text))) = message else {
                                break "closed by the keeper";
                            };
                            let call: Value = serde_json::from_str(text.as_str()).unwrap();
                            let (heard, result) = {
                                let chain = chain.lock().unwrap();
                                heard(&chain, &call, serves)
                            };
                            say(format!("{name} session {session}: <- {heard}"));
                            let answer = json!({"jsonrpc":"2.0","id":call["id"],"result":result});
                            if socket.send(Message::text(answer.to_string())).await.is_err() {
                                break "closed by the keeper";
                            }
                        }
                        command = script.recv() => match command {
                            Some(Command::Push(value)) => {
                                if socket.send(Message::text(value.to_string())).await.is_err() {
                                    break "closed by the keeper";
                                }
                            }
                            Some(Command::Drop) => {
                                let _ = socket.close(None).await;
                                break "dropped by the endpoint";
                            }
                            None => return,
                        }
                    }
                };
                say(format!("{name} session {session}: {ended}"));
            }
        });
        Self {
            url,
            lines,
            commands,
            server,
        }
    }
    /// A new head, as the endpoint pushes it on the `newHeads` subscription.
    pub fn push_head(&self, number: u64) {
        let _ = self.commands.send(Command::Push(json!({
            "jsonrpc":"2.0","method":"eth_subscription",
            "params":{"subscription":"0xheads","result":{"number":format!("0x{number:x}")}}
        })));
    }
    /// A log of `address` in block `block` on the `logs` subscription. The event is named by its signature.
    pub fn push_log(&self, address: Address, signature: &str, block: u64) {
        let topic: B256 = keccak256(signature);
        let _ = self.commands.send(Command::Push(json!({
            "jsonrpc":"2.0","method":"eth_subscription",
            "params":{"subscription":"0xlogs","result":{
                "address":address,"topics":[topic],"blockNumber":format!("0x{block:x}")}}
        })));
    }
    /// The endpoint closes the connection.
    pub fn drop_connection(&self) {
        let _ = self.commands.send(Command::Drop);
    }
    /// What has been recorded since the last call.
    pub fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.lines.lock().unwrap())
    }
}
/// A call of the handshake as the trace names it, and its answer.
fn heard(chain: &Chain, call: &Value, serves: u64) -> (String, Value) {
    match (call["method"].as_str().unwrap(), call["params"][0].as_str()) {
        ("eth_chainId", _) => ("eth_chainId".into(), json!(format!("0x{serves:x}"))),
        ("eth_subscribe", Some("newHeads")) => ("eth_subscribe newHeads".into(), json!("0xheads")),
        ("eth_subscribe", Some("logs")) => {
            let roles: Vec<String> = call["params"][1]["address"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|address| {
                    chain.role(serde_json::from_value::<Address>(address.clone()).unwrap())
                })
                .collect();
            (
                format!("eth_subscribe logs addresses=[{}]", roles.join(",")),
                json!("0xlogs"),
            )
        }
        (other, _) => (format!("UNSCRIPTED {other}"), Value::Null),
    }
}
