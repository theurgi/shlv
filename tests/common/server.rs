//! A tiny HTTP/1.1 server on 127.0.0.1 for tests: std only, one thread per connection, every
//! response closes the connection. Tests give it a handler and inspect the requests it saw. No
//! test touches the internet.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// Path and query, as sent.
    pub path: String,
    /// Names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("request body is JSON")
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Wait this long before sending anything.
    pub delay: Duration,
    /// Send the body in pieces of this many bytes, pausing `chunk_pause` between them.
    pub chunk: Option<(usize, Duration)>,
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
            delay: Duration::ZERO,
            chunk: None,
        }
    }

    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        Response::new(200)
            .header("content-type", "text/html; charset=utf-8")
            .body(body)
    }

    pub fn json(v: &serde_json::Value) -> Self {
        Response::new(200)
            .header("content-type", "application/json")
            .body(v.to_string())
    }

    pub fn redirect(status: u16, to: &str) -> Self {
        Response::new(status).header("location", to)
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    pub fn delay(mut self, d: Duration) -> Self {
        self.delay = d;
        self
    }

    pub fn chunked(mut self, size: usize, pause: Duration) -> Self {
        self.chunk = Some((size, pause));
        self
    }
}

type Handler = dyn Fn(&Request) -> Response + Send + Sync;

pub struct Server {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
}

impl Server {
    pub fn start(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handler: Arc<Handler> = Arc::new(handler);
        let (reqs, st) = (requests.clone(), stop.clone());
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                if st.load(Ordering::Relaxed) {
                    break;
                }
                let Ok(conn) = conn else { continue };
                let (handler, reqs) = (handler.clone(), reqs.clone());
                std::thread::spawn(move || serve(conn, &*handler, &reqs));
            }
        });
        Server {
            addr,
            requests,
            stop,
        }
    }

    /// `http://127.0.0.1:PORT` followed by `path`.
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the accept loop so it sees the flag.
        let _ = TcpStream::connect(self.addr);
    }
}

fn serve(conn: TcpStream, handler: &Handler, requests: &Mutex<Vec<Request>>) {
    let mut reader = BufReader::new(conn.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).unwrap_or(0) == 0 {
            break;
        }
        let h = h.trim_end();
        if h.is_empty() {
            break;
        }
        if let Some((n, v)) = h.split_once(':') {
            headers.push((n.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }
    let len = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; len];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let req = Request {
        method,
        path,
        headers,
        body,
    };
    let resp = handler(&req);
    requests.lock().unwrap().push(req);
    std::thread::sleep(resp.delay);
    let mut conn = conn;
    let mut head = format!("HTTP/1.1 {} X\r\n", resp.status);
    for (n, v) in &resp.headers {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str(&format!(
        "content-length: {}\r\nconnection: close\r\n\r\n",
        resp.body.len()
    ));
    if conn.write_all(head.as_bytes()).is_err() {
        return;
    }
    match resp.chunk {
        None => {
            let _ = conn.write_all(&resp.body);
        }
        Some((size, pause)) => {
            for piece in resp.body.chunks(size.max(1)) {
                if conn.write_all(piece).and_then(|_| conn.flush()).is_err() {
                    return;
                }
                std::thread::sleep(pause);
            }
        }
    }
    let _ = conn.flush();
}
