// SPDX-License-Identifier: GPL-3.0-only
//! Test-only helpers. [`FakeServer`] is a tiny local HTTP server that stands in for rclone's rc
//! API and for the Google Drive API: it answers fixed JSON per path and records every request.

use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{Arc, Mutex},
    thread,
};

/// One request the server received.
#[derive(Debug, Clone)]
pub struct Request {
    /// Path and query, e.g. `/drive/v3/files/abc?fields=id`.
    pub target: String,
    pub body: String,
    pub authorization: Option<String>,
}

impl Request {
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }
}

/// A canned response for one path (query strings are ignored when matching).
pub struct Route {
    pub path: &'static str,
    pub status: u16,
    pub body: String,
}

pub fn ok(path: &'static str, body: &str) -> Route {
    Route {
        path,
        status: 200,
        body: body.to_string(),
    }
}

pub fn status(path: &'static str, status: u16) -> Route {
    Route {
        path,
        status,
        body: "{}".into(),
    }
}

pub struct FakeServer {
    /// `127.0.0.1:PORT`
    pub addr: String,
    log: Arc<Mutex<Vec<Request>>>,
}

impl FakeServer {
    /// Start serving `routes` on a free port. Unknown paths get a 404. The server thread lives
    /// until the test process exits.
    pub fn start(routes: Vec<Route>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr").to_string();
        let log = Arc::new(Mutex::new(Vec::new()));
        let routes = Arc::new(routes);
        let thread_log = Arc::clone(&log);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (routes, log) = (Arc::clone(&routes), Arc::clone(&thread_log));
                thread::spawn(move || serve(stream, &routes, &log));
            }
        });
        Self { addr, log }
    }

    pub fn requests(&self) -> Vec<Request> {
        self.log.lock().expect("log").clone()
    }

    /// Paths requested so far, in order.
    pub fn paths(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|r| r.path().to_string())
            .collect()
    }
}

fn serve(stream: TcpStream, routes: &[Route], log: &Mutex<Vec<Request>>) {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let target = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let (mut length, mut authorization) = (0usize, None);
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(v) = lower.strip_prefix("content-length:") {
            length = v.trim().parse().unwrap_or(0);
        } else if lower.starts_with("authorization:") {
            authorization = line.split_once(':').map(|(_, v)| v.trim().to_string());
        }
    }
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let request = Request {
        target,
        body: String::from_utf8_lossy(&body).into_owned(),
        authorization,
    };
    let route = routes.iter().find(|r| r.path == request.path());
    let (code, reply) = route.map_or((404, "{}".to_string()), |r| (r.status, r.body.clone()));
    log.lock().expect("log").push(request);
    let response = format!(
        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
        reply.len()
    );
    let mut stream = reader.into_inner();
    let _ = stream.write_all(response.as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(addr: &str, path: &str) -> Result<String, String> {
        ureq::get(&format!("http://{addr}{path}"))
            .call()
            .map_err(|e| e.to_string())?
            .into_string()
            .map_err(|e| e.to_string())
    }

    #[test]
    fn serves_routes_and_records_requests() {
        let s = FakeServer::start(vec![ok("/a", r#"{"x":1}"#), status("/gone", 404)]);
        assert_eq!(get(&s.addr, "/a?q=1").unwrap(), r#"{"x":1}"#);
        assert!(get(&s.addr, "/gone").is_err());
        assert!(get(&s.addr, "/unknown").is_err());
        assert_eq!(s.paths(), ["/a", "/gone", "/unknown"]);
        assert_eq!(s.requests()[0].target, "/a?q=1");
    }
}
