//! JSON a serde-derived type would refuse although the contract allows it: JSON Schema's
//! `integer` is any number with a zero fractional part, so `1.0` is an integer.

use serde::de::{self, Deserializer};
use serde_json::{Number, Value};

// ---- Integers ------------------------------------------------------------------------------

/// JSON Schema's `integer` is any number with a zero fractional part, so `1.0` is an integer.
/// Rewrite such a number to its integer spelling so serde's integer types accept it.
pub fn integral(value: &mut Value) {
    let Value::Number(number) = value else {
        return;
    };
    if number.is_i64() || number.is_u64() {
        return;
    }
    let Some(float) = number.as_f64() else {
        return;
    };
    if float.fract() != 0.0 || !float.is_finite() {
        return;
    }
    if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&float) {
        *value = Value::Number(Number::from(float as i64));
    } else if (0.0..18_446_744_073_709_551_616.0).contains(&float) {
        *value = Value::Number(Number::from(float as u64));
    }
}

/// Deserialize a JSON value, first applying `normalize` (which may rewrite integral numbers or
/// refuse a value the Rust type cannot tell apart from a valid one).
pub fn deserialize_normalized<'de, D, T>(
    deserializer: D,
    normalize: fn(&mut Value) -> Result<(), String>,
) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let mut value = <Value as de::Deserialize>::deserialize(deserializer)?;
    normalize(&mut value).map_err(de::Error::custom)?;
    serde_json::from_value(value).map_err(de::Error::custom)
}
