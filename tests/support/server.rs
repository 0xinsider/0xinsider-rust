//! A scripted HTTP/1.1 server on a loopback port: it answers each connection
//! with the next reply in its script and records every request it received.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One request as the server received it.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub method: String,
    /// The request target: path and query.
    pub target: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// One scripted answer. `chunks` are written one by one with a short pause, and
/// the connection is closed after the last, which ends the body.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub chunks: Vec<Vec<u8>>,
}

impl Reply {
    pub fn new(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            chunks: Vec::new(),
        }
    }

    pub fn json(status: u16, body: &serde_json::Value) -> Self {
        Self::new(status)
            .header("Content-Type", "application/json")
            .body(serde_json::to_vec(body).unwrap())
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.chunks = vec![body.into()];
        self
    }

    pub fn chunks<I: IntoIterator<Item = &'static str>>(mut self, chunks: I) -> Self {
        self.chunks = chunks.into_iter().map(|c| c.as_bytes().to_vec()).collect();
        self
    }
}

pub struct Server {
    pub base_url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Server {
    pub async fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(
            script.into_iter().collect::<std::collections::VecDeque<_>>(),
        ));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let reply = script
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Reply::new(500).body("script exhausted"));
                let recorded = Arc::clone(&recorded);
                tokio::spawn(async move { serve(stream, reply, recorded).await });
            }
        });
        Self { base_url, requests }
    }

    pub fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve(mut stream: TcpStream, reply: Reply, recorded: Arc<Mutex<Vec<Recorded>>>) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
        let Ok(n) = stream.read(&mut chunk).await else { return };
        if n == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default().to_owned();
    let target = request_line.next().unwrap_or_default().to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        })
        .collect();
    let length: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let Ok(n) = stream.read(&mut chunk).await else { return };
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    recorded.lock().unwrap().push(Recorded {
        method,
        target,
        headers,
        body,
    });

    let mut out = format!("HTTP/1.1 {} Scripted\r\nConnection: close\r\n", reply.status);
    for (name, value) in &reply.headers {
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if reply.chunks.len() <= 1 {
        let body = reply.chunks.first().cloned().unwrap_or_default();
        out.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        let _ = stream.write_all(out.as_bytes()).await;
        let _ = stream.write_all(&body).await;
    } else {
        out.push_str("\r\n");
        let _ = stream.write_all(out.as_bytes()).await;
        for part in &reply.chunks {
            let _ = stream.write_all(part).await;
            let _ = stream.flush().await;
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    }
    let _ = stream.shutdown().await;
}
