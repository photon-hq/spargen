#[cfg(test)]
mod request_presence_tests {
    use super::{types, Client, ExecuteFuture, HttpBackend};
    use serde_json::{json, Value};
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Waker};

    fn required() -> Value {
        json!({"requiredValue": "keep", "requiredNullable": null, "defaultValue": 7})
    }

    #[test]
    fn optional_nullable_preserves_omission_null_and_value() {
        for value in [None, Some(Value::Null), Some(json!("replacement"))] {
            let mut input = required();
            if let Some(value) = value {
                input["optionalNullable"] = value;
            }
            let request: types::Change = serde_json::from_value(input.clone()).unwrap();
            assert_eq!(serde_json::to_value(request).unwrap(), input);
        }
    }

    #[test]
    fn requiredness_and_nullability_are_independent() {
        let mut missing = required();
        missing.as_object_mut().unwrap().remove("requiredNullable");
        assert!(serde_json::from_value::<types::Change>(missing).is_err());
        for field in ["requiredValue", "optionalValue"] {
            let mut invalid = required();
            invalid[field] = Value::Null;
            assert!(
                serde_json::from_value::<types::Change>(invalid).is_err(),
                "{field}"
            );
        }
        let mut missing = required();
        missing.as_object_mut().unwrap().remove("requiredValue");
        assert!(serde_json::from_value::<types::Change>(missing).is_err());
        for field in [
            "requiredValue",
            "requiredNullable",
            "optionalValue",
            "optionalNullable",
        ] {
            let mut invalid = required();
            invalid[field] = json!(42);
            assert!(
                serde_json::from_value::<types::Change>(invalid).is_err(),
                "{field}"
            );
            let mut valid = required();
            valid[field] = json!("value");
            let decoded: types::Change = serde_json::from_value(valid.clone()).unwrap();
            assert_eq!(serde_json::to_value(decoded).unwrap(), valid);
        }
    }

    #[test]
    fn nested_references_arrays_maps_unions_and_cycles_preserve_null() {
        let mut input = required();
        input["child"] = json!({"label": null});
        input["children"] = json!([{}, {"label": null}, {"label": "value"}]);
        input["byName"] = json!({"empty": {}, "clear": {"label": null}});
        input["choice"] = json!({"label": null});
        input["recursive"] = json!({"label": null, "next": {"label": null}});
        input["tuple"] = json!([{"label": null}, "value"]);
        input["status"] = Value::Null;
        let request: types::Change = serde_json::from_value(input.clone()).unwrap();
        assert_eq!(serde_json::to_value(request).unwrap(), input);
    }

    #[derive(Debug, Default)]
    struct CaptureRequest(Mutex<Option<Value>>);

    impl HttpBackend for CaptureRequest {
        fn execute(&self, request: reqwest::Request) -> ExecuteFuture<'_> {
            assert_eq!(request.method(), reqwest::Method::PATCH);
            assert_eq!(request.url().path(), "/change");
            assert_eq!(request.headers()["content-type"], "application/json");
            *self.0.lock().unwrap() =
                Some(serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap());
            // Stop at the transport boundary: no socket, service or asynchronous runtime needed.
            Box::pin(std::future::pending())
        }
    }

    #[test]
    fn generated_client_sends_the_three_distinct_payloads() {
        for state in [None, Some(None), Some(Some("replacement".to_owned()))] {
            let mut body: types::Change = serde_json::from_value(required()).unwrap();
            let mut expected = required();
            if let Some(value) = &state {
                expected["optionalNullable"] = json!(value);
            }
            body.optional_nullable = state;
            let backend = Arc::new(CaptureRequest::default());
            let client = Client::with_backend(backend.clone(), "https://example.test").unwrap();
            let mut future = std::pin::pin!(client.change(&body));
            assert!(future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending());
            assert_eq!(*backend.0.lock().unwrap(), Some(expected));
        }
    }

    #[test]
    fn existing_nonnullable_defaults_and_response_only_interfaces_stay_valid() {
        let mut input = required();
        input.as_object_mut().unwrap().remove("defaultValue");
        let request: types::Change = serde_json::from_value(input).unwrap();
        assert_eq!(request.default_value, Some(7));
        let response = types::ResponseOnly {
            label: Some("value".to_owned()),
        };
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            json!({"label": "value"})
        );
    }
}
