use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::Deserialize;

use crate::types::{TxInput, DEFAULT_SCALE, DIM};

#[derive(Clone, Debug)]
pub struct Normalization {
    max_amount: f64,
    max_installments: f64,
    amount_vs_avg_ratio: f64,
    max_minutes: f64,
    max_km: f64,
    max_tx_count_24h: f64,
    max_merchant_avg_amount: f64,
    scale: i64,
    mcc_risk: HashMap<String, f64>,
}

#[derive(Deserialize)]
struct NormalizationJson {
    max_amount: f64,
    max_installments: f64,
    amount_vs_avg_ratio: f64,
    max_minutes: f64,
    max_km: f64,
    max_tx_count_24h: f64,
    max_merchant_avg_amount: f64,
    scale: Option<i64>,
}

impl Normalization {
    pub fn load(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref();
        let mut result = fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<NormalizationJson>(&bytes).ok())
            .map(Self::from_json)
            .unwrap_or_default();

        let mcc_path = path.with_file_name("mcc_risk.json");
        if let Ok(bytes) = fs::read(mcc_path) {
            if let Ok(values) = serde_json::from_slice::<HashMap<String, f64>>(&bytes) {
                result.mcc_risk = values;
            }
        }

        result
    }

    pub fn scale(&self) -> i64 {
        self.scale
    }

    pub fn quantize_reference_value(&self, value: f64) -> i64 {
        quantize_normalized(value, self.scale)
    }

    fn from_json(json: NormalizationJson) -> Self {
        Self {
            max_amount: positive_or(json.max_amount, 10_000.0),
            max_installments: positive_or(json.max_installments, 12.0),
            amount_vs_avg_ratio: positive_or(json.amount_vs_avg_ratio, 10.0),
            max_minutes: positive_or(json.max_minutes, 1_440.0),
            max_km: positive_or(json.max_km, 1_000.0),
            max_tx_count_24h: positive_or(json.max_tx_count_24h, 20.0),
            max_merchant_avg_amount: positive_or(json.max_merchant_avg_amount, 10_000.0),
            scale: json.scale.unwrap_or(DEFAULT_SCALE).max(128),
            mcc_risk: default_mcc_risk(),
        }
    }

    fn mcc_risk(&self, tx: &TxInput) -> f64 {
        let key = std::str::from_utf8(&tx.mcc[..tx.mcc_len as usize]).unwrap_or("");
        self.mcc_risk.get(key).copied().unwrap_or(0.5).clamp(0.0, 1.0)
    }
}

impl Default for Normalization {
    fn default() -> Self {
        Self {
            max_amount: 10_000.0,
            max_installments: 12.0,
            amount_vs_avg_ratio: 10.0,
            max_minutes: 1_440.0,
            max_km: 1_000.0,
            max_tx_count_24h: 20.0,
            max_merchant_avg_amount: 10_000.0,
            scale: DEFAULT_SCALE,
            mcc_risk: default_mcc_risk(),
        }
    }
}

pub fn normalized_vector(tx: &TxInput, norm: &Normalization) -> [f32; DIM] {
    let amount_vs_avg = if tx.customer_avg_amount > 0.0 {
        tx.amount / tx.customer_avg_amount
    } else {
        tx.amount
    };

    [
        clamp01(tx.amount / norm.max_amount) as f32,
        clamp01(tx.installments / norm.max_installments) as f32,
        clamp01(amount_vs_avg / norm.amount_vs_avg_ratio) as f32,
        clamp01(f64::from(tx.hour_of_day) / 23.0) as f32,
        clamp01(f64::from(tx.day_of_week) / 6.0) as f32,
        if tx.has_last_tx {
            clamp01(tx.minutes_since_last_tx / norm.max_minutes) as f32
        } else {
            -1.0
        },
        if tx.has_last_tx {
            clamp01(tx.km_from_last_tx / norm.max_km) as f32
        } else {
            -1.0
        },
        clamp01(tx.km_from_home / norm.max_km) as f32,
        clamp01(tx.tx_count_24h / norm.max_tx_count_24h) as f32,
        if tx.is_online { 1.0 } else { 0.0 },
        if tx.card_present { 1.0 } else { 0.0 },
        if tx.unknown_merchant { 1.0 } else { 0.0 },
        norm.mcc_risk(tx) as f32,
        clamp01(tx.merchant_avg_amount / norm.max_merchant_avg_amount) as f32,
    ]
}

fn quantize_normalized(value: f64, scale: i64) -> i64 {
    if !value.is_finite() {
        return 0;
    }
    if value < 0.0 {
        return -scale;
    }
    (clamp01(value) * scale as f64).round() as i64
}

fn clamp01(value: f64) -> f64 {
    if !value.is_finite() {
        0.0
    } else {
        value.clamp(0.0, 1.0)
    }
}

fn positive_or(value: f64, fallback: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value
    } else {
        fallback
    }
}

fn default_mcc_risk() -> HashMap<String, f64> {
    [
        ("5411", 0.15),
        ("5812", 0.30),
        ("5912", 0.20),
        ("5944", 0.45),
        ("7801", 0.80),
        ("7802", 0.75),
        ("7995", 0.85),
        ("4511", 0.35),
        ("5311", 0.25),
        ("5999", 0.50),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}
