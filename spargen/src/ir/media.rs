use super::{ParamStyle, Ty};

/// The wire codec selected for a supported request/response media type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    /// `application/json` (canonical).
    Json,
    /// `application/x-www-form-urlencoded`.
    FormUrlEncoded,
    /// `application/xml` / `text/xml`: a body serialized/deserialized as XML via the runtime's
    /// feature-gated `quick-xml` codec. Lowers to the same struct type `T` as JSON; JSON still wins
    /// when both are offered. Supported for request bodies, a single success body, and error
    /// bodies (see [`Responses::xml_in_multi_status`]).
    Xml,
    /// `application/octet-stream` (raw bytes).
    OctetStream,
    /// A raw UTF-8 textual representation (`text/*`, plus explicitly supported textual vendor
    /// media such as GitHub's `application/octocat-stream`).
    Text,
    /// `multipart/form-data` (request bodies): an object schema whose properties are the form
    /// parts — binary/bytes properties become file parts, scalars/composites become text parts.
    Multipart,
    /// `multipart/related`: a complete caller-preencoded multipart message carried as raw bytes.
    /// The operation's required `Content-Type` header parameter supplies the boundary that frames
    /// those bytes; spargen deliberately does not rebuild the message as multipart/form-data.
    MultipartRelated,
    /// `text/event-stream` (Server-Sent Events, response bodies): a stream of items decoded from
    /// the event `data:` fields. Lowered to a streaming operation returning `EventStream<T>`.
    EventStream,
    /// `application/x-ndjson` (newline-delimited JSON, response bodies): a stream of items, one per
    /// line. Lowered to a streaming operation returning `EventStream<T>`.
    Ndjson,
    /// RFC 7464 JSON Text Sequences and `+json-seq` media.
    JsonSequence,
}

impl MediaType {
    /// The stream framing for a streaming response media type, or `None` for a non-streaming media.
    pub fn stream_framing(self) -> Option<Framing> {
        match self {
            MediaType::EventStream => Some(Framing::Sse),
            MediaType::Ndjson => Some(Framing::Ndjson),
            MediaType::JsonSequence => Some(Framing::JsonSequence),
            _ => None,
        }
    }
}

/// How a streaming response body is framed into typed items. Mirrors the runtime `Framing` enum;
/// codegen maps each variant to its `support::Framing` counterpart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Server-Sent Events (`text/event-stream`).
    Sse,
    /// Standards-compliant SSE events converted to JSON objects before schema deserialization.
    SseEvent,
    /// OpenAPI 3.2 SSE whose envelope `data` string contains JSON described by `contentSchema`.
    /// The runtime parses the envelope metadata but yields the decoded JSON payload directly.
    SseJsonData,
    /// Newline-delimited JSON (`application/x-ndjson`).
    Ndjson,
    /// RFC 7464 records separated by ASCII RS (`0x1E`).
    JsonSequence,
}

/// A request body (matrix: Bodies).
#[derive(Debug, Clone)]
pub struct RequestBody {
    /// The body media type.
    pub media: MediaType,
    /// The selected content type essence, preserved for the emitted `Content-Type` header.
    pub content_type: String,
    /// The body's type, or `None` for an untyped/byte body.
    pub ty: Option<Ty>,
    /// Whether the body is `required`. A required body is a plain argument; an optional one is
    /// passed as `Option<&T>` and omitted from the request when absent.
    pub required: bool,
    /// Per-property wire encoding for a form or multipart body. Always fully resolved — one entry
    /// per body property, in field order — so the runtime never has to infer a default.
    pub encoding: BodyEncoding,
}

/// Per-property wire encoding for an `application/x-www-form-urlencoded` or
/// `multipart/form-data` request body (matrix: Media → Encoding Object).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BodyEncoding {
    /// One entry per body-schema property, in the body struct's field order.
    pub properties: Vec<PropertyEncoding>,
}

/// How one property of a form or multipart body is rendered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyEncoding {
    /// The wire property name — the form field name, or the multipart part name.
    pub name: String,
    pub mode: EncodingMode,
    /// Literal extra part headers (multipart only), with `Content-Type` already removed because
    /// the specification describes it separately.
    pub headers: Vec<(String, String)>,
}

