use std::env;
use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener as TokioUnixListener, UnixStream as TokioUnixStream};
use tokio_uring::buf::BoundedBuf;
use tokio_uring::net::{TcpListener, TcpStream};

use crate::ivf_index::ExactIndex;
use crate::parser::parse_transaction;
use crate::response;
use crate::types::StageNanos;
use crate::vectorize::{normalized_vector, Normalization};

const READ_BUF: usize = 8 * 1024;

pub struct App {
    norm: Normalization,
    index: ExactIndex,
    ready: bool,
}

impl App {
    pub fn new(norm: Normalization, index: ExactIndex) -> Self {
        Self {
            norm,
            index,
            ready: false,
        }
    }

    pub fn warmup(&mut self) {
        let q = [0.0f32; 14];
        for _ in 0..64 {
            let _ = self.index.query(&q);
        }
        self.ready = true;
    }

    fn fraud_response(&self, body: &[u8]) -> &'static [u8] {
        let tx = parse_transaction(body);
        if !tx.parse_ok {
            return response::fraud_http(5);
        }
        let q = normalized_vector(&tx, &self.norm);
        response::fraud_http(self.index.query(&q).min(5))
    }

    pub fn classify_for_bench(&self, body: &[u8]) -> (usize, StageNanos) {
        let total = Instant::now();
        let t = Instant::now();
        let tx = parse_transaction(body);
        let json_parse = t.elapsed().as_nanos();
        let t = Instant::now();
        let q = normalized_vector(&tx, &self.norm);
        let vectorize_ns = t.elapsed().as_nanos();
        let t = Instant::now();
        let score = if tx.parse_ok {
            self.index.query(&q).min(5)
        } else {
            5
        };
        let ann = t.elapsed().as_nanos();
        (
            score,
            StageNanos {
                json_parse,
                vectorize: vectorize_ns,
                ann,
                total: total.elapsed().as_nanos(),
                ..StageNanos::default()
            },
        )
    }
}

pub fn serve(addr: &str, app: App) -> io::Result<()> {
    let sock_addr: SocketAddr = addr
        .parse()
        .map_err(|e: std::net::AddrParseError| io::Error::new(ErrorKind::InvalidInput, e))?;
    let uds_path = match env::var("API_SOCKET") {
        Ok(path) if !path.is_empty() => Some(PathBuf::from(path)),
        _ => None,
    };

    tokio_uring::start(async move {
        let tcp_listener = TcpListener::bind(sock_addr)?;
        eprintln!("api listening on tcp://{addr}");

        let uds_listener = match uds_path {
            Some(path) => {
                let _ = std::fs::remove_file(&path);
                let uds = TokioUnixListener::bind(&path)?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
                eprintln!("api listening on unix://{}", path.display());
                Some(uds)
            }
            None => None,
        };

        let app = Arc::new(app);
        if let Some(uds_listener) = uds_listener {
            tokio::select! {
                res = accept_tcp(tcp_listener, app.clone()) => res,
                res = accept_uds(uds_listener, app) => res,
            }
        } else {
            accept_tcp(tcp_listener, app).await
        }
    })
}

async fn accept_tcp(listener: TcpListener, app: Arc<App>) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        set_nodelay(stream.as_raw_fd());
        set_quickack(stream.as_raw_fd());
        tokio_uring::spawn(handle_tcp_conn(stream, app.clone()));
    }
}

fn set_nodelay(fd: std::os::fd::RawFd) {
    let one: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_NODELAY,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

fn set_quickack(fd: std::os::fd::RawFd) {
    // SOL_TCP=6, TCP_QUICKACK=12 on Linux. Avoids delayed ACKs on response.
    let one: libc::c_int = 1;
    unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_TCP,
            libc::TCP_QUICKACK,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
    }
}

async fn accept_uds(listener: TokioUnixListener, app: Arc<App>) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(handle_uds_conn(stream, app.clone()));
    }
}

