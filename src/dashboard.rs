//! Ephemeral, capability-authenticated loopback inspection server.
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use serde::Deserialize;
use serde_json::{Value, json};
use signal_hook::consts::{SIGINT, SIGTERM};

use crate::{
    client,
    protocol::{Failure, Request, Response, random_id, validate_id},
};

const MAX_HEADER: usize = 16_384;
const MAX_BODY: usize = 4096;
const MAX_CLIENTS: usize = 32;
const DEADLINE: Duration = Duration::from_secs(3);

struct Dashboard {
    runtime: PathBuf,
    authority: String,
    origin: String,
    token: String,
    nonce: String,
}

struct HttpRequest {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

struct HttpResponse {
    status: u16,
    kind: &'static str,
    body: Vec<u8>,
}

impl HttpResponse {
    fn json(status: u16, data: &Value) -> Self {
        Self {
            status,
            kind: "application/json",
            body: data.to_string().into_bytes(),
        }
    }

    fn error(status: u16) -> Self {
        let message = match status {
            401 => "Open the private dashboard link printed by dashboard serve.",
            403 => "Dashboard origin or host rejected.",
            404 => "Dashboard endpoint not found.",
            405 => "Method not allowed.",
            413 => "Request exceeds dashboard limits.",
            503 => "Local service is unavailable.",
            _ => "Invalid dashboard request.",
        };
        Self::json(status, &json!({"error": message}))
    }
}

pub fn serve(runtime: &Path, port: u16) -> Result<(), Failure> {
    service_request(runtime, &Request::Ping {})?;
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    listener.set_nonblocking(true)?;
    let authority = listener.local_addr()?.to_string();
    let dashboard = Arc::new(Dashboard {
        runtime: runtime.to_owned(),
        origin: format!("http://{authority}"),
        authority,
        token: format!("{}{}", random_id()?, random_id()?),
        nonce: random_id()?,
    });
    let stopping = Arc::new(AtomicBool::new(false));
    let interrupt = signal_hook::flag::register(SIGINT, Arc::clone(&stopping))?;
    let terminate = signal_hook::flag::register(SIGTERM, Arc::clone(&stopping))?;
    println!(
        "{}",
        json!({"url":format!("{}/#token={}",dashboard.origin,dashboard.token),"status":"listening"})
    );
    std::io::stdout().flush()?;
    let mut clients: Vec<thread::JoinHandle<()>> = Vec::new();
    while !stopping.load(Ordering::Relaxed) {
        clients.retain(|handle| !handle.is_finished());
        match listener.accept() {
            Ok((stream, peer)) if peer.ip().is_loopback() && clients.len() < MAX_CLIENTS => {
                let dashboard = Arc::clone(&dashboard);
                clients.push(thread::spawn(move || handle_connection(stream, &dashboard)));
            }
            Ok(_) => (), // Drop excess clients without admitting unbounded work.
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
    drop(listener);
    for handle in clients {
        let _ = handle.join();
    }
    signal_hook::low_level::unregister(interrupt);
    signal_hook::low_level::unregister(terminate);
    Ok(())
}

fn handle_connection(mut stream: TcpStream, dashboard: &Dashboard) {
    // Accepted sockets can inherit the listener's nonblocking mode on Unix.
    // Request and response deadlines require blocking I/O here.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let response = read_request(&mut stream)
        .map_or_else(HttpResponse::error, |request| dashboard.route(&request));
    let reason = match response.status {
        200 => "OK",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        503 => "Service Unavailable",
        _ => "Bad Request",
    };
    let headers = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nX-Frame-Options: DENY\r\nCross-Origin-Resource-Policy: same-origin\r\nContent-Security-Policy: default-src 'none'; script-src 'nonce-{}'; style-src 'nonce-{}'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'\r\n\r\n",
        response.status,
        response.kind,
        response.body.len(),
        dashboard.nonce,
        dashboard.nonce,
    );
    if stream.set_write_timeout(Some(DEADLINE)).is_ok()
        && stream.write_all(headers.as_bytes()).is_ok()
    {
        let _ = stream.write_all(&response.body);
    }
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, u16> {
    let deadline = Instant::now() + DEADLINE;
    let mut header = Vec::new();
    let mut byte = [0];
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() >= MAX_HEADER {
            return Err(413);
        }
        read_before(stream, &mut byte, deadline)?;
        header.push(byte[0]);
    }
    let header = std::str::from_utf8(&header).map_err(|_| 400_u16)?;
    let mut lines = header.split("\r\n");
    let mut first = lines.next().ok_or(400_u16)?.split(' ');
    let method = first.next().ok_or(400_u16)?.to_owned();
    let target = first.next().ok_or(400_u16)?.to_owned();
    if first.next() != Some("HTTP/1.1") || first.next().is_some() || target.len() > 2048 {
        return Err(400);
    }
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(400_u16)?;
        if name.is_empty()
            || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || value.bytes().any(|b| b.is_ascii_control() && b != b'\t')
            || headers
                .insert(name.to_ascii_lowercase(), value.trim().to_owned())
                .is_some()
        {
            return Err(400);
        }
    }
    if headers.contains_key("transfer-encoding") || headers.contains_key("expect") {
        return Err(400);
    }
    let length = headers.get("content-length").map_or(Ok(0), |value| {
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err(400_u16);
        }
        value.parse::<usize>().map_err(|_| 413_u16)
    })?;
    if length > MAX_BODY {
        return Err(413);
    }
    let mut body = vec![0; length];
    // Each read uses the remaining total deadline, not a renewable per-byte timeout.
    for byte in &mut body {
        read_before(stream, std::slice::from_mut(byte), deadline)?;
    }
    Ok(HttpRequest {
        method,
        target,
        headers,
        body,
    })
}

