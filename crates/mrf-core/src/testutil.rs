//! Test-only helpers. Compiled for this crate's tests and, via the
//! `test-util` feature, for other crates' tests.
//!
//! `TestServer` is a deliberately tiny HTTP/1.1 server on std `TcpListener`:
//! GET only, one request per connection, fixed routes. It avoids pulling an
//! HTTP server crate into the dependency tree just for tests.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

pub struct TestServer {
    addr: String,
    hits: Arc<AtomicUsize>,
}

impl TestServer {
    /// Start serving `routes` (path -> body) on an ephemeral localhost port.
    /// The accept thread lives until the test process exits.
    pub fn start(routes: Vec<(String, Vec<u8>)>) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().unwrap().to_string();
        let routes: Arc<HashMap<String, Vec<u8>>> = Arc::new(routes.into_iter().collect());
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_srv = Arc::clone(&hits);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let routes = Arc::clone(&routes);
                let hits = Arc::clone(&hits_srv);
                thread::spawn(move || {
                    let _ = handle(stream, &routes, &hits);
                });
            }
        });
        TestServer { addr, hits }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    /// Number of requests served so far (to assert caching behavior).
    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

fn handle(
    stream: TcpStream,
    routes: &HashMap<String, Vec<u8>>,
    hits: &AtomicUsize,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    // Drain headers up to the blank line. Requests without a User-Agent
    // are refused with 403, like some real payer CDNs do.
    let mut has_user_agent = false;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if line.to_ascii_lowercase().starts_with("user-agent:") {
            has_user_agent = true;
        }
    }
    hits.fetch_add(1, Ordering::SeqCst);
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let mut out = stream;
    if !has_user_agent {
        write!(
            out,
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
        return out.flush();
    }
    match routes.get(path) {
        Some(body) => {
            write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )?;
            out.write_all(body)?;
        }
        None => {
            write!(
                out,
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )?;
        }
    }
    out.flush()
}
