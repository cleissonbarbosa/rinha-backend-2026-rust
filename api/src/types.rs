pub const DIM: usize = 14;
pub const DEFAULT_SCALE: i64 = 32_767;
pub const MCC_BUF: usize = 8;

#[derive(Clone, Copy, Debug)]
pub struct TxInput {
    pub amount: f64,
    pub installments: f64,
    pub customer_avg_amount: f64,
    pub merchant_avg_amount: f64,
    pub tx_count_24h: f64,
    pub km_from_home: f64,
    pub km_from_last_tx: f64,
    pub minutes_since_last_tx: f64,
    pub requested_at_seconds: i64,
    pub hour_of_day: u8,
    pub day_of_week: u8,
    pub mcc: [u8; MCC_BUF],
    pub mcc_len: u8,
    pub is_online: bool,
    pub card_present: bool,
    pub unknown_merchant: bool,
    pub has_last_tx: bool,
    pub has_last_tx_km: bool,
    pub amount_seen: bool,
    pub parse_ok: bool,
}

impl Default for TxInput {
    fn default() -> Self {
        Self {
            amount: 0.0,
            installments: 1.0,
            customer_avg_amount: 0.0,
            merchant_avg_amount: 0.0,
            tx_count_24h: 0.0,
            km_from_home: 0.0,
            km_from_last_tx: 0.0,
            minutes_since_last_tx: 0.0,
            requested_at_seconds: 0,
            hour_of_day: 12,
            day_of_week: 3,
            mcc: [0; MCC_BUF],
            mcc_len: 0,
            is_online: false,
            card_present: true,
            unknown_merchant: true,
            has_last_tx: false,
            has_last_tx_km: false,
            amount_seen: false,
            parse_ok: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct StageNanos {
    pub http_parse: u128,
    pub json_parse: u128,
    pub vectorize: u128,
    pub ann: u128,
    pub total: u128,
}
