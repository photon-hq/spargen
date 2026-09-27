//! RFC 3339 date-time and full-date newtypes for `format: date-time` and `format: date`.
//!
//! JSON Schema 2020-12 — the dialect OpenAPI 3.1 and 3.2 use — defines `date-time` as an RFC 3339
//! `date-time` and `date` as an RFC 3339 `full-date`. Neither of those is what `time`'s own `serde`
//! implementations produce:
//!
//! - Without `time`'s `serde-human-readable` feature, `OffsetDateTime` serializes as a **nine-element
//!   integer sequence** and `Date` as a two-element one — not a string at all.
//! - *With* that feature, the human-readable form is `[year]-[month]-[day] [hour]:[minute]:[second]
//!   .[subsecond] [offset]`, which uses a space separator and a seconds-bearing offset. RFC 3339
//!   requires `T` (or a lowercase `t`) and `±HH:MM`.
//!
//! Either way the bytes on the wire would not be what the specification describes, so these types
//! carry their own hand-written `Serialize`/`Deserialize` that are always RFC 3339. They do not
//! consult `is_human_readable`: a generated client speaks JSON, and the specification fixes the
//! representation regardless of the format's self-description.
//!
//! Both types are transparent wrappers — they `Deref` to the `time` type and convert with `From` in
//! both directions — so anything `time` can do with the inner value is one `.0` or one deref away.

use std::fmt;
use std::ops::{Deref, DerefMut};
use std::str::FromStr;

use serde::de::{Deserialize, Deserializer, Error as _};
use serde::ser::{Serialize, Serializer};
use time::format_description::well_known::Rfc3339;

/// An RFC 3339 `date-time`, the wire form of OpenAPI's `format: date-time`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DateTime(pub time::OffsetDateTime);

/// An RFC 3339 `full-date` (`YYYY-MM-DD`), the wire form of OpenAPI's `format: date`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Date(pub time::Date);

/// A value could not be read as its RFC 3339 form.
#[derive(Debug)]
pub struct ParseError {
    /// What was expected, for the message.
    expected: &'static str,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "expected an RFC 3339 {}", self.expected)
    }
}

impl std::error::Error for ParseError {}

// --- DateTime ----------------------------------------------------------------------------------

impl DateTime {
    /// The wrapped `time` value.
    pub fn into_inner(self) -> time::OffsetDateTime {
        self.0
    }
}

impl Deref for DateTime {
    type Target = time::OffsetDateTime;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for DateTime {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<time::OffsetDateTime> for DateTime {
    fn from(value: time::OffsetDateTime) -> Self {
        Self(value)
    }
}

impl From<DateTime> for time::OffsetDateTime {
    fn from(value: DateTime) -> Self {
        value.0
    }
}

impl fmt::Display for DateTime {
    /// The RFC 3339 rendering — the same bytes `Serialize` writes.
    ///
    /// This matters beyond convenience: a `multipart/form-data` text part and a `text/plain` body
    /// are rendered through `Display`, so a `Display` that disagreed with `Serialize` would put two
    /// different encodings of the same value on the wire depending on the body's media type.
    ///
    /// RFC 3339 fixes the year at four digits, so a year outside `0..=9999` — which `time` can hold
    /// but RFC 3339 cannot express — has no rendering here. It falls back to `time`'s own form
    /// rather than failing, because `Display` failure would panic through `to_string()` at exactly
    /// those two call sites. `Serialize` still refuses such a value, so it can never reach a JSON
    /// body claiming to be RFC 3339.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0.format(&Rfc3339) {
            Ok(text) => formatter.write_str(&text),
            Err(_) => write!(formatter, "{}", self.0),
        }
    }
}

impl DateTime {
    /// Parse an RFC 3339 `date-time` exactly: `YYYY-MM-DDTHH:MM:SS[.fraction](Z|±HH:MM)`, with an
    /// upper-case `T` and `Z`, a year from 0001, a real calendar day and no leap second.
    pub fn parse_strict(text: &str) -> Result<Self, ParseError> {
        parse_date_time(text, true)
    }

