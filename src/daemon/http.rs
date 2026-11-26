use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_HEADER_DURATION: Duration = Duration::from_secs(3);
const MAX_WRITE_DURATION: Duration = Duration::from_secs(5);

pub struct HttpRequest {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
    content_length: usize,
    buffered_body: Vec<u8>,
    expect_continue: bool,
    stream: TcpStream,
}

fn write_all_with_deadline(stream: &mut TcpStream, mut data: &[u8], deadline: Instant) -> io::Result<()> {
    while !data.is_empty() {
        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "response write timeout"));
        }
        let _ = stream.set_write_timeout(Some(deadline - now));
        match stream.write(data) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "socket closed")),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

impl HttpRequest {
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn first_header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn content_length(&self) -> usize {
        self.content_length
    }

    pub fn read_body(&mut self, max_bytes: usize, timeout: Duration) -> Result<Vec<u8>, (u16, &'static str)> {
        if self.content_length > max_bytes {
            return Err((413, "Request body too large"));
        }
        if self.content_length == 0 {
            return Ok(Vec::new());
        }
        if self.buffered_body.len() >= self.content_length {
            let mut body = std::mem::take(&mut self.buffered_body);
            body.truncate(self.content_length);
            return Ok(body);
        }
        if self.expect_continue {
            let _ = write_all_with_deadline(
                &mut self.stream,
                b"HTTP/1.1 100 Continue\r\n\r\n",
                Instant::now() + Duration::from_secs(2),
            );
            let _ = self.stream.flush();
            self.expect_continue = false;
        }
        let mut body = Vec::with_capacity(self.content_length);
        body.extend_from_slice(&self.buffered_body);
        self.buffered_body.clear();
        let mut remaining = self.content_length - body.len();
        let deadline = Instant::now() + timeout;
        let mut chunk = [0u8; 8192];
        while remaining > 0 {
            let now = Instant::now();
            if now >= deadline {
                return Err((408, "Request body timeout"));
            }
            let _ = self.stream.set_read_timeout(Some(deadline - now));
            let want = remaining.min(chunk.len());
            match self.stream.read(&mut chunk[..want]) {
                Ok(0) => return Err((400, "Truncated request body")),
                Ok(n) => {
                    body.extend_from_slice(&chunk[..n]);
                    remaining -= n;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    return Err((408, "Request body timeout"));
                }
                Err(_) => return Err((400, "Failed reading request body")),
            }
        }
        Ok(body)
    }

    pub fn respond(mut self, status: u16, headers: &[(&str, &str)], body: &[u8]) {
        let reason = status_reason(status);
        let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
        for (k, v) in headers {
            head.push_str(k);
            head.push_str(": ");
            head.push_str(v);
            head.push_str("\r\n");
        }
        head.push_str(&format!("Content-Length: {}\r\nConnection: close\r\n\r\n", body.len()));
        let deadline = Instant::now() + MAX_WRITE_DURATION;
        if write_all_with_deadline(&mut self.stream, head.as_bytes(), deadline).is_ok()
            && !self.method.eq_ignore_ascii_case("HEAD")
            && !body.is_empty()
        {
            let _ = write_all_with_deadline(&mut self.stream, body, deadline);
        }
        let _ = self.stream.flush();
    }
}

pub struct LoopbackListener {
    listener: TcpListener,
    addr: SocketAddr,
}

impl LoopbackListener {
    pub fn bind(port: u16) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", port))?;
        let addr = listener.local_addr()?;
        Ok(Self { listener, addr })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn accept(&self) -> io::Result<TcpStream> {
        let (stream, _) = self.listener.accept()?;
        Ok(stream)
    }

    pub fn wake(addr: SocketAddr) {
        let _ = TcpStream::connect_timeout(&addr, Duration::from_millis(200));
    }
}

fn find_header_end(buf: &[u8]) -> Option<(usize, usize)> {
    let mut line_start = 0usize;
    let mut i = 0usize;
    while i < buf.len() {
        if buf[i] == b'\n' {
            let line = &buf[line_start..i];
            let trimmed = line.strip_suffix(b"\r").unwrap_or(line);
            if trimmed.is_empty() && line_start > 0 {
                let head_end = if buf[..line_start].ends_with(b"\r\n") {
                    line_start - 2
                } else {
                    line_start - 1
                };
                return Some((head_end, i + 1));
            }
            line_start = i + 1;
        }
        i += 1;
    }
    None
}