/// The Encoding Object's mode switch.
///
/// The specification keys this on *presence*: any explicit `style`/`explode`/`allowReserved`
/// selects RFC 6570 query-style serialization and makes `contentType` inert, while all three
/// absent selects media-type serialization under `contentType` (explicit or defaulted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodingMode {
    /// Media-type mode: the value is rendered in `content_type` by `codec`.
    Media {
        /// The single concrete media type this property is sent as.
        content_type: String,
        /// The codec that renders it.
        codec: MediaType,
    },
    /// RFC 6570 query-style mode.
    Style {
        /// Restricted by the document schema to form/spaceDelimited/pipeDelimited/deepObject.
        style: ParamStyle,
        explode: bool,
        allow_reserved: bool,
    },
}

/// A response status selector (matrix: Responses).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusSpec {
    /// An exact status code, e.g. `200`.
    Exact(u16),
    /// A status range by leading digit, e.g. `Range(2)` for `2XX`.
    Range(u8),
}

impl StatusSpec {
    /// Whether the selector covers only success (2xx) statuses.
    pub fn is_success(self) -> bool {
        match self {
            StatusSpec::Exact(code) => (200..300).contains(&code),
            StatusSpec::Range(prefix) => prefix == 2,
        }
    }
}

/// A typed response for one status selector. Documented headers get typed accessors; every header
/// stays reachable raw through `ResponseValue::headers`.
#[derive(Debug, Clone)]
pub struct Response {
    /// The response body type, if any.
    pub body: Option<Ty>,
    /// The chosen body media type, or `None` for a bodyless response. Codegen routes the decode by
    /// this (e.g. XML/text/binary bodies use their own codecs rather than serde_json).
    pub media: Option<MediaType>,
    /// For a streaming response (chosen media `text/event-stream` or `application/x-ndjson`), the
    /// framing of the streamed items; `None` for a whole-body response. The `body` is the item
    /// type `T` when this is `Some`.
    pub stream: Option<Framing>,
    /// Documented response headers, in source order. A `Content-Type` entry is dropped during
    /// lowering, because the specification says it is ignored.
    pub headers: Vec<ResponseHeader>,
}

/// One documented response header.
#[derive(Debug, Clone)]
pub struct ResponseHeader {
    /// The header name, as declared.
    pub name: String,
    /// The value type.
    pub ty: Ty,
    /// Whether the header is documented as always present.
    pub required: bool,
    /// `explode` for the `simple` style; the default is `false`.
    pub explode: bool,
    /// The wire shape, derived from `ty` — `simple` is lossy, so the shape cannot be recovered
    /// from the text and must travel with it.
    pub shape: HeaderShape,
    /// `deprecated` → `#[deprecated]` on the accessor field.
    pub deprecated: bool,
    /// Documentation carried onto the generated field.
    pub docs: super::Docs,
}

/// The wire shape of a documented header value. Mirrors the runtime enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeaderShape {
    /// A single scalar.
    Scalar,
    /// A comma-separated list.
    Array,
    /// Alternating `key,value` pairs, or `key=value` when exploded.
    Object,
    /// A `content`-typed header carrying JSON.
    Json,
    /// `Set-Cookie`: one value per occurrence, never comma-joined (RFC 9110 §5.3 exempts it from
    /// the field-list rule), decoded into a list of the declared per-line type.
    SetCookie,
}

/// The full set of responses for an operation: per-status entries plus an optional `default`.
#[derive(Debug, Clone)]
pub struct Responses {
    /// Per-status responses, most-specific first (exact before range).
    pub by_status: Vec<(StatusSpec, Response)>,
    /// The `default` response, if declared.
    pub default: Option<Response>,
}

