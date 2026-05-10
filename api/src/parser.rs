use crate::types::{TxInput, MCC_BUF};

const MAX_BODY: usize = 64 * 1024;
const KNOWN_CAP: usize = 64;
const ID_BUF: usize = 32;

pub fn parse_transaction(body: &[u8]) -> TxInput {
    let mut tx = TxInput::default();
    if body.is_empty() || body.len() > MAX_BODY {
        tx.parse_ok = false;
        return tx;
    }

    // Stack scratch for known_merchants vs merchant.id comparison.
    let mut merchant_id = [0u8; ID_BUF];
    let mut merchant_id_len: usize = 0;
    let mut known_ids: [[u8; ID_BUF]; KNOWN_CAP] = [[0u8; ID_BUF]; KNOWN_CAP];
    let mut known_lens: [u8; KNOWN_CAP] = [0u8; KNOWN_CAP];
    let mut known_count: usize = 0;
    let mut last_tx_seconds: i64 = 0;
    let mut last_tx_seen = false;

    let mut p = Parser { body, pos: 0 };
    p.skip_ws();
    if p.peek() == Some(b'{') {
        p.pos += 1;
        while let Some(key) = p.next_key() {
            if !p.expect_colon() {
                tx.parse_ok = false;
                return tx;
            }
            match key {
                b"transaction" => parse_transaction_block(&mut p, &mut tx),
                b"customer" => parse_customer_block(&mut p, &mut tx, &mut known_ids, &mut known_lens, &mut known_count),
                b"merchant" => parse_merchant_block(&mut p, &mut tx, &mut merchant_id, &mut merchant_id_len),
                b"terminal" => parse_terminal_block(&mut p, &mut tx),
                b"last_transaction" => {
                    parse_last_tx_block(&mut p, &mut tx, &mut last_tx_seconds, &mut last_tx_seen);
                }
                _ => p.skip_value(),
            }
        }
    } else {
        tx.parse_ok = false;
        return tx;
    }

    // Resolve unknown_merchant from collected ids.
    let target = &merchant_id[..merchant_id_len];
    if target.is_empty() {
        tx.unknown_merchant = true;
    } else {
        let mut found = false;
        for i in 0..known_count {
            let entry = &known_ids[i][..known_lens[i] as usize];
            if entry == target {
                found = true;
                break;
            }
        }
        tx.unknown_merchant = !found;
    }

    // Resolve minutes_since_last_tx if last_transaction was a non-null object.
    if last_tx_seen {
        let diff = tx.requested_at_seconds.saturating_sub(last_tx_seconds);
        let minutes = (diff as f64) / 60.0;
        tx.minutes_since_last_tx = minutes.max(0.0);
        tx.has_last_tx = true;
    }

    if !tx.amount_seen || !tx.amount.is_finite() || tx.amount < 0.0 {
        tx.parse_ok = false;
    }
    if tx.installments <= 0.0 || !tx.installments.is_finite() {
        tx.installments = 1.0;
    }
    if tx.customer_avg_amount <= 0.0 || !tx.customer_avg_amount.is_finite() {
        tx.customer_avg_amount = tx.amount.max(1.0);
    }
    if tx.merchant_avg_amount <= 0.0 || !tx.merchant_avg_amount.is_finite() {
        tx.merchant_avg_amount = tx.amount.max(1.0);
    }
    if tx.has_last_tx && !tx.has_last_tx_km {
        tx.km_from_last_tx = 0.0;
    }

    tx
}

