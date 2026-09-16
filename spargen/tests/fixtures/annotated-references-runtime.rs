#[cfg(test)]
mod annotated_reference_tests {
    use super::{types, Client, Error, GetTimestampError, ResponseValue};
    use serde_json::json;
    use std::future::Future;

    fn timestamp_response(
        _: impl Future<Output = Result<ResponseValue<types::Timestamp>, Error<GetTimestampError>>>,
    ) {
    }

    #[test]
    fn endpoint_keeps_the_timestamp_response_type() {
        let client = Client::new("https://example.test").unwrap();
        timestamp_response(client.get_timestamp());
    }

    #[test]
    fn timestamp_aliases_and_nullability_round_trip() {
        for nullable in [json!(null), json!("2026-09-15T23:00:00Z")] {
            let value = json!({
                "createdAt": "2026-09-15T22:00:00Z",
                "updatedAt": "2026-09-15T23:00:00Z",
                "nullableUpdatedAt": nullable
            });
            let decoded: types::Envelope = serde_json::from_value(value.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        }
        assert!(serde_json::from_value::<types::UpdatedAt>(json!({})).is_err());
        assert!(serde_json::from_value::<types::CreatedAt>(json!(false)).is_err());
    }

    #[test]
    fn recursive_alias_keeps_its_object_fields() {
        let value = json!({"value": "head", "next": {"value": "tail"}});
        let decoded: types::Alias = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(decoded.value, "head");
        assert_eq!(decoded.next.as_ref().unwrap().value, "tail");
        assert_eq!(serde_json::to_value(decoded).unwrap(), value);
        let nullable = json!({"next": null});
        let decoded: types::NullableAlias = serde_json::from_value(nullable.clone()).unwrap();
        assert!(decoded.next.is_none());
        assert_eq!(serde_json::to_value(decoded).unwrap(), nullable);
    }
}