impl Responses {
    /// The success shape of the operation. One documented success status yields plain `T` (or
    /// `()` when bodyless). Two or more documented success statuses yield a per-operation success
    /// enum whose entries are sorted into decode precedence (exact code ascending, then range
    /// ascending, then `default` last): a bodied status becomes a payload-carrying variant and a
    /// bodyless one (e.g. `202` beside `204`, or `204` beside a bodied `200`) a unit variant, so
    /// the caller can tell apart every status the contract distinguishes and a bodyless status is
    /// never decoded as a body.
    ///
    /// The exception is a lone streaming or XML success body: those decode paths are scoped to the
    /// single-body case (see [`Self::stream_success`] and [`Self::xml_in_multi_status`]), so that
    /// body stays `Plain` and its bodyless siblings remain reachable through the response status.
    pub fn success(&self) -> SuccessShape {
        // A default with no explicit status entries is the operation's single success body.
        if self.by_status.is_empty() {
            return match self.default.as_ref().and_then(|default| default.body) {
                Some(body) => SuccessShape::Plain(body),
                None => SuccessShape::Unit,
            };
        }

        let success: Vec<&(StatusSpec, Response)> = self
            .by_status
            .iter()
            .filter(|(status, _)| self.is_success_side(*status))
            .collect();
        let mut bodied = success
            .iter()
            .filter(|(_, response)| response.body.is_some());
        let lone_body = match (bodied.next(), bodied.next()) {
            (Some((_, response)), None) => Some(response),
            _ => None,
        };
        match (success.as_slice(), lone_body) {
            ([], _) => SuccessShape::Unit,
            ([(_, response)], _) => match response.body {
                Some(body) => SuccessShape::Plain(body),
                None => SuccessShape::Unit,
            },
            (_, Some(response))
                if response.stream.is_some() || response.media == Some(MediaType::Xml) =>
            {
                SuccessShape::Plain(response.body.expect("lone bodied response has a body"))
            }
            _ => {
                let mut entries: Vec<(StatusSpec, Option<Ty>)> = success
                    .iter()
                    .map(|(status, response)| (*status, response.body))
                    .collect();
                entries.sort_by_key(|(status, _)| precedence_key(*status));
                SuccessShape::Enum(entries)
            }
        }
    }

    /// Whether the operation's success response is a typed *stream* (`text/event-stream` or
    /// `application/x-ndjson`), and if so its framing plus the streamed item type `T`. Streaming is
    /// scoped to the single-success-body case: it fires only when exactly one success response
    /// carries a body and that body was lowered from a streaming media. The generated method then
    /// returns `EventStream<T>` in place of `ResponseValue<T>`. A JSON alternative on the same
    /// response wins during media selection (see `choose_media`), so it never reaches here as a
    /// stream. Multiple bodied success statuses fall back to the normal (non-streaming) shape.
    pub fn stream_success(&self) -> Option<(Framing, Ty)> {
        let responses = self.success_responses();
        let mut bodied = responses
            .into_iter()
            .filter(|response| response.body.is_some());
        match (bodied.next(), bodied.next()) {
            (Some(response), None) => {
                let framing = response.stream?;
                let body = response.body?;
                Some((framing, body))
            }
            _ => None,
        }
    }

    /// The media type of the operation's single bodied success response, when exactly one success
    /// response carries a body (i.e. [`Self::success`] is [`SuccessShape::Plain`]). Codegen uses this
    /// to route the success decode. `None` when there is no single bodied success.
    pub fn single_success_media(&self) -> Option<MediaType> {
        let mut bodied = self
            .success_responses()
            .into_iter()
            .filter(|response| response.body.is_some());
        match (bodied.next(), bodied.next()) {
            (Some(response), None) => response.media,
            _ => None,
        }
    }

    /// The media type of the operation's single bodied error response, when exactly one error
    /// response carries a body (as it does when [`Self::error`] is [`ErrorShape::Single`]). Codegen uses this to
    /// route the error-body classification. `None` when there is no single bodied error.
    pub fn single_error_media(&self) -> Option<MediaType> {
        let mut bodied = self
            .error_responses()
            .into_iter()
            .filter(|response| response.body.is_some());
        match (bodied.next(), bodied.next()) {
            (Some(response), None) => response.media,
            _ => None,
        }
    }

    /// Whether an XML body appears in a *multi-status success* enum (two or more bodied success
    /// statuses). XML success decode is scoped to the single-body path, so this exotic combination
    /// is rejected cleanly during lowering (narrowed `E009`) rather than silently mis-decoding an
    /// XML body as JSON. Error enums decode each variant through its own codec, XML included.
    pub fn xml_in_multi_status(&self) -> bool {
        matches!(self.success(), SuccessShape::Enum(_))
            && self
                .success_responses()
                .iter()
                .any(|response| response.media == Some(MediaType::Xml))
    }

