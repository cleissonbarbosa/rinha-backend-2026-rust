pub const READY: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nContent-Length: 14\r\n\r\n{\"ready\":true}";
pub const NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nContent-Length: 21\r\n\r\n{\"error\":\"not_found\"}";

const SCORE0: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.0}";
const SCORE1: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.2}";
const SCORE2: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nContent-Length: 35\r\n\r\n{\"approved\":true,\"fraud_score\":0.4}";
const SCORE3: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":0.6}";
const SCORE4: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":0.8}";
const SCORE5: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: keep-alive\r\nContent-Length: 36\r\n\r\n{\"approved\":false,\"fraud_score\":1.0}";

pub fn fraud_http(score: usize) -> &'static [u8] {
    match score {
        0 => SCORE0,
        1 => SCORE1,
        2 => SCORE2,
        3 => SCORE3,
        4 => SCORE4,
        _ => SCORE5,
    }
}