    /// As [`parse_strict`](Self::parse_strict), but seconds may be omitted (`HH:MM`), `T` and `Z`
    /// may be lower case (RFC 3339 §5.6) and the year may be 0000: every value a contract with a
    /// `date-time` format or its own `pattern` may carry.
    pub fn parse_lenient(text: &str) -> Result<Self, ParseError> {
        parse_date_time(text, false)
    }
}

impl DateTime {
    /// RFC 3339 with exactly three fractional digits (`2024-01-01T00:00:00.000Z`): the form a
    /// `date-time` takes where the contract's `pattern` requires milliseconds.
    pub fn to_millis_string(&self) -> String {
        let value = self.0;
        let offset = value.offset();
        let zone = if offset.is_utc() {
            "Z".to_owned()
        } else {
            let (hours, minutes, _) = offset.as_hms();
            let sign = if hours < 0 || minutes < 0 { '-' } else { '+' };
            format!(
                "{sign}{:02}:{:02}",
                hours.unsigned_abs(),
                minutes.unsigned_abs()
            )
        };
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}{zone}",
            value.year(),
            u8::from(value.month()),
            value.day(),
            value.hour(),
            value.minute(),
            value.second(),
            value.millisecond(),
        )
    }
}

/// Serialize a value whose JSON strings are all `date-time`s with three fractional digits (see
/// [`DateTime::to_millis_string`]).
pub fn serialize_millis<S, T>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
    T: Serialize + ?Sized,
{
    fn rewrite(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => {
                if let Ok(parsed) = DateTime::parse_lenient(text) {
                    *text = parsed.to_millis_string();
                }
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(rewrite),
            _ => {}
        }
    }
    let mut json = serde_json::to_value(value).map_err(serde::ser::Error::custom)?;
    rewrite(&mut json);
    json.serialize(serializer)
}

fn parse_date_time(text: &str, strict: bool) -> Result<DateTime, ParseError> {
    let invalid = || ParseError {
        expected: "date-time",
    };
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Result<u32, ParseError> {
        let part = bytes.get(range).ok_or_else(invalid)?;
        if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
            return Err(invalid());
        }
        std::str::from_utf8(part)
            .ok()
            .and_then(|part| part.parse().ok())
            .ok_or_else(invalid)
    };
    if bytes.len() < 17
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !(bytes[10] == b'T' || (!strict && bytes[10] == b't'))
        || bytes[13] != b':'
    {
        return Err(invalid());
    }
    let year = digits(0..4)?;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    let hour = digits(11..13)?;
    let minute = digits(14..16)?;
    let mut index = 16;
    let mut second = 0;
    let mut nanosecond = 0u32;
    if bytes.get(index) == Some(&b':') {
        second = digits(index + 1..index + 3)?;
        index += 3;
        if bytes.get(index) == Some(&b'.') {
            let start = index + 1;
            let mut end = start;
            while bytes.get(end).is_some_and(u8::is_ascii_digit) {
                end += 1;
            }
            if end == start {
                return Err(invalid());
            }
            // Nanosecond precision; further digits are below what `time` can represent.
            let mut scaled = 0u32;
            for position in 0..9 {
                let digit = bytes
                    .get(start + position)
                    .filter(|_| start + position < end);
                scaled = scaled * 10 + digit.map_or(0, |digit| u32::from(digit - b'0'));
            }
            nanosecond = scaled;
            index = end;
        }
    } else if strict {
        return Err(invalid());
    }
    let offset = match bytes.get(index) {
        Some(b'Z') if index + 1 == bytes.len() => 0i32,
        Some(b'z') if !strict && index + 1 == bytes.len() => 0i32,
        Some(sign @ (b'+' | b'-')) if index + 6 == bytes.len() && bytes[index + 3] == b':' => {
            let hours = digits(index + 1..index + 3)?;
            let minutes = digits(index + 4..index + 6)?;
            if hours > 23 || minutes > 59 {
                return Err(invalid());
            }
            let total = (hours * 3600 + minutes * 60) as i32;
            if *sign == b'-' {
                -total
            } else {
                total
            }
        }
        _ => return Err(invalid()),
    };
    if (strict && year == 0) || hour > 23 || minute > 59 || second > 59 {
        return Err(invalid());
    }
    let month = u8::try_from(month)
        .ok()
        .and_then(|month| time::Month::try_from(month).ok())
        .ok_or_else(invalid)?;
    let date =
        time::Date::from_calendar_date(year as i32, month, day as u8).map_err(|_| invalid())?;
    let clock = time::Time::from_hms_nano(hour as u8, minute as u8, second as u8, nanosecond)
        .map_err(|_| invalid())?;
    let offset = time::UtcOffset::from_whole_seconds(offset).map_err(|_| invalid())?;
    Ok(DateTime(date.with_time(clock).assume_offset(offset)))
}

