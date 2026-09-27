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

// ---- Exact matching ----------------------------------------------------------------------------

thread_local! {
    static STRICT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Run `read` in exact mode: members a closed object does not declare, and enum values a type does
/// not know, fail the read instead of being ignored or kept. Trial unions read each variant this
/// way first, so the variant the contract means is chosen.
pub fn strictly<T>(read: impl FnOnce() -> T) -> T {
    struct Leave;
    impl Drop for Leave {
        fn drop(&mut self) {
            STRICT.with(|strict| strict.set(strict.get() - 1));
        }
    }
    STRICT.with(|strict| strict.set(strict.get() + 1));
    let _leave = Leave;
    read()
}

/// Whether a read is in exact mode (see [`strictly`]).
pub fn is_strict() -> bool {
    STRICT.with(|strict| strict.get() > 0)
}

/// Refuse an object member not in `known` (exact mode only; see [`strictly`]).
pub fn check_known_members<E: de::Error>(value: &Value, known: &[&str]) -> Result<(), E> {
    if let Value::Object(members) = value {
        if let Some(name) = members.keys().find(|name| !known.contains(&name.as_str())) {
            return Err(E::custom(format!("unknown member {name:?}")));
        }
    }
    Ok(())
}

/// How many of `input`'s object members survive in `decoded`'s JSON: a trial union without an exact
/// match prefers the variant that keeps the most of the value.
pub fn retained_members<T: serde::Serialize + ?Sized>(input: &Value, decoded: &T) -> usize {
    let (Value::Object(input), Ok(Value::Object(output))) = (input, serde_json::to_value(decoded))
    else {
        return 0;
    };
    input
        .keys()
        .filter(|name| output.contains_key(name.as_str()))
        .count()
}