fn read_before(stream: &mut TcpStream, bytes: &mut [u8], deadline: Instant) -> Result<(), u16> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .ok_or(400_u16)?;
    if left.is_zero() {
        return Err(400);
    }
    stream.set_read_timeout(Some(left)).map_err(|_| 400_u16)?;
    stream.read_exact(bytes).map_err(|_| 400_u16)
}

impl Dashboard {
    fn route(&self, request: &HttpRequest) -> HttpResponse {
        let header = |name| request.headers.get(name).map(String::as_str);
        if header("host") != Some(self.authority.as_str())
            || header("origin").is_some_and(|origin| origin != self.origin)
            || header("sec-fetch-site").is_some_and(|site| !matches!(site, "same-origin" | "none"))
        {
            return HttpResponse::error(403);
        }
        if request.target == "/" && request.method == "GET" && request.body.is_empty() {
            return HttpResponse {
                status: 200,
                kind: "text/html",
                body: include_str!("dashboard.html")
                    .replace("__NONCE__", &self.nonce)
                    .into_bytes(),
            };
        }
        if !authorized(header("authorization"), &self.token) {
            return HttpResponse::error(401);
        }
        if request.method == "POST" {
            if header("origin") != Some(self.origin.as_str())
                || header("content-type") != Some("application/json")
            {
                return HttpResponse::error(403);
            }
        } else if request.method != "GET" {
            return HttpResponse::error(405);
        }
        if request.method == "GET" && !request.body.is_empty() {
            return HttpResponse::error(400);
        }
        self.api(request).unwrap_or_else(|error| {
            if matches!(
                error.code.as_str(),
                "io" | "service_unavailable" | "disconnected" | "protocol"
            ) {
                HttpResponse::error(503)
            } else {
                HttpResponse::json(400, &json!({"error":error.message,"code":error.code}))
            }
        })
    }

    fn api(&self, request: &HttpRequest) -> Result<HttpResponse, Failure> {
        let data = match (request.method.as_str(), request.target.as_str()) {
            ("GET", "/api/status") => json!({
                "service":service_request(&self.runtime, &Request::Ping {})?,
                "sessions":service_request(&self.runtime, &Request::Status { session: None })?.get("sessions").cloned().unwrap_or_default(),
            }),
            ("GET", "/api/events") => {
                service_request(&self.runtime, &Request::Events { session: None })?
            }
            ("GET", target) if target.starts_with("/api/analytics?days=") => {
                let days = match target.strip_prefix("/api/analytics?days=") {
                    Some("1") => 1,
                    Some("7") => 7,
                    Some("30") => 30,
                    Some("90") => 90,
                    _ => return Ok(HttpResponse::error(400)),
                };
                service_request(&self.runtime, &Request::Analytics { days })?
            }
            ("GET", target) if target.starts_with("/api/inspect?session=") => {
                let session = target.trim_start_matches("/api/inspect?session=");
                validate_id(session)?;
                service_request(
                    &self.runtime,
                    &Request::Inspect {
                        session: session.into(),
                    },
                )?
            }
            ("POST", "/api/session/stop" | "/api/session/refresh") => {
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct SessionBody {
                    session: String,
                }
                let body: SessionBody = serde_json::from_slice(&request.body).map_err(|_| {
                    Failure::new("invalid_request", "Expected a session identifier.")
                })?;
                validate_id(&body.session)?;
                let query = if request.target.ends_with("/stop") {
                    Request::Stop {
                        session: body.session,
                    }
                } else {
                    Request::Refresh {
                        session: body.session,
                    }
                };
                service_request(&self.runtime, &query)?
            }
            _ => return Ok(HttpResponse::error(404)),
        };
        Ok(HttpResponse::json(200, &data))
    }
}

fn authorized(header: Option<&str>, token: &str) -> bool {
    let Some(candidate) = header.and_then(|header| header.strip_prefix("Bearer ")) else {
        return false;
    };
    if candidate.len() != token.len() {
        return false;
    }
    candidate
        .bytes()
        .zip(token.bytes())
        .fold(0_u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

fn service_request(runtime: &Path, query: &Request) -> Result<Value, Failure> {
    match client::request(runtime, query)? {
        Response::Ok { data } => Ok(data),
        Response::Error { code, message } => Err(Failure { code, message }),
        _ => Err(Failure::new("protocol", "Unexpected service response.")),
    }
}