async fn handle_tcp_conn(stream: TcpStream, app: Arc<App>) {
    let mut read_buf = vec![0u8; READ_BUF];
    let mut read_len = 0usize;

    loop {
        while let Some(resp) = next_response(&app, &mut read_buf, &mut read_len) {
            if stream.write_all(resp).await.0.is_err() {
                return;
            }
        }

        if read_len == read_buf.len() {
            return;
        }

        let buf = std::mem::take(&mut read_buf);
        let (result, slice) = stream.read(buf.slice(read_len..)).await;
        read_buf = slice.into_inner();
        match result {
            Ok(0) => {
                return;
            }
            Ok(n) => {
                read_len += n;
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => {
                continue;
            }
            Err(_) => {
                return;
            }
        }
    }
}

async fn handle_uds_conn(mut stream: TokioUnixStream, app: Arc<App>) {
    let mut read_buf = vec![0u8; READ_BUF];
    let mut read_len = 0usize;

    loop {
        while let Some(resp) = next_response(&app, &mut read_buf, &mut read_len) {
            if stream.write_all(resp).await.is_err() {
                return;
            }
        }

        if read_len == read_buf.len() {
            return;
        }

        match stream.read(&mut read_buf[read_len..]).await {
            Ok(0) => {
                return;
            }
            Ok(n) => {
                read_len += n;
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => {
                continue;
            }
            Err(_) => {
                return;
            }
        }
    }
}

fn next_response(app: &App, read_buf: &mut [u8], read_len: &mut usize) -> Option<&'static [u8]> {
    let (parsed, consumed) = parse_http_consumed(&read_buf[..*read_len])?;
    let resp = if parsed.method == b"GET" && parsed.path == b"/ready" {
        if app.ready {
            response::READY
        } else {
            response::fraud_http(5)
        }
    } else if parsed.method == b"POST" && parsed.path == b"/fraud-score" {
        app.fraud_response(parsed.body)
    } else if parsed.method == b"POST" {
        response::fraud_http(5)
    } else {
        response::NOT_FOUND
    };

    if consumed >= *read_len {
        *read_len = 0;
    } else {
        read_buf.copy_within(consumed..*read_len, 0);
        *read_len -= consumed;
    }

    Some(resp)
}

pub struct ParsedHttp<'a> {
    pub method: &'a [u8],
    pub path: &'a [u8],
    pub body: &'a [u8],
}

pub fn parse_http(buf: &[u8]) -> Option<ParsedHttp<'_>> {
    parse_http_consumed(buf).map(|(p, _)| p)
}

fn parse_http_consumed(buf: &[u8]) -> Option<(ParsedHttp<'_>, usize)> {
    let header_end = find_bytes(buf, b"\r\n\r\n")?;
    let request_line_end = find_bytes(&buf[..header_end], b"\r\n")?;
    let line = &buf[..request_line_end];
    let first_sp = memchr(line, b' ')?;
    let second_sp = memchr(&line[first_sp + 1..], b' ')? + first_sp + 1;
    let method = &line[..first_sp];
    let path = &line[first_sp + 1..second_sp];
    let content_length = content_length(&buf[..header_end]).unwrap_or(0);
    let body_start = header_end + 4;
    let body_end = body_start.checked_add(content_length)?;
    if buf.len() < body_end {
        return None;
    }
    Some((
        ParsedHttp {
            method,
            path,
            body: &buf[body_start..body_end],
        },
        body_end,
    ))
}

fn content_length(headers: &[u8]) -> Option<usize> {
    for line in headers.split(|&b| b == b'\n') {
        let line = trim_ascii(line);
        if line.len() >= 15 && line[..15].eq_ignore_ascii_case(b"content-length:") {
            let digits = trim_ascii(&line[15..]);
            let mut n = 0usize;
            for &b in digits {
                if !b.is_ascii_digit() {
                    break;
                }
                n = n.saturating_mul(10).saturating_add((b - b'0') as usize);
            }
            return Some(n.min(READ_BUF));
        }
    }
    None
}

fn trim_ascii(mut s: &[u8]) -> &[u8] {
    while s.first().is_some_and(|b| b.is_ascii_whitespace()) {
        s = &s[1..];
    }
    while s.last().is_some_and(|b| b.is_ascii_whitespace()) {
        s = &s[..s.len() - 1];
    }
    s
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn memchr(buf: &[u8], byte: u8) -> Option<usize> {
    buf.iter().position(|&b| b == byte)
}
