#[cfg(test)]
mod intersection_tests {
    use super::types;
    use serde_json::json;

    #[test]
    fn base64_content_encoding_preserves_the_json_string() {
        let value = json!({"rawRequestBase64": "aGVsbG8="});
        let decoded: types::EncodedPayload = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        assert!(serde_json::from_value::<types::EncodedPayload>(json!({
            "rawRequestBase64": [104, 101, 108, 108, 111]
        })).is_err());
    }

    #[test]
    fn intersections_preserve_fields_and_enforce_narrowed_variants() {
        let sms = json!({"platform": "sms", "text": "hello", "status": "received"});
        let email = json!({"platform": "email", "subject": "hello", "status": "received"});
        for value in [sms.clone(), email.clone()] {
            let decoded: types::Received = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), value);
            let inline: types::Inline = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(inline).unwrap(), value);
        }
        let narrowed: types::SmsReceived = serde_json::from_value(sms.clone()).unwrap();
        assert_eq!(narrowed.text, "hello");
        assert_eq!(serde_json::to_value(narrowed).unwrap(), sms);
        assert!(serde_json::from_value::<types::SmsReceived>(email).is_err());
        let one: types::OverlappingReceived = serde_json::from_value(sms.clone()).unwrap();
        assert_eq!(serde_json::to_value(one).unwrap(), sms);
        // Both narrowed branches accept this payload: oneOf must still reject ambiguity.
        assert!(serde_json::from_value::<types::OverlappingReceived>(json!({
            "platform": "sms", "text": "special", "status": "received"
        })).is_err());
        for invalid in [
            json!({"platform": "sms", "text": "hello"}),
            json!({"platform": "sms", "status": "received"}),
            json!({"platform": "sms", "text": "hello", "status": "sent"}),
            json!({"platform": "other", "text": "hello", "status": "received"}),
            json!("not an object"),
            json!(null),
        ] {
            assert!(serde_json::from_value::<types::Received>(invalid.clone()).is_err());
            assert!(serde_json::from_value::<types::Inline>(invalid).is_err());
        }
    }
}