impl FromStr for DateTime {
    type Err = ParseError;

    /// Strict RFC 3339 (see [`DateTime::parse_strict`]).
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse_strict(text)
    }
}

impl Serialize for DateTime {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let text = self.0.format(&Rfc3339).map_err(|error| {
            serde::ser::Error::custom(format!("date-time is not RFC 3339: {error}"))
        })?;
        serializer.serialize_str(&text)
    }
}

impl<'de> Deserialize<'de> for DateTime {
    /// Lenient (see [`DateTime::parse_lenient`]), so no value the contract allows is refused.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Self::parse_lenient(&text).map_err(D::Error::custom)
    }
}

// --- Date --------------------------------------------------------------------------------------

impl Date {
    /// The wrapped `time` value.
    pub fn into_inner(self) -> time::Date {
        self.0
    }
}

impl Deref for Date {
    type Target = time::Date;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Date {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<time::Date> for Date {
    fn from(value: time::Date) -> Self {
        Self(value)
    }
}

impl From<Date> for time::Date {
    fn from(value: Date) -> Self {
        value.0
    }
}

impl fmt::Display for Date {
    /// `YYYY-MM-DD`, matching `Serialize` for the reason given on [`DateTime`]'s `Display`, with the
    /// same fallback for a year RFC 3339's four-digit `full-date` cannot express.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let year = self.0.year();
        if !(0..=9999).contains(&year) {
            return write!(formatter, "{}", self.0);
        }
        write!(
            formatter,
            "{year:04}-{:02}-{:02}",
            u8::from(self.0.month()),
            self.0.day()
        )
    }
}

impl FromStr for Date {
    type Err = ParseError;

    /// Parses exactly `YYYY-MM-DD`. The length and separator positions are checked first so that a
    /// longer string (an accidental full `date-time`, say) is rejected rather than silently
    /// truncated to its date part.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let invalid = || ParseError {
            expected: "full-date (YYYY-MM-DD)",
        };
        let bytes = text.as_bytes();
        if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return Err(invalid());
        }
        let year: i32 = text[0..4].parse().map_err(|_| invalid())?;
        let month: u8 = text[5..7].parse().map_err(|_| invalid())?;
        let day: u8 = text[8..10].parse().map_err(|_| invalid())?;
        let month = time::Month::try_from(month).map_err(|_| invalid())?;
        time::Date::from_calendar_date(year, month, day)
            .map(Self)
            .map_err(|_| invalid())
    }
}

impl Serialize for Date {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let year = self.0.year();
        if !(0..=9999).contains(&year) {
            return Err(serde::ser::Error::custom(format!(
                "date year {year} has no RFC 3339 full-date representation"
            )));
        }
        serializer.serialize_str(&format!(
            "{year:04}-{:02}-{:02}",
            u8::from(self.0.month()),
            self.0.day()
        ))
    }
}

impl<'de> Deserialize<'de> for Date {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        text.parse().map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date_time() -> DateTime {
        DateTime(
            time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
                .expect("representable timestamp"),
        )
    }

    /// The regression this module exists for: a JSON string, not `time`'s nine-integer sequence.
    #[test]
    fn date_time_serializes_as_an_rfc3339_string() {
        let json = serde_json::to_string(&date_time()).unwrap();
        assert_eq!(json, "\"2023-11-14T22:13:20Z\"");
        assert!(!json.starts_with('['), "must not be a sequence: {json}");
    }