    /// Whether a documented `304 Not Modified` is decoded as a success-side outcome rather than an
    /// error: it answers a conditional request, so it joins the 2xx statuses (as a unit variant of
    /// the success enum). The exception is a lone streaming or XML success body, whose decode path
    /// is single-body only, and a `304` documented with a body, which HTTP never sends; there `304`
    /// stays a documented error status.
    pub fn not_modified_is_outcome(&self) -> bool {
        if !self
            .by_status
            .iter()
            .any(|(status, response)| *status == StatusSpec::Exact(304) && response.body.is_none())
        {
            return false;
        }
        let mut bodied = self
            .by_status
            .iter()
            .filter(|(status, response)| status.is_success() && response.body.is_some());
        match (bodied.next(), bodied.next()) {
            (Some((_, response)), None) => {
                response.stream.is_none() && response.media != Some(MediaType::Xml)
            }
            _ => true,
        }
    }

    /// Whether a documented status is decoded on the success side: every 2xx status, plus `304`
    /// when [`Self::not_modified_is_outcome`]. Every other status is an error.
    pub fn is_success_side(&self, status: StatusSpec) -> bool {
        status.is_success() || (status == StatusSpec::Exact(304) && self.not_modified_is_outcome())
    }

    /// The operation's error responses: every non-success explicit status plus the `default`
    /// response (which matches any status). Mirrors the entry set built by [`Self::error`].
    fn error_responses(&self) -> Vec<&Response> {
        let mut responses: Vec<&Response> = self
            .by_status
            .iter()
            .filter(|(status, _)| !self.is_success_side(*status))
            .map(|(_, response)| response)
            .collect();
        if let Some(default) = &self.default {
            responses.push(default);
        }
        responses
    }

    /// The operation's success responses in document order: the `default` response alone when no
    /// explicit statuses are declared (it is then the sole success), otherwise the 2xx entries.
    /// Mirrors the success/error split used by [`Self::success`].
    fn success_responses(&self) -> Vec<&Response> {
        if self.by_status.is_empty() {
            return self.default.iter().collect();
        }
        self.by_status
            .iter()
            .filter(|(status, _)| self.is_success_side(*status))
            .map(|(_, response)| response)
            .collect()
    }

    /// The error shape of the operation. No documented error status yields `None`; a single
    /// documented error status with a body yields that typed `E` body; any other set of documented
    /// error statuses yields a per-operation error enum, sorted into classification precedence
    /// (exact code ascending, then range ascending, then `default` — the `Range(0)` sentinel —
    /// last), with a payload-carrying variant per bodied status and a unit variant per bodyless
    /// one. Every documented error status therefore decodes to `Error::Api`, bodyless or not; only
    /// an undocumented status is `Error::UnexpectedStatus`. `default` contributes here (as
    /// `Range(0)`) whenever it is not the sole success source.
    pub fn error(&self) -> ErrorShape {
        let mut entries: Vec<(StatusSpec, Option<Ty>)> = Vec::new();
        for (status, response) in &self.by_status {
            if !self.is_success_side(*status) {
                entries.push((*status, response.body));
            }
        }
        if let Some(default) = &self.default {
            entries.push((StatusSpec::Range(0), default.body));
        }
        match entries.as_slice() {
            [] => ErrorShape::None,
            [(_, Some(body))] => ErrorShape::Single(*body),
            _ => {
                entries.sort_by_key(|(status, _)| precedence_key(*status));
                ErrorShape::Enum(entries)
            }
        }
    }
}

/// The deterministic decode-precedence sort key for a documented status selector: exact codes
/// first (ascending), then ranges (ascending by leading digit), then the `default` response (the
/// `Range(0)` sentinel) last. Keys are unique within one operation's entries, so the sort is total.
fn precedence_key(status: StatusSpec) -> (u8, u16) {
    match status {
        StatusSpec::Exact(code) => (0, code),
        StatusSpec::Range(0) => (2, 0),
        StatusSpec::Range(prefix) => (1, u16::from(prefix)),
    }
}

/// The success return type of an operation (before wrapping in `ResponseValue<T>`).
#[derive(Debug, Clone)]
pub enum SuccessShape {
    /// No success body.
    Unit,
    /// A single success body type.
    Plain(Ty),
    /// Two or more documented success statuses. Generated as a per-operation response enum, one
    /// variant per status — a payload-carrying variant for a bodied status, a unit variant for a
    /// documented bodyless status (e.g. `204`). Entries are pre-sorted into decode precedence
    /// (exact before range; `default` last); decode dispatches by HTTP status in that order.
    Enum(Vec<(StatusSpec, Option<Ty>)>),
}