struct Parser<'a> {
    body: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<u8> {
        self.body.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if matches!(b, b' ' | b'\n' | b'\r' | b'\t') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn next_key(&mut self) -> Option<&'a [u8]> {
        self.skip_ws();
        if self.peek() == Some(b',') {
            self.pos += 1;
            self.skip_ws();
        }
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return None;
        }
        self.parse_string_slice()
    }

    fn expect_colon(&mut self) -> bool {
        self.skip_ws();
        if self.peek() == Some(b':') {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_string_slice(&mut self) -> Option<&'a [u8]> {
        self.skip_ws();
        if self.peek() != Some(b'"') {
            return None;
        }
        self.pos += 1;
        let start = self.pos;
        while self.pos < self.body.len() {
            match self.body[self.pos] {
                b'\\' => {
                    self.pos = (self.pos + 2).min(self.body.len());
                }
                b'"' => {
                    let s = &self.body[start..self.pos];
                    self.pos += 1;
                    return Some(s);
                }
                _ => self.pos += 1,
            }
        }
        None
    }

    fn parse_number(&mut self) -> Option<f64> {
        self.skip_ws();
        let strip = self.peek() == Some(b'"');
        if strip {
            self.pos += 1;
        }
        let start = self.pos;
        if matches!(self.peek(), Some(b'-' | b'+')) {
            self.pos += 1;
        }
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'-' | b'+')) {
                self.pos += 1;
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let raw = &self.body[start..self.pos];
        if strip && self.peek() == Some(b'"') {
            self.pos += 1;
        }
        if raw.is_empty() {
            return None;
        }
        std::str::from_utf8(raw).ok()?.parse().ok()
    }

    fn parse_bool(&mut self) -> Option<bool> {
        self.skip_ws();
        let rest = &self.body[self.pos..];
        if rest.starts_with(b"true") {
            self.pos += 4;
            return Some(true);
        }
        if rest.starts_with(b"false") {
            self.pos += 5;
            return Some(false);
        }
        if let Some(s) = self.parse_string_slice() {
            return match s {
                b"true" | b"1" | b"yes" => Some(true),
                b"false" | b"0" | b"no" => Some(false),
                _ => None,
            };
        }
        None
    }

    fn parse_null(&mut self) -> bool {
        self.skip_ws();
        if self.body[self.pos..].starts_with(b"null") {
            self.pos += 4;
            return true;
        }
        false
    }

    fn skip_value(&mut self) {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => {
                let _ = self.parse_string_slice();
            }
            Some(b'{') => self.skip_object(),
            Some(b'[') => self.skip_array(),
            Some(b't' | b'f') => {
                let _ = self.parse_bool();
            }
            Some(b'n') => {
                let _ = self.parse_null();
            }
            _ => {
                let _ = self.parse_number();
            }
        }
    }

    fn skip_object(&mut self) {
        if self.peek() != Some(b'{') {
            return;
        }
        self.pos += 1;
        let mut depth = 1;
        while self.pos < self.body.len() && depth > 0 {
            match self.body[self.pos] {
                b'"' => {
                    let _ = self.parse_string_slice();
                }
                b'{' | b'[' => {
                    self.pos += 1;
                    depth += 1;
                }
                b'}' | b']' => {
                    self.pos += 1;
                    depth -= 1;
                }
                _ => self.pos += 1,
            }
        }
    }

    fn skip_array(&mut self) {
        if self.peek() != Some(b'[') {
            return;
        }
        self.pos += 1;
        let mut depth = 1;
        while self.pos < self.body.len() && depth > 0 {
            match self.body[self.pos] {
                b'"' => {
                    let _ = self.parse_string_slice();
                }
                b'[' | b'{' => {
                    self.pos += 1;
                    depth += 1;
                }
                b']' | b'}' => {
                    self.pos += 1;
                    depth -= 1;
                }
                _ => self.pos += 1,
            }
        }
    }
}

fn parse_transaction_block(p: &mut Parser, tx: &mut TxInput) {
    p.skip_ws();
    if p.peek() != Some(b'{') {
        p.skip_value();
        return;
    }
    p.pos += 1;
    while let Some(key) = p.next_key() {
        if !p.expect_colon() {
            return;
        }
        match key {
            b"amount" => {
                if let Some(v) = p.parse_number() {
                    tx.amount = v;
                    tx.amount_seen = true;
                } else {
                    p.skip_value();
                }
            }
            b"installments" => {
                if let Some(v) = p.parse_number() {
                    tx.installments = v;
                } else {
                    p.skip_value();
                }
            }
            b"requested_at" => {
                if let Some(s) = p.parse_string_slice() {
                    apply_iso(tx, s, true);
                } else {
                    p.skip_value();
                }
            }
            _ => p.skip_value(),
        }
    }
}