    #[test]
    fn date_time_round_trips_through_json() {
        let value = date_time();
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<DateTime>(&json).unwrap(), value);
    }

    /// An offset is preserved as `±HH:MM`, never the space-separated form `time`'s own
    /// human-readable serializer would emit.
    #[test]
    fn date_time_keeps_a_non_utc_offset_in_rfc3339_form() {
        let offset = time::UtcOffset::from_hms(-5, 30, 0).unwrap();
        let value = DateTime(date_time().0.to_offset(offset));
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(json, "\"2023-11-14T16:43:20-05:30\"");
        assert_eq!(serde_json::from_str::<DateTime>(&json).unwrap(), value);
    }

    #[test]
    fn date_time_display_matches_serialization() {
        let value = date_time();
        let json: String = serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap();
        assert_eq!(value.to_string(), json);
    }

    #[test]
    fn date_time_rejects_a_non_rfc3339_string() {
        // Exactly `time`'s own human-readable rendering: a space where RFC 3339 requires `T`.
        let error = serde_json::from_str::<DateTime>("\"2023-11-14 22:13:20.0 +00:00:00\"");
        assert!(error.is_err(), "{error:?}");
        assert!(serde_json::from_str::<DateTime>("[2023,318,22,13,20,0,0,0,0]").is_err());
    }

    #[test]
    fn date_serializes_as_a_full_date_string() {
        let value = Date(time::Date::from_calendar_date(2023, time::Month::November, 14).unwrap());
        assert_eq!(serde_json::to_string(&value).unwrap(), "\"2023-11-14\"");
        assert_eq!(value.to_string(), "2023-11-14");
        assert_eq!(
            serde_json::from_str::<Date>("\"2023-11-14\"").unwrap(),
            value
        );
    }

    #[test]
    fn date_pads_single_digit_months_and_days() {
        let value = Date(time::Date::from_calendar_date(7, time::Month::January, 2).unwrap());
        assert_eq!(serde_json::to_string(&value).unwrap(), "\"0007-01-02\"");
    }

    #[test]
    fn date_rejects_malformed_and_over_long_input() {
        // A full `date-time` must not be silently truncated to its date part.
        assert!(serde_json::from_str::<Date>("\"2023-11-14T22:13:20Z\"").is_err());
        assert!(serde_json::from_str::<Date>("\"2023-11-4\"").is_err());
        assert!(serde_json::from_str::<Date>("\"2023/11/14\"").is_err());
        assert!(serde_json::from_str::<Date>("\"2023-13-01\"").is_err());
        assert!(serde_json::from_str::<Date>("\"2023-02-30\"").is_err());
    }

    /// `Display` must never fail: generated code renders a `multipart/form-data` text part and a
    /// `text/plain` body through `to_string()`, which panics on a `Display` error. A year outside
    /// RFC 3339's four-digit range is constructible (`time` holds -9999..=9999), so this is
    /// reachable from ordinary user input rather than only from a malformed response.
    #[test]
    fn display_never_panics_for_a_year_rfc3339_cannot_express() {
        let date = Date(time::Date::from_calendar_date(-1, time::Month::January, 2).unwrap());
        assert!(!date.to_string().is_empty());
        let stamp = DateTime(time::OffsetDateTime::new_utc(date.0, time::Time::MIDNIGHT));
        assert!(!stamp.to_string().is_empty());
        // Serialization still refuses: such a value must never reach a body claiming RFC 3339.
        assert!(serde_json::to_string(&date).is_err());
        assert!(serde_json::to_string(&stamp).is_err());
    }

    /// The newtypes are transparent: `time`'s API is one deref away.
    #[test]
    fn newtypes_deref_to_the_time_types() {
        assert_eq!(date_time().year(), 2023);
        let date = Date(time::Date::from_calendar_date(2023, time::Month::November, 14).unwrap());
        assert_eq!(date.month(), time::Month::November);
        assert_eq!(time::Date::from(date), date.0);
    }
}
