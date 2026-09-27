#[cfg(test)]
mod exact_json_tests {
    use super::types;
    use serde_json::{json, Value};

    fn item() -> Value {
        json!({"name": "ab", "count": 4, "at": "2024-01-01T00:00:00Z"})
    }

    fn decode(value: Value) -> Result<types::Item, String> {
        serde_json::from_value(value).map_err(|error| error.to_string())
    }

    fn accepts(field: &str, value: Value) {
        let mut input = item();
        input[field] = value;
        let decoded = decode(input.clone()).unwrap_or_else(|error| panic!("{field}: {error}"));
        let output = serde_json::to_value(&decoded).unwrap();
        assert_eq!(output[field], input[field], "{field} round-trips");
    }

    #[test]
    fn integers_accept_integral_numbers() {
        let decoded = decode(json!({"name": "ab", "count": 4.0, "at": "2024-01-01T00:00:00Z"}));
        assert_eq!(decoded.unwrap().count, 4);
        let mut input = item();
        input["count"] = json!(4.5);
        assert!(decode(input).is_err());
        accepts("status", json!(404));
        // Validation-only keywords are the server's to enforce.
        accepts("count", json!(-3));
        accepts("name", json!("a b c d"));
    }

    #[test]
    fn open_prefix_items_and_typed_maps() {
        accepts("prefix", json!([]));
        accepts("prefix", json!(["*", 5, {}]));
        let mut input = item();
        input["limits"] = json!({"cap": 20.0, "x": 2.0});
        let decoded = decode(input).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap()["limits"], json!({"cap": 20, "x": 2}));
    }

    #[test]
    fn presence_null_and_open_objects() {
        accepts("nickname", Value::Null);
        let mut missing = item();
        missing.as_object_mut().unwrap().remove("name");
        assert!(decode(missing).is_err());
        // Members the schema does not declare are kept.
        let mut open = item();
        open["extra"] = json!({"nested": [1, 2]});
        let decoded = decode(open.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), open);
    }

    #[test]
    fn response_models_keep_null_and_tolerate_it() {
        let summary: types::Summary =
            serde_json::from_value(json!({"id": "a", "flag": null, "note": null, "count": 2.0}))
                .unwrap();
        assert_eq!(summary.flag, None);
        assert_eq!(summary.note, None);
        assert_eq!(summary.count, Some(2));
        // A decoded value serializes back unchanged.
        let value = json!({"id": "a", "flag": "x", "count": 1});
        let summary: types::Summary = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(summary).unwrap(), value);
        let missing_flag: types::Summary = serde_json::from_value(json!({"id": "a"})).unwrap();
        assert_eq!(missing_flag.flag, None);
    }

    #[test]
    fn date_times_accept_every_rfc_3339_spelling() {
        for text in [
            "2024-01-01T00:00:00.5+05:30",
            "2024-01-01t00:00:00z",
            "2024-01-01T00:00Z",
            "0000-01-01T00:00:00Z",
        ] {
            let mut input = item();
            input["at"] = json!(text);
            assert!(decode(input).is_ok(), "{text}");
        }
        // A pattern requiring milliseconds keeps three digits.
        accepts("stamp", json!("2024-01-01T00:00:00.910Z"));
        let mut input = item();
        input["stamp"] = json!("2024-01-01T00:00Z");
        assert!(decode(input).is_ok());
    }
}
