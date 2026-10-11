//! A minimal HTTP/1.1 server for the local services.
//!
//! One request per connection, `Connection: close`, and a hand-rolled parser.
//! These are local or trusted services exposing a handful of routes; anything
//! larger belongs behind a real server.
//!
//! The parsing is separated from the socket so it can be tested directly, which
//! is where the interesting bugs are.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::thread;

use serde_json::Value;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: String,
}

impl Request {
    pub fn query(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Non-empty path segments: `/swaps/abc` yields `swaps`, `abc`.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.path.split('/').filter(|segment| !segment.is_empty())
    }

    /// Route to `method path`, with `:name` segments captured by position.
    ///
    /// Returns the captured values in order, or `None` when nothing matches.
    pub fn matches(&self, method: &str, pattern: &str) -> Option<Vec<String>> {
        if self.method != method {
            return None;
        }

        let pattern: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
        let actual: Vec<&str> = self.segments().collect();
        if pattern.len() != actual.len() {
            return None;
        }

        let mut captures = Vec::new();
        for (want, got) in pattern.iter().zip(actual) {
            match want.strip_prefix(':') {
                Some(_) => captures.push(got.to_owned()),
                None if want == &got => {}
                None => return None,
            }
        }
        Some(captures)
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: String,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, value: &Value) -> Self {
        Self {
            status,
            content_type: "application/json".to_owned(),
            body: value.to_string().into_bytes(),
        }
    }

    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8".to_owned(),
            body: body.into().into_bytes(),
        }
    }

    pub fn html(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/html; charset=utf-8".to_owned(),
            body: body.into().into_bytes(),
        }
    }

    pub fn bytes(status: u16, content_type: &str, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: content_type.to_owned(),
            body,
        }
    }

    pub fn error(status: u16, message: impl Into<String>) -> Self {
        Self::json(status, &serde_json::json!({ "error": message.into() }))
    }

    pub fn not_found() -> Self {
        Self::error(404, "not found")
    }

    pub fn no_content() -> Self {
        Self {
            status: 204,
            content_type: "text/plain; charset=utf-8".to_owned(),
            body: Vec::new(),
        }
    }
}

/// Serve requests until the process stops. One thread per connection.
pub fn serve<H>(listen: &str, handler: H) -> std::io::Result<()>
where
    H: Fn(Request) -> Response + Send + Sync + 'static,
{
    let listener = TcpListener::bind(listen)?;
    eprintln!("listening on http://{listen}");
    serve_on(listener, handler)
}

/// Serve on an already bound listener, so a caller (or a test) can pick the
/// port.
pub fn serve_on<H>(listener: TcpListener, handler: H) -> std::io::Result<()>
where
    H: Fn(Request) -> Response + Send + Sync + 'static,
{
    let handler = Arc::new(handler);
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let handler = Arc::clone(&handler);
                thread::spawn(move || {
                    if let Err(e) = handle(stream, &*handler) {
                        eprintln!("connection: {e}");
                    }
                });
            }
            Err(e) => eprintln!("accept: {e}"),
        }
    }
    Ok(())
}

fn handle<H>(mut stream: TcpStream, handler: &H) -> std::io::Result<()>
where
    H: Fn(Request) -> Response,
{
    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(request) = read_request(&mut reader)? else {
        return Ok(());
    };

    // Answer preflights here so no route has to think about them. `text/plain`
    // posts from a page are simple requests, but not every caller is a page.
    let response = match preflight(&request) {
        Some(response) => response,
        None => handler(request),
    };

    let head = format!(
        "HTTP/1.1 {} {}\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Access-Control-Allow-Origin: *\r\n\
         Access-Control-Allow-Headers: Content-Type\r\n\
         Access-Control-Allow-Methods: GET, POST, OPTIONS\r\n\
         Connection: close\r\n\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len()
    );

    stream.write_all(head.as_bytes())?;
    stream.write_all(&response.body)?;
    stream.flush()
}

/// A response from [`fetch`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Fetched {
    pub status: u16,
    pub body: String,
}

/// One HTTP request, for talking to a sibling service.
///
/// As small as the server side: plain HTTP, `Connection: close`, no redirects,
/// no chunked requests, and the body is treated as text.
pub fn fetch(method: &str, url: &str, body: Option<&str>) -> std::io::Result<Fetched> {
    let invalid = |message: String| std::io::Error::new(std::io::ErrorKind::InvalidInput, message);

    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| invalid(format!("only http:// is supported: {url}")))?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_owned()),
    };
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .map_err(|_| invalid(format!("bad port in {url}")))?,
        ),
        None => (authority, 80),
    };

    let mut stream = TcpStream::connect((host, port))?;
    let body = body.unwrap_or("");
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\n\
         Content-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;

    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| invalid("malformed HTTP response".to_owned()))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);

    Ok(Fetched {
        status,
        body: String::from_utf8_lossy(&raw[split + 4..]).into_owned(),
    })
}

/// Answer a CORS preflight without troubling any route.
fn preflight(request: &Request) -> Option<Response> {
    (request.method == "OPTIONS").then(Response::no_content)
}

