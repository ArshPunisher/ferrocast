//! Exact money and platform currencies (R-32).
//!
//! Real money is kept in minor units per currency and never converted silently. Bits and
//! KICKs are platform value, not money: mixing them would make goals and payouts lie.

use std::fmt;

use serde::{Deserialize, Serialize};

/// An amount of real money, e.g. ₹500.00 = `Money { minor: 50000, currency: "INR" }`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Money {
    pub minor: i64,
    /// ISO 4217 code, upper case.
    pub currency: String,
}

impl Money {
    pub fn new(minor: i64, currency: &str) -> Self {
        Money {
            minor,
            currency: currency.to_ascii_uppercase(),
        }
    }

    /// Builds money from a decimal string such as `"12.50"`, as payment providers send it.
    /// Rejects anything that isn't a plain non-negative decimal with at most the currency's
    /// number of fraction digits, so "1e3", "-5" or "12.345 USD" never become an amount.
    pub fn parse_decimal(amount: &str, currency: &str) -> Result<Self, MoneyError> {
        let currency = currency.trim().to_ascii_uppercase();
        if currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()) {
            return Err(MoneyError::BadCurrency(currency));
        }
        let digits = exponent(&currency);
        let s = amount.trim();
        let (whole, frac) = match s.split_once('.') {
            Some((w, f)) => (w, f),
            None => (s, ""),
        };
        let valid = !whole.is_empty()
            && whole.bytes().all(|b| b.is_ascii_digit())
            && frac.bytes().all(|b| b.is_ascii_digit())
            && frac.len() <= digits as usize
            && !(s.contains('.') && frac.is_empty());
        if !valid {
            return Err(MoneyError::BadAmount(amount.to_string()));
        }
        let scale = 10i64.pow(digits);
        let whole: i64 = whole
            .parse()
            .map_err(|_| MoneyError::BadAmount(amount.to_string()))?;
        let mut frac_val: i64 = if frac.is_empty() {
            0
        } else {
            frac.parse()
                .map_err(|_| MoneyError::BadAmount(amount.to_string()))?
        };
        for _ in frac.len()..digits as usize {
            frac_val *= 10;
        }
        let minor = whole
            .checked_mul(scale)
            .and_then(|w| w.checked_add(frac_val))
            .ok_or_else(|| MoneyError::BadAmount(amount.to_string()))?;
        Ok(Money { minor, currency })
    }

    /// The amount in major units as a float, for display and conditions only (never sums).
    pub fn major(&self) -> f64 {
        self.minor as f64 / 10f64.powi(exponent(&self.currency) as i32)
    }

    /// `₹500`, `$4.99`, `€10.50`: the symbol when we know it, otherwise the code.
    pub fn display(&self) -> String {
        let digits = exponent(&self.currency);
        let sign = if self.minor < 0 { "-" } else { "" };
        let abs = self.minor.unsigned_abs();
        let scale = 10u64.pow(digits);
        let whole = abs / scale;
        let frac = abs % scale;
        let number = if digits == 0 || frac == 0 {
            group(whole)
        } else {
            format!("{}.{:0width$}", group(whole), frac, width = digits as usize)
        };
        match symbol(&self.currency) {
            Some(sym) => format!("{sign}{sym}{number}"),
            None => format!("{sign}{number} {}", self.currency),
        }
    }
}

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    #[error("'{0}' is not a currency code")]
    BadCurrency(String),
    #[error("'{0}' is not an amount")]
    BadAmount(String),
}

/// Number of minor-unit digits (ISO 4217). Most currencies use 2.
pub fn exponent(currency: &str) -> u32 {
    match currency {
        "JPY" | "KRW" | "VND" | "CLP" | "ISK" | "UGX" | "XAF" | "XOF" | "PYG" | "RWF" | "KMF"
        | "GNF" | "VUV" | "XPF" | "DJF" | "BIF" => 0,
        "BHD" | "KWD" | "OMR" | "JOD" | "TND" | "IQD" | "LYD" => 3,
        _ => 2,
    }
}

