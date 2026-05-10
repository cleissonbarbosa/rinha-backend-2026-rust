use std::collections::HashMap;
use std::env;
use std::io::{self, ErrorKind, Read, Write};
use std::net::SocketAddr;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use mio::event::Source;
use mio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use mio::{Events, Interest, Poll, Registry, Token};

use crate::ivf_index::ExactIndex;
use crate::parser::parse_transaction;
use crate::response;
use crate::types::StageNanos;
use crate::vectorize::{normalized_vector, Normalization};

const READ_BUF: usize = 8 * 1024;
const TCP_LISTEN: Token = Token(0);
const UDS_LISTEN: Token = Token(1);
const FIRST_CLIENT_TOKEN: usize = 16;
const POLL_EVENTS: usize = 1024;

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

enum Stream {
    Tcp(TcpStream),
    Uds(UnixStream),
}

impl Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.read(buf),
            Stream::Uds(s) => s.read(buf),
        }
    }

    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Tcp(s) => s.write(buf),
            Stream::Uds(s) => s.write(buf),
        }
    }

    fn deregister(&mut self, registry: &Registry) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.deregister(registry),
            Stream::Uds(s) => s.deregister(registry),
        }
    }

    fn reregister(
        &mut self,
        registry: &Registry,
        tok: Token,
        interest: Interest,
    ) -> io::Result<()> {
        match self {
            Stream::Tcp(s) => s.reregister(registry, tok, interest),
            Stream::Uds(s) => s.reregister(registry, tok, interest),
        }
    }
}

struct Conn {
    stream: Stream,
    read_buf: Box<[u8; READ_BUF]>,
    read_len: usize,
    write_resp: Option<&'static [u8]>,
    write_pos: usize,
    interest: Interest,
}

impl Conn {
    fn new(stream: Stream) -> Self {
        Self {
            stream,
            read_buf: Box::new([0u8; READ_BUF]),
            read_len: 0,
            write_resp: None,
            write_pos: 0,
            interest: Interest::READABLE,
        }
    }
}

pub fn serve(addr: &str, app: App) -> io::Result<()> {
    let mut poll = Poll::new()?;
    let mut events = Events::with_capacity(POLL_EVENTS);

    let sock_addr: SocketAddr = addr
        .parse()
        .map_err(|e: std::net::AddrParseError| io::Error::new(ErrorKind::InvalidInput, e))?;
    let mut tcp_listener = TcpListener::bind(sock_addr)?;
    poll.registry()
        .register(&mut tcp_listener, TCP_LISTEN, Interest::READABLE)?;
    eprintln!("api listening on tcp://{addr}");

    let mut uds_listener = match env::var("API_SOCKET") {
        Ok(path) if !path.is_empty() => {
            let path = PathBuf::from(path);
            let _ = std::fs::remove_file(&path);
            let mut uds = UnixListener::bind(&path)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666))?;
            poll.registry()
                .register(&mut uds, UDS_LISTEN, Interest::READABLE)?;
            eprintln!("api listening on unix://{}", path.display());
            Some(uds)
        }
        _ => None,
    };

    let app = Arc::new(app);
    let mut conns: HashMap<Token, Conn> = HashMap::new();
    let mut next_token: usize = FIRST_CLIENT_TOKEN;

    loop {
        poll.poll(&mut events, None)?;
        for event in events.iter() {
            match event.token() {
                TCP_LISTEN => {
                    accept_tcp(&mut tcp_listener, &poll, &mut conns, &mut next_token);
                }
                UDS_LISTEN => {
                    if let Some(ref mut uds) = uds_listener {
                        accept_uds(uds, &poll, &mut conns, &mut next_token);
                    }
                }
                tok => {
                    let mut close = false;
                    if let Some(conn) = conns.get_mut(&tok) {
                        if event.is_readable() {
                            handle_read(conn, &app, &mut close);
                        }
                        if !close && event.is_writable() {
                            handle_write(conn, &mut close);
                        }
                        if !close {
                            let want = if conn.write_resp.is_some() {
                                Interest::WRITABLE
                            } else {
                                Interest::READABLE
                            };
                            if want != conn.interest {
                                let _ = conn.stream.reregister(poll.registry(), tok, want);
                                conn.interest = want;
                            }
                        }
                    }
                    if close {
                        if let Some(mut conn) = conns.remove(&tok) {
                            let _ = conn.stream.deregister(poll.registry());
                        }
                    }
                }
            }
        }
    }
}

fn accept_tcp(
    listener: &mut TcpListener,
    poll: &Poll,
    conns: &mut HashMap<Token, Conn>,
    next_token: &mut usize,
) {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nodelay(true);
                set_quickack(stream.as_raw_fd());
                let tok = Token(*next_token);
                *next_token = next_token.wrapping_add(1).max(FIRST_CLIENT_TOKEN);
                if poll
                    .registry()
                    .register(&mut stream, tok, Interest::READABLE)
                    .is_ok()
                {
                    conns.insert(tok, Conn::new(Stream::Tcp(stream)));
                }
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
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

fn accept_uds(
    listener: &mut UnixListener,
    poll: &Poll,
    conns: &mut HashMap<Token, Conn>,
    next_token: &mut usize,
) {
    loop {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let tok = Token(*next_token);
                *next_token = next_token.wrapping_add(1).max(FIRST_CLIENT_TOKEN);
                if poll
                    .registry()
                    .register(&mut stream, tok, Interest::READABLE)
                    .is_ok()
                {
                    conns.insert(tok, Conn::new(Stream::Uds(stream)));
                }
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(_) => break,
        }
    }
}

fn handle_read(conn: &mut Conn, app: &Arc<App>, close: &mut bool) {
    loop {
        if conn.read_len == conn.read_buf.len() {
            // request bigger than buffer; bail
            *close = true;
            return;
        }
        let read_into = &mut conn.read_buf[conn.read_len..];
        match conn.stream.read(read_into) {
            Ok(0) => {
                *close = true;
                return;
            }
            Ok(n) => {
                conn.read_len += n;
                process_buffer(conn, app, close);
                if *close || conn.write_resp.is_some() {
                    return;
                }
                // keep reading until WouldBlock
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => return,
            Err(_) => {
                *close = true;
                return;
            }
        }
    }
}

fn process_buffer(conn: &mut Conn, app: &Arc<App>, close: &mut bool) {
    while let Some((parsed, consumed)) = parse_http_consumed(&conn.read_buf[..conn.read_len]) {
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

        // shift remaining bytes to start
        if consumed >= conn.read_len {
            conn.read_len = 0;
        } else {
            conn.read_buf.copy_within(consumed..conn.read_len, 0);
            conn.read_len -= consumed;
        }

        // try non-blocking write immediately
        match conn.stream.write(resp) {
            Ok(n) if n == resp.len() => {
                // fully written; loop and try parsing next request
                continue;
            }
            Ok(n) => {
                conn.write_resp = Some(resp);
                conn.write_pos = n;
                return;
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                conn.write_resp = Some(resp);
                conn.write_pos = 0;
                return;
            }
            Err(_) => {
                *close = true;
                return;
            }
        }
    }
}

fn handle_write(conn: &mut Conn, close: &mut bool) {
    let Some(resp) = conn.write_resp else { return };
    while conn.write_pos < resp.len() {
        match conn.stream.write(&resp[conn.write_pos..]) {
            Ok(0) => {
                *close = true;
                return;
            }
            Ok(n) => conn.write_pos += n,
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => return,
            Err(_) => {
                *close = true;
                return;
            }
        }
    }
    conn.write_resp = None;
    conn.write_pos = 0;
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