fn parse_customer_block(
    p: &mut Parser,
    tx: &mut TxInput,
    known_ids: &mut [[u8; ID_BUF]; KNOWN_CAP],
    known_lens: &mut [u8; KNOWN_CAP],
    known_count: &mut usize,
) {
    p.skip_ws();
    if p.peek() != Some(b'{') {
        p.skip_value();
        return;
    }
    p.pos += 1;
    while let Some(key) = p.next_key() {
        if !p.expect_colon() {
            return;
        }
        match key {
            b"avg_amount" => {
                if let Some(v) = p.parse_number() {
                    tx.customer_avg_amount = v;
                } else {
                    p.skip_value();
                }
            }
            b"tx_count_24h" => {
                if let Some(v) = p.parse_number() {
                    tx.tx_count_24h = v.max(0.0);
                } else {
                    p.skip_value();
                }
            }
            b"known_merchants" => parse_known_merchants(p, known_ids, known_lens, known_count),
            _ => p.skip_value(),
        }
    }
}

fn parse_known_merchants(
    p: &mut Parser,
    known_ids: &mut [[u8; ID_BUF]; KNOWN_CAP],
    known_lens: &mut [u8; KNOWN_CAP],
    known_count: &mut usize,
) {
    p.skip_ws();
    if p.peek() != Some(b'[') {
        p.skip_value();
        return;
    }
    p.pos += 1;
    loop {
        p.skip_ws();
        match p.peek() {
            Some(b']') => {
                p.pos += 1;
                return;
            }
            Some(b',') => {
                p.pos += 1;
                continue;
            }
            Some(b'"') => {
                if let Some(s) = p.parse_string_slice() {
                    if *known_count < KNOWN_CAP {
                        let len = s.len().min(ID_BUF);
                        known_ids[*known_count][..len].copy_from_slice(&s[..len]);
                        known_lens[*known_count] = len as u8;
                        *known_count += 1;
                    }
                } else {
                    return;
                }
            }
            _ => return,
        }
    }
}

fn parse_merchant_block(
    p: &mut Parser,
    tx: &mut TxInput,
    merchant_id: &mut [u8; ID_BUF],
    merchant_id_len: &mut usize,
) {
    p.skip_ws();
    if p.peek() != Some(b'{') {
        p.skip_value();
        return;
    }
    p.pos += 1;
    while let Some(key) = p.next_key() {
        if !p.expect_colon() {
            return;
        }
        match key {
            b"id" => {
                if let Some(s) = p.parse_string_slice() {
                    let len = s.len().min(ID_BUF);
                    merchant_id[..len].copy_from_slice(&s[..len]);
                    *merchant_id_len = len;
                } else {
                    p.skip_value();
                }
            }
            b"mcc" => {
                if let Some(s) = p.parse_string_slice() {
                    set_mcc_bytes(tx, s);
                } else if let Some(n) = p.parse_number() {
                    let buf = format_mcc_number(n);
                    set_mcc_bytes(tx, &buf);
                } else {
                    p.skip_value();
                }
            }
            b"avg_amount" => {
                if let Some(v) = p.parse_number() {
                    tx.merchant_avg_amount = v;
                } else {
                    p.skip_value();
                }
            }
            _ => p.skip_value(),
        }
    }
}

fn parse_terminal_block(p: &mut Parser, tx: &mut TxInput) {
    p.skip_ws();
    if p.peek() != Some(b'{') {
        p.skip_value();
        return;
    }
    p.pos += 1;
    while let Some(key) = p.next_key() {
        if !p.expect_colon() {
            return;
        }
        match key {
            b"is_online" => {
                if let Some(v) = p.parse_bool() {
                    tx.is_online = v;
                } else {
                    p.skip_value();
                }
            }
            b"card_present" => {
                if let Some(v) = p.parse_bool() {
                    tx.card_present = v;
                } else {
                    p.skip_value();
                }
            }
            b"km_from_home" => {
                if let Some(v) = p.parse_number() {
                    tx.km_from_home = v;
                } else {
                    p.skip_value();
                }
            }
            _ => p.skip_value(),
        }
    }
}

