//! A scripted HTTP receiver, for what the keeper reports out of band, its health: it records each request (the line, the
//! headers and the body) and answers with the status a script chose for it, 200 when it chose none.
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One request the receiver was sent.
pub struct Received {
    /// `POST /health`.
    pub line: String,
    /// Header names in lower case, as they came.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// The status the receiver answered with.
    pub status: u16,
}
impl Received {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

pub struct Sink {
    pub url: String,
    requests: Arc<Mutex<Vec<Received>>>,
    statuses: Arc<Mutex<VecDeque<u16>>>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Sink {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Sink {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<Received>>> = Arc::new(Mutex::new(Vec::new()));
        let statuses: Arc<Mutex<VecDeque<u16>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (log, chosen) = (requests.clone(), statuses.clone());
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let (log, chosen) = (log.clone(), chosen.clone());
                tokio::spawn(async move {
                    let mut bytes = Vec::new();
                    let mut buffer = [0; 4096];
                    let (head, body_at) = loop {
                        let size = socket.read(&mut buffer).await.unwrap_or(0);
                        if size == 0 {
                            return;
                        }
                        bytes.extend_from_slice(&buffer[..size]);
                        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&bytes[..at]).into_owned();
                            let length = head
                                .lines()
                                .find_map(|line| {
                                    let (key, value) = line.split_once(':')?;
                                    key.eq_ignore_ascii_case("content-length")
                                        .then(|| value.trim().parse::<usize>().unwrap())
                                })
                                .unwrap_or(0);
                            if bytes.len() >= at + 4 + length {
                                break (head, at + 4);
                            }
                        }
                    };
                    let mut lines = head.lines();
                    let request = lines.next().unwrap_or_default();
                    let line = request
                        .rsplit_once(' ')
                        .map_or(request, |(line, _)| line)
                        .to_owned();
                    let headers = lines
                        .filter_map(|line| line.split_once(':'))
                        .map(|(key, value)| {
                            (key.trim().to_ascii_lowercase(), value.trim().to_owned())
                        })
                        .collect();
                    let status = chosen.lock().unwrap().pop_front().unwrap_or(200);
                    log.lock().unwrap().push(Received {
                        line,
                        headers,
                        body: bytes[body_at..].to_vec(),
                        status,
                    });
                    let answer = format!(
                        "HTTP/1.1 {status} Scripted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(answer.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self {
            url,
            requests,
            statuses,
            server,
        }
    }
    /// The statuses to answer the next requests with, in order; after them, 200.
    pub fn answer_with(&self, statuses: &[u16]) {
        self.statuses
            .lock()
            .unwrap()
            .extend(statuses.iter().copied());
    }
    /// How many requests have come since the last `take`.
    pub fn received(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    pub fn take(&self) -> Vec<Received> {
        std::mem::take(&mut *self.requests.lock().unwrap())
    }
}