pub fn read_request_head(mut stream: TcpStream) -> io::Result<Option<HttpRequest>> {
    let _ = stream.set_nodelay(true);

    let deadline = Instant::now() + MAX_HEADER_DURATION;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let (header_end, body_start) = loop {
        let now = Instant::now();
        if now >= deadline {
            let _ = write_plain_error(&mut stream, 408, "Request header timeout");
            return Ok(None);
        }
        let _ = stream.set_read_timeout(Some(deadline - now));

        let n = match stream.read(&mut chunk) {
            Ok(0) => {
                if buf.is_empty() {
                    return Ok(None);
                }
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "truncated HTTP headers"));
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                let _ = write_plain_error(&mut stream, 408, "Request header timeout");
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        if buf.len() + n > MAX_HEADER_BYTES {
            let _ = write_plain_error(&mut stream, 431, "Request headers too large");
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(found) = find_header_end(&buf) {
            break found;
        }
    };

    let head_bytes = &buf[..header_end];
    let head_str = match std::str::from_utf8(head_bytes) {
        Ok(s) => s,
        Err(_) => {
            let _ = write_plain_error(&mut stream, 400, "Invalid UTF-8 in HTTP headers");
            return Ok(None);
        }
    };

    let mut lines = head_str.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l));
    let Some(req_line) = lines.next() else {
        return Ok(None);
    };
    let mut parts = req_line.split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next()) else {
        let _ = write_plain_error(&mut stream, 400, "Malformed HTTP request line");
        return Ok(None);
    };
    if parts.next().is_some() || !version.starts_with("HTTP/1.") {
        let _ = write_plain_error(&mut stream, 400, "Unsupported HTTP version");
        return Ok(None);
    }

    let mut headers = Vec::new();
    let mut content_length: Option<usize> = None;
    let mut expect_continue = false;
    for line in lines {
        if line.is_empty() || line.starts_with([' ', '\t']) {
            let _ = write_plain_error(&mut stream, 400, "Malformed HTTP header");
            return Ok(None);
        }
        let Some((k, v)) = line.split_once(':') else {
            let _ = write_plain_error(&mut stream, 400, "Malformed HTTP header");
            return Ok(None);
        };
        if k.is_empty() || !k.bytes().all(is_tchar) {
            let _ = write_plain_error(&mut stream, 400, "Invalid HTTP header name");
            return Ok(None);
        }
        let val = v.trim().to_string();
        if k.eq_ignore_ascii_case("Transfer-Encoding") {
            let _ = write_plain_error(&mut stream, 501, "Transfer-Encoding is not supported");
            return Ok(None);
        }
        if k.eq_ignore_ascii_case("Expect") && val.eq_ignore_ascii_case("100-continue") {
            expect_continue = true;
        }
        if k.eq_ignore_ascii_case("Content-Length") {
            if val.is_empty() || !val.bytes().all(|b| b.is_ascii_digit()) {
                let _ = write_plain_error(&mut stream, 400, "Invalid Content-Length header");
                return Ok(None);
            }
            let Ok(len) = val.parse::<usize>() else {
                let _ = write_plain_error(&mut stream, 400, "Invalid Content-Length header");
                return Ok(None);
            };
            if let Some(prev) = content_length {
                if prev != len {
                    let _ = write_plain_error(&mut stream, 400, "Conflicting Content-Length headers");
                    return Ok(None);
                }
            } else {
                content_length = Some(len);
            }
        }
        headers.push((k.to_string(), val));
    }

    let cl = content_length.unwrap_or(0);
    let buffered_body = if body_start < buf.len() {
        buf[body_start..].to_vec()
    } else {
        Vec::new()
    };

    Ok(Some(HttpRequest {
        method: method.to_string(),
        target: target.to_string(),
        headers,
        content_length: cl,
        buffered_body,
        expect_continue,
        stream,
    }))
}

fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(
            b,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
        )
}

fn write_plain_error(stream: &mut TcpStream, status: u16, msg: &str) -> io::Result<()> {
    let reason = status_reason(status);
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{msg}",
        msg.len()
    );
    write_all_with_deadline(stream, resp.as_bytes(), Instant::now() + MAX_WRITE_DURATION)?;
    stream.flush()
}

fn status_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    }
}