fn symbol(currency: &str) -> Option<&'static str> {
    Some(match currency {
        "INR" => "₹",
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        "BRL" => "R$",
        "AUD" => "A$",
        "CAD" => "C$",
        "KRW" => "₩",
        "TRY" => "₺",
        "PHP" => "₱",
        _ => return None,
    })
}

/// 1234567 → "1,234,567". (Indian lakh grouping is a UI locale concern, not core's.)
fn group(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A platform's own currency.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueUnit {
    /// Twitch Bits.
    Bits,
    /// Kick KICKs.
    Kicks,
    /// YouTube Jewels.
    Jewels,
    Other(String),
}

impl ValueUnit {
    pub fn label(&self, amount: u64) -> String {
        let one = amount == 1;
        match self {
            ValueUnit::Bits => {
                if one {
                    "bit".into()
                } else {
                    "bits".into()
                }
            }
            ValueUnit::Kicks => {
                if one {
                    "KICK".into()
                } else {
                    "KICKs".into()
                }
            }
            ValueUnit::Jewels => {
                if one {
                    "jewel".into()
                } else {
                    "jewels".into()
                }
            }
            ValueUnit::Other(s) => s.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformValue {
    pub unit: ValueUnit,
    pub amount: u64,
}

impl PlatformValue {
    pub fn display(&self) -> String {
        format!("{} {}", group(self.amount), self.unit.label(self.amount))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_amounts_exactly() {
        assert_eq!(
            Money::parse_decimal("12.50", "usd").unwrap(),
            Money::new(1250, "USD")
        );
        assert_eq!(
            Money::parse_decimal("500", "INR").unwrap(),
            Money::new(50_000, "INR")
        );
        assert_eq!(
            Money::parse_decimal("0.1", "EUR").unwrap(),
            Money::new(10, "EUR")
        );
        assert_eq!(
            Money::parse_decimal("1000", "JPY").unwrap(),
            Money::new(1000, "JPY")
        );
        assert_eq!(
            Money::parse_decimal("1.234", "KWD").unwrap(),
            Money::new(1234, "KWD")
        );
    }

    #[test]
    fn rejects_hostile_amounts() {
        for bad in [
            "",
            "-5",
            "1e3",
            "12.345",
            "1,000",
            "12.",
            ".5",
            "NaN",
            "inf",
            "  ",
            "１２",
            "99999999999999999999",
        ] {
            assert!(
                Money::parse_decimal(bad, "USD").is_err(),
                "{bad:?} should fail"
            );
        }
        assert!(
            Money::parse_decimal("1.5", "JPY").is_err(),
            "JPY has no minor units"
        );
        for cur in ["", "US", "USDT", "us1", "₹"] {
            assert!(
                Money::parse_decimal("1", cur).is_err(),
                "{cur:?} should fail"
            );
        }
    }

    #[test]
    fn displays_with_symbols_and_grouping() {
        assert_eq!(Money::new(50_000, "INR").display(), "₹500");
        assert_eq!(Money::new(499, "USD").display(), "$4.99");
        assert_eq!(Money::new(123_456_789, "USD").display(), "$1,234,567.89");
        assert_eq!(Money::new(1000, "JPY").display(), "¥1,000");
        assert_eq!(Money::new(1050, "SEK").display(), "10.50 SEK");
        assert_eq!(Money::new(-250, "EUR").display(), "-€2.50");
    }

    #[test]
    fn platform_value_labels() {
        assert_eq!(
            PlatformValue {
                unit: ValueUnit::Bits,
                amount: 1
            }
            .display(),
            "1 bit"
        );
        assert_eq!(
            PlatformValue {
                unit: ValueUnit::Bits,
                amount: 1500
            }
            .display(),
            "1,500 bits"
        );
        assert_eq!(
            PlatformValue {
                unit: ValueUnit::Kicks,
                amount: 100
            }
            .display(),
            "100 KICKs"
        );
    }
}
