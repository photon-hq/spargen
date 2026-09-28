# Support Matrix

This is the operational support matrix for OpenAPI 3.1.x and 3.2.x. Both versions use JSON Schema
2020-12 and share one frontend; their complete document structures are validated against vendored,
date-pinned official OpenAPI schemas before lowering. OpenAPI 3.2 is a focused extension of 3.1;
the exact delta and spargen disposition are summarized in [OpenAPI 3.2 scope](openapi-3.2.md).
Unsupported constructs fail loudly — a diagnostic and no output — rather than degrading silently
to `serde_json::Value`. Where a construct maps to an untyped value, the input schema itself was
untyped; degradation is never silent.

Named schema references with annotation siblings (for example `$ref` plus `description`) retain
both the shared target and the annotated component's own type and documentation. References to
nullable targets remain nullable, including recursive edges. Alias-only cycles with no concrete
schema are rejected as `E004`; they do not produce untyped placeholder models.

| Area | Supported | Warned | Rejected |
| --- | --- | --- | --- |
| Version | OpenAPI `3.1.x` and `3.2.x`; version-specific required fields are enforced by the corresponding official schema | - | OpenAPI `3.0.x`/`<3.1`/`3.3+`/other or malformed versions (`E001`) |
| Dialect | The shared OAS base dialect (`https://spec.openapis.org/oas/3.1/dialect/base`) or omitted; OpenAPI 3.2 intentionally retains this URI | - | Other `jsonSchemaDialect` values, including the nonexistent `…/oas/3.2/dialect/base` (`E002`) |
| References | Local/internal component refs and relative-file refs used by the frontend; remote (`http`/`https`) refs resolved **hermetically** from a locally vendored, hash-pinned copy — `generate`/`check` never touch the network. The opt-in `spargen lock <spec>` step is the only place that fetches: it walks the bundle (recursing through relative sub-files and fetched remote docs, whose relative refs resolve against their own URL), writes copies under `.spargen/vendor/`, and pins each `url`/`sha256`/`path` in a deterministic `spargen.lock` (sorted by URL, no timestamps). A remote ref's fragment (`…#/components/schemas/Foo`) is resolved as a JSON Pointer within the vendored doc | - | A remote ref not pinned in `spargen.lock`, or an unfetchable absolute-URI scheme such as `urn:` (`E003` — run `spargen lock` to vendor and pin it); a vendored copy that is missing or whose bytes no longer match the pinned `sha256` (`E021` — the lock is the source of truth); unresolved refs (`E004`) |
| Schema shape | boolean schemas (`true`/`false`), objects, arrays, tuples, maps, scalar primitives (`contentEncoding: base64` preserves the encoded JSON string), multi-type arrays, exact JSON `null` (Rust `()`), homogeneous scalar enums (a mixed `null` member makes the enum nullable — `Option<Enum>`), untyped schemas, recursive `$ref` cycles (self- and mutually-recursive; cycle-closing references are boxed), and Schema Object `$ref` siblings as intersections; `allOf` composition merged into a single struct by flattening member properties and recursively intersecting repeated property/additional-value types (including integer within number, enum within scalar, arrays, objects, unions, and nullability), or intersected with typed union/scalar members (object constraints narrow every union branch); `oneOf`/`anyOf` unions lowered to typed Rust enums with uniformly boxed payloads and custom `Deserialize`/`Serialize` (never `serde(untagged)`, never an untyped fallback), where a plain `number` member with no plain `integer` sibling holds a `serde_json::Number` so an integer is not re-serialized as a float: discriminator/category and statically-disjoint fast paths inspect one buffered value, while overlapping variants use typed trial matching (`oneOf` requires exactly one match; `anyOf` deterministically chooses enum over broad scalar, integer over number, more-required object over broader object, recursively narrower array, then source order); serialization revalidates the same semantics, sibling shape constraints intersect every branch, mixed discriminator unions dispatch non-objects by unique JSON category and objects by tag, and a `null` member makes the union nullable (a lone remaining member collapses to `Option<T>`); `patternProperties` composed with `properties`/`additionalProperties` into a typed overflow map when every pattern/additional value schema lowers to the same type; `readOnly`/`writeOnly`/`deprecated`/`title`/`description` annotations surface as rustdoc / `#[deprecated]`, read from the property subschema rather than its enclosing object; 3.2 `discriminator.defaultMapping` generates a fallback variant; a property, parameter or component `default` is documented in rustdoc and never applied: an absent optional field decodes as `None` and `None` is not serialized, so the service applies its own default | validation-only keywords and their nested schemas, including conditional/applicator validation and a `patternProperties` key regex (`W001`); a `default` with no documentable home — array `items`, `additionalProperties` value, tuple `prefixItems`, or a request/response body root — is reported and not applied (`W005`) | `patternProperties` only when it cannot be a typed map — heterogeneous value types or combined with `additionalProperties: false` (`E005`), dynamic refs (`E006`), union applicator combinations not representable as one generated enum (`E007`), enums/consts that mix distinct scalar kinds or contain object/array members (`E008`; a `null` member alone is allowed), `allOf` compositions whose typed intersection is empty/unrepresentable — incompatible property/additional-value constraints, disjoint object/scalar types, or a direct recursive `$ref` member whose fields are not yet known (`E013`); `items` beside `prefixItems`, which describes a tuple with a typed variable-length rest that no Rust type expresses — `items: false` is exactly a tuple and is supported (`E015`); static `$id`/`$anchor` scopes are rejected until scope-aware lowering is available (`E004`) |
| Media | JSON (`application/json` and structured `application/*+json`); `application/xml` / `text/xml` request and single-body responses through the `quick-xml` codec, embedded and required only for an API that has an XML body (`xml.name`, `xml.attribute`, and 3.2 `xml.nodeType: attribute` are honored); raw UTF-8 `text/*` and GitHub's textual `application/octocat-stream` with a string-like/binary schema; `application/octet-stream` with a binary schema as `bytes::Bytes`; response-only `*/*` with an absent, unconstrained, or binary schema also returns raw bytes without interpreting the actual Content-Type; response selection skips request-only form codecs before choosing a supported concrete representation or the wildcard fallback; typed wildcard schemas are rejected; textual/binary codecs apply independently to single- and multi-status successes/errors; `application/x-www-form-urlencoded` requests; `multipart/form-data` object requests; raw `multipart/related` bodies with a binary schema, with requests additionally requiring a `Content-Type` header parameter carrying the matching boundary; **Encoding Objects** on form-urlencoded and multipart/form-data, with the specification's mode switch — any explicit `style`/`explode`/`allowReserved` selects RFC 6570 query-style serialization (and `contentType` becomes inert), otherwise the property is rendered by its `contentType`, explicit or defaulted per the version's table — so every multipart part is sent with a resolved `Content-Type` and a form body is built to the byte; RFC 6570 multipart parts carry **literal** delimiters and unencoded data, since the specification forbids percent-encoding there, and an exploded array becomes one part per item under the same name; reusable 3.2 `components.mediaTypes`; `text/event-stream`, JSON Lines/NDJSON, and RFC 7464 JSON Text Sequences with `itemSchema` as typed `EventStream<T>`; a 3.2 SSE envelope whose string `data` has `contentMediaType: application/json` plus `contentSchema` yields the declared JSON payload type directly | `contentMediaType`/`contentSchema` outside the recognized SSE `data` position are validation-only (`W001`); `itemSchema` on non-sequential media is ignored (`W010`); a body or response offering several media types generates exactly one and reports the alternatives it did not (`W014`); `encoding`/`prefixEncoding`/`itemEncoding` on a media type that is neither multipart nor form-urlencoded, an entry naming a property the schema does not declare, and `encoding.headers` that pins no `const`/`default` value are acknowledged as having no effect (`W011`); unsupported XML hints on a type never serialized as XML are ignored (`W006`) | Other media; a `multipart/related` request without a binary schema and required boundary-bearing `Content-Type` header parameter; raw text/binary with an incompatible schema; form-urlencoded or multipart/form-data responses; non-object multipart requests; streaming requests; XML in a multi-status response enum; 3.2 complete-sequence `schema` without `itemSchema`; a wildcard `encoding.contentType`, a binary property in a form-urlencoded body, or an `encoding.style` that is not a form style; an `encoding.style` of `spaceDelimited`/`pipeDelimited` with `explode: true`, which the specification's own table leaves undefined; on `multipart/form-data`, an `encoding.style: deepObject` (a query-only style, with no part representation) or RFC 6570 serialization selected for an *object* property, for which the specification defines no part representation; `xml.namespace`/`xml.prefix`/`xml.wrapped` (and node types other than `element`/`attribute`) on a type that **is** serialized as XML, where ignoring them would put structurally different XML on the wire (`E009`) |
| Parameters | **Every RFC 6570 style the specification defines**: `simple`, `matrix`, and `label` in path; `form`, `spaceDelimited`, `pipeDelimited`, and `deepObject` in query; `form` and 3.2 `cookie` in cookie; `simple` in header — each with scalar/array/object serialization and `explode` defaults/overrides. `allowReserved: true` sends reserved characters unencoded; everywhere else delimiters are emitted literally and every data byte is percent-encoded, so a joining `,` stays distinguishable from a `,` inside a value. Path values are encoded, so no value can change the route it is spliced into. JSON content params; one 3.2 `in: querystring` parameter encoded as JSON or `application/x-www-form-urlencoded`; a parameter `default` is documented in rustdoc (never serde-wired) | examples are documentation-only; `allowReserved` on a parameter that is never encoded — `in: header`, or `style: cookie` — and the deprecated `allowEmptyValue` are acknowledged as having no effect (`W011`) | a style not permitted for the parameter's location; `spaceDelimited`/`pipeDelimited` with `explode: true`, which the specification's own table leaves undefined; nested arrays or objects inside a `simple`/`form` value; a `querystring` parameter without exactly one JSON or form-urlencoded content entry with a schema, or with a non-object form-urlencoded schema (`E010`); duplicate parameter identities, path-template mismatches, duplicate whole-query-string parameters, mixing `query` and `querystring`, or a style/location pairing the official schema forbids (`E011`) |
| Responses | single success status typed (`()` when bodyless); two or more documented success statuses, bodied or bodyless (e.g. `202` beside `204`), → a per-operation `{Method}Response` enum, except that a lone streaming or XML success body stays typed as the single body; multiple documented error bodies → a payload-carrying `{Method}Error` enum; enums carry one uniformly boxed variant per bodied status (`Status200`/`Status2xx`/`Default`) and a unit variant per bodyless status, decoded by dispatching on the HTTP status (exact before range before `default`) — no `serde(untagged)`, no `serde_json::Value`; no documented error body → an uninhabited error type, with undocumented statuses surfaced as `UnexpectedStatus`; **documented response headers** become a per-(operation, status) struct with `from_headers`/`from_response`, read as an explicit second step so a malformed header can never turn a successful call into a failure (raw headers stay on `ResponseValue`); a documented `Set-Cookie` is a list of its per-line values, never comma-joined — RFC 9110 §5.3 exempts it from the field-list rule and its values carry unescaped commas; 3.2 response `summary`/optional `description` become rustdoc | a documented `Content-Type` response header, which the specification says SHALL be ignored (`W011`) | - |
| Security | `http` bearer/basic and `apiKey` (header/query/cookie) attach registered credentials per operation `security` (first satisfiable alternative; a missing credential is a request-construction error); `oauth2`/`openIdConnect`, including 3.2 metadata/device-flow descriptions, accept a caller-supplied token attached as bearer; a `$ref`-valued security scheme is resolved; scheme documentation (`bearerFormat`, flows, `openIdConnectUrl`, deprecation) becomes rustdoc on credential registration; a 3.2 Security Requirement URI resolves with component-name precedence | `mutualTLS` is always satisfiable and attaches nothing — the transport's client certificate satisfies it (`W011`) | a requirement naming an undeclared scheme, or an `http` scheme other than `bearer`/`basic`, reported where it is declared rather than only where it is used (`E012`) |
| Document | servers lowered **with their variables**, generating a typed builder per templated server (an `enum` variable becomes a Rust enum, so an illegal value is unconstructible) plus `Client::with_default_server`; path-item and operation `servers` overrides are honored, sending just those operations to another base URL (absolute overrides replace the client's base, relative ones are joined onto it) with each server variable at its declared default — the choice is per call, so unlike the document's `servers` an override gets no typed builder; Path Item `$ref` is resolved, including across files; `requestBody.required` drives `&T` vs `Option<&T>` in the generated signature; Reference Object `summary`/`description` override the target's docs; 3.2 `$self` establishes canonical root identity for reference resolution; `QUERY` and every valid `additionalOperations` method generate client methods; Info `summary`/`contact`/`license`/`externalDocs`, Server `name`, path-item `summary`/`description`, operation tags, response metadata, and tag `summary`/`parent`/`kind` become rustdoc; tag parent integrity, unique `operationId`, parameter identity, and path-template integrity are checked | `webhooks`, operation `callbacks`, and response `links` acknowledged (`W002`), no code emitted; a `servers` entry past the first on a path item or operation, where the specification defines no client selection rule (`W011`) | malformed JSON/YAML or a structure rejected by the version-specific official OpenAPI schema, duplicate operation IDs, invalid tag hierarchies, semantic parameter/path conflicts, or a repeated path-template expression or server variable (`E011`); an object/mapping that declares the same key twice (`E022`); a Path Item `$ref` declared beside structural siblings, which the specification leaves *undefined* (`E016`) |
| Build cache | Build-time generation fingerprints the generator, complete transitive local/vendored input set, lock file, and config under Cargo's `OUT_DIR`; fingerprint labels are checkout-location- and path-separator-independent; missing, stale, or manually edited output is regenerated | - | - |
| Runtime dependencies | After lowering, build-script and proc-macro generation derive the crates/features referenced by that API and audit the consumer manifest before emitting code; `spargen deps <spec>` prints that exact `[dependencies]` block up front, from the same table the audit reads. Core floors are `bytes 1.12.1`, `reqwest 0.12.28` (default features off), `secrecy 0.10.3`, `serde 1.0.229` (`derive`), and `serde_json 1.0.151`. Conditional floors/features are `futures-core 0.3.32` plus `reqwest/stream` for sequential responses, `quick-xml 0.41.0/serialize` for XML, `uuid 1.24.0/serde` and `time 0.3.55/formatting,parsing` only for enabled mappings actually present, `reqwest/json` only for JSON requests, `reqwest/multipart` only for multipart requests, `bytes/serde` only for serialized aggregates containing bytes, and native optional `tokio 1.53.1/rt` only when the consumer declares `blocking`. Higher compatible caret floors are allowed; unrelated dependencies and features are ignored. Cargo resolves the accepted declaration and rustc proves the selected versions compile the emitted API | Generation outside a build script cannot emit rebuild triggers or find a manifest to audit, and says so (`W013`, or `W012` when only the manifest is missing) unless the caller declares `CargoIntegration::Off` | Cargo integration declared `Required` but unavailable (`E024`); a missing dependency/feature, a requirement admitting a version below the tested floor or crossing the next breaking line, reqwest defaults enabled, a renamed runtime crate, or invalid `blocking` wiring (`E023`) |
| Compatibility | exact omit rules for paths, operations, components, pointers, file-local pointers | matched omissions (`W009`) | unmatched/invalid omit rules (`E019`), invalid post-omit document (`E020`) |
| Targets | native targets (async client, plus the opt-in `blocking` client) and **`wasm32-unknown-unknown`** — the browser, via reqwest's `fetch` backend. The embedded runtime carries conditional `Send`/`Sync` bounds (`MaybeSend`/`MaybeSync`), so the transport seam, middleware/retry helpers, auth token providers, and streaming compile against reqwest's `!Send` wasm futures; consumers target-gate `tokio` native-only in their own manifest | on wasm the `blocking` client is unavailable (its tokio runtime cannot run on the single-threaded browser — the `BlockingClient` is `cfg`-gated off wasm) and streaming (`EventStream`) delivers non-incrementally, buffering the whole body once via `fetch` (the browser buffers it regardless; the API and items are identical). Native-only reqwest features configured on the injected client (timeouts, TLS backend, proxies) are browser/`fetch` limitations, not spargen ones | - |

Generated output is freestanding Rust with embedded support code and no `spargen` runtime
dependency. It compiles for native targets and `wasm32-unknown-unknown` (browser `fetch`); see the
**Targets** row for wasm limitations (no blocking client, non-incremental streaming).

### Request field presence

In models reachable from a selected request body, optional nullable fields use
`Option<Option<T>>`: `None` omits the property, `Some(None)` serializes JSON `null`, and
`Some(Some(value))` serializes the value. Optional non-nullable fields remain `Option<T>`;
required nullable fields remain `Option<T>` but must be present when deserializing. Required
non-nullable fields remain `T`. Null is rejected when the field's schema does not permit it.
A `default` never fills in an absent field.

This applies through referenced/nested models, arrays, tuples, map values and unions, including
models shared by requests and responses. Response-only models keep their existing representation.
Regenerating changes the Rust construction API of optional nullable request fields from `Some(v)`
to `Some(Some(v))`; use `Some(None)` to clear a JSON field. Multipart continues to omit null parts,
as it has no generic JSON-null part representation; this change does not define a new form encoding.

### Generated names

Type names come from the document. A `components/schemas` entry keeps its name, including a scalar
component (`pub type ProjectId = String;`). A `components/headers` entry referenced from responses
is lowered once for every response that uses it: an enum or object header is named after the
header component, a scalar one is spelled out, and a header whose schema is itself a `$ref` uses
that schema's type. A union variant whose member is a `$ref` is named after the referenced
component. Structs, string enums and unions without a name of their own take a positional name
(`<Parent><property>`, `Variant<n>`, `Item`).

`X | null` (`oneOf`/`anyOf` of one schema and `null`) is `Option<X>`: a `$ref` member is used as
is, never copied under the property's name, and the property's description moves onto the field.
When an intersection (`allOf`) leaves one side's shape unchanged, that side's type is reused.
`allOf` over a union plus named constraint components keeps the union's member types as variants
(dropping members no value can satisfy) and checks each constraint component against the JSON value
on deserialize and serialize; any other narrowed member is named `<Owner><Member>`.

Inline scalars, arrays, tuples, bytes, `null`, untyped values, integer/boolean enums and map-only
objects (`additionalProperties: <schema>` with no properties) get no name: the field, parameter,
header or variant spells out the Rust type (`String`, `i64`, `DateTime`, `Vec<types::Pet>`,
`std::collections::BTreeMap<String, T>`, …), and an inline property's description moves onto the
field. An inline string `const` (or single-value string `enum`) used as a struct field is a
`String` field whose value is checked when it is deserialized and serialized, so it accepts and
produces exactly the constant; as an operation parameter it is a `String` the client refuses to
send with any other value; used anywhere else it stays a one-variant enum. A type that lowering
produced but nothing in the API reaches gets no name and no item. An enum value with no letters or
digits is named after its symbols (`*` is `Asterisk`, `#` is `Hash`), and a leading sign is part of
the name (`-created_at` is `MinusCreatedAt`, `-Infinity` is `MinusInfinity`).

A single server with no variables and no 3.2 `name` generates only `servers::default_url()`; a
single templated server's builder is `servers::Server`, and several unnamed servers remain
`Server0`, `Server1`, ….

When two positions produce the same Rust identifier in one scope, all but the first get a stable
suffix hashed from their JSON Pointer. With `strict_names` on (`Spec::strict_names(true)` or
`strict_names = true` in `spargen.toml`) that is `E025` instead, listing every position that claims
the name, so no hash-suffixed name reaches the public API. It also reports `E026` for every used
type that could only be named by position: an inline union member, a tuple item, or a union with
two members referencing the same component.

### Exact JSON

Generated models accept every value the contract allows and write it back unchanged; the
validation-only keywords (`pattern`, lengths, bounds, sizes, formats, `uniqueItems`) are the
server's to enforce and are not checked by the client. Where serde's defaults would refuse or
alter a valid value:

* An `integer` accepts an integral number written with a fraction (`1.0`).
* An optional nullable member keeps absence and `null` apart as `Option<Option<T>>` (`None`
  absent, `Some(None)` null), in every model. In a model used for requests an optional
  non-nullable member is `Option<T>` and absent when `None`; in a response-only model it also
  reads `null` as `None`. An optional unconstrained (`{}`) member is `Option<serde_json::Value>`
  in every model: `None` absent, `Some(Value::Null)` a present `null`. An optional member declared
  `false` is refused whenever present, `null` included, so a union variant that forbids a member
  is never chosen for a value carrying it.
* An open object (no `additionalProperties`) keeps the members it does not declare in its
  `additional` map, so a decoded value serializes back unchanged. XML bodies keep their declared
  shape, since the XML codec cannot write a flattened map.
* A closed object (`additionalProperties: false`) ignores members it does not declare, so a
  response carrying a member added later still decodes. A trial-matched `oneOf`/`anyOf` first
  reads each variant exactly — undeclared members of a closed object count against it — and
  only if no variant matches exactly takes the variant that keeps the most of the value. When
  variants pin a required property to one value (`const`, or an `enum` of one value, such as
  `platform: "sms"`), the variants whose values the input carries are read first, the same way;
  the other variants (such as an open fallback) are read only if none of those reads it.
* A string enum of two or more values that a response can carry is open: besides the listed
  values it has an `Unknown(String)` variant (`Unknown<n>` if the contract lists a value named
  `Unknown`) holding any other string, which serializes back unchanged; `as_str` gives the wire
  value. An enum only requests use lists exactly the contract's values. Every generated string
  enum is `#[non_exhaustive]`. A trial union's exact pass counts an unlisted value against a
  variant.
* A `date-time` accepts every RFC 3339 spelling, lower-case `t`/`z` included, and, where the
  schema's `pattern` allows it, one without seconds or from year 0000. A pattern requiring three
  fraction digits is written that way.
* An open `prefixItems` array (no `items: false`) is a `Vec<serde_json::Value>`, so it may be
  shorter than the prefix or hold more items; `items: false` stays a Rust tuple.
* A `oneOf`/`anyOf` with a `null` member (the recursive `JsonValue` shape) accepts `null`.
* A `oneOf`/`anyOf` whose members are all the same scalar type, told apart only by
  validation-only keywords (string members that differ in `pattern`, say), is that scalar under
  the union's own name and description; as an enum, every value would match every member.
* A component name that is already a Rust type name keeps its exact spelling (`OAuthClient`).

Generated output requires `serde_json`'s `float_roundtrip` feature, so numbers are read exactly.