/// The typed error body `E` of an operation (matrix: Responses).
#[derive(Debug, Clone)]
pub enum ErrorShape {
    /// No documented error status.
    None,
    /// A single documented error status with a body, typed as that body.
    Single(Ty),
    /// Any other set of documented error statuses: two or more, or one without a body. Generated
    /// as a per-operation error enum, one variant per status — a payload-carrying variant for a
    /// bodied status, a unit variant for a documented bodyless status. Entries are pre-sorted into classification precedence (exact before range;
    /// `default` — carried as the `Range(0)` sentinel — last); classification dispatches by HTTP
    /// status in that order.
    Enum(Vec<(StatusSpec, Option<Ty>)>),
}

#[cfg(test)]
mod tests {
    use super::{ErrorShape, Response, Responses, StatusSpec, SuccessShape, Ty};
    use crate::ir::TypeId;

    fn ty(id: u32) -> Ty {
        Ty {
            id: TypeId(id),
            nullable: false,
            boxed: false,
        }
    }

    fn resp(body: Option<u32>) -> Response {
        Response {
            media: body.map(|_| super::MediaType::Json),
            body: body.map(ty),
            stream: None,
            headers: Vec::new(),
        }
    }

    #[test]
    fn success_enum_sorts_exact_before_range_and_keeps_bodyless_unit() {
        // Document order lists the 2XX range BEFORE the exact 200 (and mixes in a bodyless 204).
        // Precedence must reorder to exact-before-range so a real HTTP 200 decodes into its exact
        // variant, not the overlapping range one; the bodyless 204 survives as a payload-free entry.
        let responses = Responses {
            by_status: vec![
                (StatusSpec::Range(2), resp(Some(1))),
                (StatusSpec::Exact(200), resp(Some(2))),
                (StatusSpec::Exact(204), resp(None)),
            ],
            default: None,
        };
        match responses.success() {
            SuccessShape::Enum(entries) => {
                let shape: Vec<_> = entries.iter().map(|(s, b)| (*s, b.is_some())).collect();
                assert_eq!(
                    shape,
                    vec![
                        (StatusSpec::Exact(200), true),
                        (StatusSpec::Exact(204), false),
                        (StatusSpec::Range(2), true),
                    ]
                );
                // The exact 200 body (id 2) precedes the range 2XX body (id 1).
                assert_eq!(entries[0].1.unwrap().id, TypeId(2));
            }
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn error_enum_sorts_exact_before_range_before_default() {
        // Document order: range 4XX, then exact 409, then a default — all must reorder to
        // exact < range < default (the `Range(0)` sentinel is last).
        let responses = Responses {
            by_status: vec![
                (StatusSpec::Range(4), resp(Some(1))),
                (StatusSpec::Exact(409), resp(Some(2))),
            ],
            default: Some(resp(Some(3))),
        };
        match responses.error() {
            ErrorShape::Enum(entries) => {
                let specs: Vec<_> = entries.iter().map(|(s, _)| *s).collect();
                assert_eq!(
                    specs,
                    vec![
                        StatusSpec::Exact(409),
                        StatusSpec::Range(4),
                        StatusSpec::Range(0),
                    ]
                );
            }
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    fn error_statuses(responses: &Responses) -> Vec<(StatusSpec, bool)> {
        match responses.error() {
            ErrorShape::Enum(entries) => entries.iter().map(|(s, b)| (*s, b.is_some())).collect(),
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn a_bodyless_error_status_is_a_documented_variant() {
        // A lone bodyless `400` is still a documented outcome, so it is an `Error::Api` unit variant
        // rather than an uninhabited error type that leaves it to `UnexpectedStatus`.
        let lone = Responses {
            by_status: vec![
                (StatusSpec::Exact(200), resp(Some(1))),
                (StatusSpec::Exact(400), resp(None)),
            ],
            default: None,
        };
        assert_eq!(error_statuses(&lone), vec![(StatusSpec::Exact(400), false)]);

        // A bodyless `400` beside one bodied `503` keeps both, instead of collapsing to the `503`
        // body alone.
        let beside_body = Responses {
            by_status: vec![
                (StatusSpec::Exact(200), resp(Some(1))),
                (StatusSpec::Exact(503), resp(Some(2))),
                (StatusSpec::Exact(400), resp(None)),
            ],
            default: None,
        };
        assert_eq!(
            error_statuses(&beside_body),
            vec![
                (StatusSpec::Exact(400), false),
                (StatusSpec::Exact(503), true)
            ]
        );
    }

    #[test]
    fn a_single_bodied_error_status_is_its_body_and_none_is_none() {
        let single = Responses {
            by_status: vec![
                (StatusSpec::Exact(200), resp(Some(1))),
                (StatusSpec::Exact(404), resp(Some(2))),
            ],
            default: None,
        };
        assert!(matches!(single.error(), ErrorShape::Single(ty) if ty.id == TypeId(2)));

        let none = Responses {
            by_status: vec![(StatusSpec::Exact(200), resp(Some(1)))],
            default: None,
        };
        assert!(matches!(none.error(), ErrorShape::None));
    }

    #[test]
    fn a_lone_xml_error_body_beside_a_bodyless_status_is_an_enum() {
        // The bodyless sibling is still a documented status, so it gets its own unit variant (and
        // `Error::Api`) instead of collapsing to the XML body; error enums decode XML per variant,
        // so the shape is not rejected.
        let mut xml = resp(Some(2));
        xml.media = Some(super::MediaType::Xml);
        let responses = Responses {
            by_status: vec![
                (StatusSpec::Exact(200), resp(Some(1))),
                (StatusSpec::Exact(400), xml),
                (StatusSpec::Exact(404), resp(None)),
            ],
            default: None,
        };
        assert_eq!(
            error_statuses(&responses),
            vec![
                (StatusSpec::Exact(400), true),
                (StatusSpec::Exact(404), false)
            ]
        );
        assert!(!responses.xml_in_multi_status());
    }

    fn success_statuses(responses: &Responses) -> Vec<(StatusSpec, bool)> {
        match responses.success() {
            SuccessShape::Enum(entries) => entries.iter().map(|(s, b)| (*s, b.is_some())).collect(),
            other => panic!("expected Enum, got {other:?}"),
        }
    }

    #[test]
    fn single_bodied_success_with_bodyless_sibling_is_an_enum() {
        // A `204` beside a bodied `200` is a distinct documented outcome with no body to decode, so
        // both statuses get their own variant instead of the `204` being parsed as the `200` body.
        let responses = Responses {
            by_status: vec![
                (StatusSpec::Exact(204), resp(None)),
                (StatusSpec::Exact(200), resp(Some(1))),
            ],
            default: None,
        };
        assert_eq!(
            success_statuses(&responses),
            vec![
                (StatusSpec::Exact(200), true),
                (StatusSpec::Exact(204), false)
            ]
        );
    }

    #[test]
    fn all_bodyless_success_statuses_are_an_enum_of_unit_variants() {
        // `202` (scheduled) and `204` (done) carry no body but mean different things.
        let responses = Responses {
            by_status: vec![
                (StatusSpec::Exact(204), resp(None)),
                (StatusSpec::Exact(202), resp(None)),
            ],
            default: None,
        };
        assert_eq!(
            success_statuses(&responses),
            vec![
                (StatusSpec::Exact(202), false),
                (StatusSpec::Exact(204), false)
            ]
        );
    }

    #[test]
    fn one_success_status_stays_plain_or_unit() {
        let bodied = Responses {
            by_status: vec![(StatusSpec::Exact(200), resp(Some(1)))],
            default: None,
        };
        assert!(matches!(bodied.success(), SuccessShape::Plain(_)));
        let bodyless = Responses {
            by_status: vec![(StatusSpec::Exact(204), resp(None))],
            default: None,
        };
        assert!(matches!(bodyless.success(), SuccessShape::Unit));
    }

    #[test]
    fn lone_streaming_or_xml_success_body_stays_plain() {
        // Streaming and XML decode are scoped to a single success body.
        for response in [
            Response {
                stream: Some(super::Framing::Ndjson),
                media: Some(super::MediaType::Ndjson),
                ..resp(Some(1))
            },
            Response {
                media: Some(super::MediaType::Xml),
                ..resp(Some(1))
            },
        ] {
            let responses = Responses {
                by_status: vec![
                    (StatusSpec::Exact(200), response),
                    (StatusSpec::Exact(204), resp(None)),
                ],
                default: None,
            };
            assert!(matches!(responses.success(), SuccessShape::Plain(_)));
        }
    }
}