/// Read one request. `None` when the peer closed without sending one.
pub fn read_request(reader: &mut impl BufRead) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }

    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or("/").to_owned();

    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path.to_owned(), parse_query(query)),
        None => (target, Vec::new()),
    };

    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        if header.trim().is_empty() {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let body = if content_length > 0 {
        let mut buffer = vec![0u8; content_length];
        reader.read_exact(&mut buffer)?;
        String::from_utf8_lossy(&buffer).into_owned()
    } else {
        String::new()
    };

    Ok(Some(Request {
        method,
        path,
        query,
        body,
    }))
}

fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) => (percent_decode(key), percent_decode(value)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// Content type for a static file, by extension.
pub fn content_type_for(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        422 => "Unprocessable Entity",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        _ => "OK",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(raw: &str) -> Request {
        let mut reader = Cursor::new(raw.as_bytes().to_vec());
        read_request(&mut reader).unwrap().unwrap()
    }

    #[test]
    fn parses_a_get_with_a_query() {
        let request = parse("GET /swaps?claim_pubkey=02ab&limit=5 HTTP/1.1\r\nHost: x\r\n\r\n");
        assert_eq!(request.method, "GET");
        assert_eq!(request.path, "/swaps");
        assert_eq!(request.query("claim_pubkey"), Some("02ab"));
        assert_eq!(request.query("limit"), Some("5"));
        assert_eq!(request.query("missing"), None);
    }

    #[test]
    fn parses_a_post_body_by_content_length() {
        let request = parse("POST /tx HTTP/1.1\r\nContent-Length: 5\r\n\r\nhelloextra");
        assert_eq!(request.method, "POST");
        assert_eq!(request.body, "hello");
    }

    #[test]
    fn the_header_name_case_does_not_matter() {
        let request = parse("POST /tx HTTP/1.1\r\ncontent-LENGTH: 2\r\n\r\nhi");
        assert_eq!(request.body, "hi");
    }

    #[test]
    fn decodes_percent_escapes_in_the_query() {
        let request = parse("GET /q?invoice=lnbcrt1%2Babc%2Fdef&note=a+b HTTP/1.1\r\n\r\n");
        assert_eq!(request.query("invoice"), Some("lnbcrt1+abc/def"));
        assert_eq!(request.query("note"), Some("a b"));
    }

    #[test]
    fn an_empty_read_is_not_a_request() {
        let mut reader = Cursor::new(Vec::new());
        assert!(read_request(&mut reader).unwrap().is_none());
    }

    #[test]
    fn preflights_are_answered_without_a_route() {
        assert!(preflight(&parse("OPTIONS /tx HTTP/1.1\r\n\r\n")).is_some());
        assert!(preflight(&parse("GET /tx HTTP/1.1\r\n\r\n")).is_none());

        let response = preflight(&parse("OPTIONS /tx HTTP/1.1\r\n\r\n")).unwrap();
        assert_eq!(response.status, 204);
        assert!(response.body.is_empty());
    }

    #[test]
    fn matches_literal_and_captured_segments() {
        let request = parse("GET /swaps/abc123 HTTP/1.1\r\n\r\n");
        assert_eq!(
            request.matches("GET", "/swaps/:id"),
            Some(vec!["abc123".to_owned()])
        );
        assert_eq!(request.matches("POST", "/swaps/:id"), None);
        assert_eq!(request.matches("GET", "/swaps"), None);
        assert_eq!(request.matches("GET", "/swaps/:id/extra"), None);
        assert_eq!(
            request.matches("GET", "/:collection/abc123"),
            Some(vec!["swaps".to_owned()])
        );
    }

    #[test]
    fn matches_captures_in_order() {
        let request = parse("GET /a/1/b/2 HTTP/1.1\r\n\r\n");
        assert_eq!(
            request.matches("GET", "/a/:first/b/:second"),
            Some(vec!["1".to_owned(), "2".to_owned()])
        );
    }

    #[test]
    fn responses_carry_json_and_cors() {
        let response = Response::json(200, &serde_json::json!({"ok": true}));
        assert_eq!(response.status, 200);
        assert_eq!(response.content_type, "application/json");
        assert_eq!(String::from_utf8(response.body).unwrap(), r#"{"ok":true}"#);
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(
            content_type_for("a/app.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type_for("a/taker.wasm"), "application/wasm");
        assert_eq!(content_type_for("a/index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type_for("a/thing.unknown"),
            "application/octet-stream"
        );
    }

    /// The client and the server, against each other, on a real socket.
    #[test]
    fn fetch_round_trips_against_our_own_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        thread::spawn(move || {
            let _ = serve_on(listener, |request| match request.matches("POST", "/echo") {
                Some(_) => Response::text(200, request.body),
                None => Response::error(404, "no"),
            });
        });

        let base = format!("http://{address}");
        let echoed = fetch("POST", &format!("{base}/echo"), Some("hello")).unwrap();
        assert_eq!(echoed.status, 200);
        assert_eq!(echoed.body, "hello");

        let missing = fetch("GET", &format!("{base}/nope"), None).unwrap();
        assert_eq!(missing.status, 404);
        assert!(missing.body.contains("no"));
    }

    #[test]
    fn fetch_rejects_a_url_it_cannot_use() {
        assert!(fetch("GET", "https://example.com/x", None).is_err());
        assert!(fetch("GET", "http://127.0.0.1:notaport/x", None).is_err());
    }
}
