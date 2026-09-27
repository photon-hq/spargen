#[cfg(test)]
mod constant_tests {
    use super::types;
    use serde_json::{json, Value};

    fn event() -> Value {
        json!({"type": "event.created", "maybe": "here"})
    }

    #[test]
    fn constant_fields_are_plain_strings_that_round_trip() {
        let mut input = event();
        input["optional"] = json!("sometimes");
        input["optionalNullable"] = json!("perhaps");
        input["defaulted"] = json!("fixed");
        input["kinds"] = json!(["only"]);
        input["labels"] = json!({"a": "1", "b": "2"});
        let parsed: types::Event = serde_json::from_value(input.clone()).unwrap();
        let kind: &String = &parsed.r#type;
        assert_eq!(kind, "event.created");
        assert_eq!(parsed.labels.as_ref().unwrap()["b"], "2");
        assert_eq!(serde_json::to_value(parsed).unwrap(), input);
    }

    #[test]
    fn only_the_constant_is_accepted() {
        for (field, wrong) in [
            ("type", json!("event.deleted")),
            ("maybe", json!("elsewhere")),
            ("optional", json!("never")),
            ("optionalNullable", json!("no")),
            ("defaulted", json!("moving")),
        ] {
            let mut input = event();
            input[field] = wrong;
            assert!(serde_json::from_value::<types::Event>(input).is_err(), "{field}");
        }
        let mut input = event();
        input["kinds"] = json!(["other"]);
        assert!(serde_json::from_value::<types::Event>(input).is_err());
        let mut input = event();
        input["labels"] = json!({"a": 1});
        assert!(serde_json::from_value::<types::Event>(input).is_err());
    }

    #[test]
    fn requiredness_nullability_and_defaults_are_unchanged() {
        // A required constant must be present; a required nullable one may be null.
        let mut missing = event();
        missing.as_object_mut().unwrap().remove("type");
        assert!(serde_json::from_value::<types::Event>(missing).is_err());
        let mut missing = event();
        missing.as_object_mut().unwrap().remove("maybe");
        assert!(serde_json::from_value::<types::Event>(missing).is_err());
        let mut null = event();
        null["maybe"] = Value::Null;
        null["optionalNullable"] = Value::Null;
        let parsed: types::Event = serde_json::from_value(null.clone()).unwrap();
        assert_eq!(parsed.maybe, None);
        assert_eq!(parsed.optional_nullable, Some(None));
        // A schema `default` is documented only: the absent member stays absent.
        assert_eq!(parsed.defaulted, None);
        assert_eq!(serde_json::to_value(parsed).unwrap(), null);
        // A non-nullable constant rejects null.
        let mut null = event();
        null["type"] = Value::Null;
        assert!(serde_json::from_value::<types::Event>(null).is_err());
    }

    #[test]
    fn response_only_constants_are_checked_too() {
        let problem: types::NotFoundProblem =
            serde_json::from_value(json!({"code": "NOT_FOUND", "status": 404})).unwrap();
        assert_eq!(problem.code, "NOT_FOUND");
        assert_eq!(problem.hint, None);
        assert!(serde_json::from_value::<types::NotFoundProblem>(
            json!({"code": "GONE", "status": 404})
        )
        .is_err());
        assert!(serde_json::from_value::<types::NotFoundProblem>(
            json!({"code": "NOT_FOUND", "status": 404, "hint": "later"})
        )
        .is_err());
    }

    #[test]
    fn a_value_other_than_the_constant_is_never_sent() {
        let mut event: types::Event = serde_json::from_value(event()).unwrap();
        event.r#type = "event.deleted".to_owned();
        assert!(serde_json::to_value(&event).is_err());
        event.r#type = "event.created".to_owned();
        event.optional = Some("never".to_owned());
        assert!(serde_json::to_value(&event).is_err());
        event.optional = None;
        assert!(serde_json::to_value(&event).is_ok());
    }

    #[test]
    fn constrained_unions_accept_and_produce_only_the_intersection() {
        for (input, ok) in [
            (json!({"kind": "cat", "status": "active"}), true),
            (json!({"kind": "dog", "status": "active"}), true),
            (json!({"kind": "cat", "status": "retired"}), false),
            (json!({"kind": "bird", "status": "active"}), false),
        ] {
            let parsed = serde_json::from_value::<types::ActivePet>(input.clone());
            assert_eq!(parsed.is_ok(), ok, "{input}");
            if let Ok(parsed) = parsed {
                assert_eq!(serde_json::to_value(parsed).unwrap(), input);
            }
        }
        let retired: types::Cat =
            serde_json::from_value(json!({"kind": "cat", "status": "retired"})).unwrap();
        assert!(serde_json::to_value(types::ActivePet::Cat(Box::new(retired))).is_err());
    }

    #[test]
    fn constant_parameters_refuse_other_values_before_sending() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let client = super::Client::new("http://127.0.0.1:9").unwrap();
        let params = super::ListActivePetsParams::default().interval("minute".to_owned());
        for (group_by, params) in [("week", None), ("day", Some(params))] {
            let mut call = Box::pin(client.list_active_pets(group_by.to_owned(), params));
            let mut context = Context::from_waker(Waker::noop());
            match call.as_mut().poll(&mut context) {
                Poll::Ready(Err(error)) => {
                    let source = std::error::Error::source(&error)
                        .map(ToString::to_string)
                        .unwrap_or_default();
                    assert!(source.contains("must be the constant"), "{error}: {source}")
                }
                other => panic!("expected an immediate refusal, got {:?}", other.is_ready()),
            }
        }
    }
}