fn parse_last_tx_block(p: &mut Parser, tx: &mut TxInput, last_tx_seconds: &mut i64, last_tx_seen: &mut bool) {
    p.skip_ws();
    if p.parse_null() {
        return;
    }
    if p.peek() != Some(b'{') {
        p.skip_value();
        return;
    }
    p.pos += 1;
    while let Some(key) = p.next_key() {
        if !p.expect_colon() {
            return;
        }
        match key {
            b"timestamp" => {
                if let Some(s) = p.parse_string_slice() {
                    if let Some(secs) = parse_iso_seconds(s) {
                        *last_tx_seconds = secs;
                        *last_tx_seen = true;
                    }
                } else {
                    p.skip_value();
                }
            }
            b"km_from_current" => {
                if let Some(v) = p.parse_number() {
                    tx.km_from_last_tx = v.max(0.0);
                    tx.has_last_tx_km = true;
                } else {
                    p.skip_value();
                }
            }
            _ => p.skip_value(),
        }
    }
}

fn apply_iso(tx: &mut TxInput, value: &[u8], require_seconds: bool) {
    if let Some((hour, dow)) = parse_iso_hour_and_day(value) {
        tx.hour_of_day = hour;
        tx.day_of_week = dow;
    } else if require_seconds {
        tx.parse_ok = false;
    }
    if let Some(seconds) = parse_iso_seconds(value) {
        tx.requested_at_seconds = seconds;
    } else if require_seconds {
        tx.parse_ok = false;
    }
}

fn set_mcc_bytes(tx: &mut TxInput, mcc: &[u8]) {
    tx.mcc = [0; MCC_BUF];
    let len = mcc.len().min(MCC_BUF);
    tx.mcc[..len].copy_from_slice(&mcc[..len]);
    tx.mcc_len = len as u8;
}

fn format_mcc_number(n: f64) -> [u8; MCC_BUF] {
    let mut out = [0u8; MCC_BUF];
    let mut buf = [0u8; 16];
    let s = format!("{}", n as i64);
    let bytes = s.as_bytes();
    let len = bytes.len().min(buf.len());
    buf[..len].copy_from_slice(&bytes[..len]);
    let copy = len.min(MCC_BUF);
    out[..copy].copy_from_slice(&buf[..copy]);
    out
}

fn parse_iso_hour_and_day(s: &[u8]) -> Option<(u8, u8)> {
    if s.len() < 10 {
        return None;
    }
    let year = parse_i32(s.get(0..4)?)?;
    let month = parse_i32(s.get(5..7)?)?;
    let day = parse_i32(s.get(8..10)?)?;
    let hour = s
        .iter()
        .position(|&b| b == b'T' || b == b' ')
        .and_then(|p| parse_i32(s.get(p + 1..p + 3)?))
        .unwrap_or(12)
        .clamp(0, 23) as u8;
    Some((hour, day_of_week(year, month, day)))
}

fn parse_iso_seconds(s: &[u8]) -> Option<i64> {
    if s.len() < 19 {
        return None;
    }
    let year = parse_i32(s.get(0..4)?)?;
    let month = parse_i32(s.get(5..7)?)?;
    let day = parse_i32(s.get(8..10)?)?;
    let time_pos = s.iter().position(|&b| b == b'T' || b == b' ')?;
    let hour = parse_i32(s.get(time_pos + 1..time_pos + 3)?)?;
    let minute = parse_i32(s.get(time_pos + 4..time_pos + 6)?)?;
    let second = parse_i32(s.get(time_pos + 7..time_pos + 9)?)?;
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + i64::from(hour * 3_600 + minute * 60 + second))
}

fn parse_i32(s: &[u8]) -> Option<i32> {
    let mut v = 0i32;
    for &b in s {
        if !b.is_ascii_digit() {
            return None;
        }
        v = v * 10 + i32::from(b - b'0');
    }
    Some(v)
}

fn day_of_week(y: i32, m: i32, d: i32) -> u8 {
    let days = days_from_civil(y, m, d);
    ((days + 3).rem_euclid(7)) as u8
}

fn days_from_civil(mut y: i32, m: i32, d: i32) -> i64 {
    y -= i32::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = m + if m > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i64::from(era * 146_097 + doe - 719_468)
}
