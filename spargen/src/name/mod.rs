//! # Subsystem: name
//! layer-deps: ir, diag
//!
//! Deterministic identifier allocation: Rust-conventional casing via Unicode-XID-aware
//! segmentation, keyword escaping, in-scope collision resolution, and `operationId` synthesis.
//! Every allocation is deterministic and injective within its scope, and always yields a
//! valid Rust identifier — property-tested.

mod casing;
mod ident;
mod keyword;
mod scope;
mod synth;

use std::collections::{HashMap, HashSet};

use crate::diag::{Code, Diagnostic, Diagnostics, JsonPointer, Provenance};
use crate::ir::{AdditionalProps, Api, OperationId, ScalarRepr, ScalarValue, TypeId, TypeKind};

pub use casing::{to_pascal_case, to_snake_case};
pub use ident::Ident;
pub use keyword::{escape, IdentRole};
pub use scope::{Collision, Scope};
pub use synth::synth_operation_id;

/// The identifiers allocated for a whole [`Api`]: one per operation, params struct, type, field,
/// and variant. Codegen looks names up here rather than deriving them, so naming stays in one
/// place and stays deterministic.
#[derive(Debug, Default)]
pub struct Names {
    /// Method name per operation.
    pub operations: HashMap<OperationId, Ident>,
    /// Optional-parameters `…Params` struct name per operation.
    pub params_structs: HashMap<OperationId, Ident>,
    /// Generator-owned signature and request-building bindings per operation. Required OpenAPI
    /// parameters reserve their natural Rust spellings first, so these identifiers can never
    /// shadow caller-provided values.
    pub operation_bindings: HashMap<OperationId, OperationBindings>,
    /// Type name per named type. A type absent here is an [`inline`](Self::inline) type, or a
    /// lowering by-product nothing in the generated API reaches (which is not emitted).
    pub types: HashMap<TypeId, Ident>,
    /// Structural types at inline positions (a scalar, array, tuple, bytes, `null`, an untyped
    /// value, a non-string scalar enum, a map-only object, or a string constant used only as a
    /// struct field) that no document name covers. They get no alias name: codegen spells their
    /// Rust type out at every use site.
    pub inline: HashMap<TypeId, TypeKind>,
    /// The inline string constants (a single-value string `const`/`enum` used only as struct
    /// fields), by wire value. Such a field is a plain `String` whose value is checked when it is
    /// deserialized and serialized, so the wire accepts and produces exactly what it did as a
    /// one-variant enum.
    pub consts: HashMap<TypeId, String>,
    /// The private checker-function stem per string-constant value (`const_not_found`). These
    /// are module-private helpers, not public names.
    pub const_checkers: HashMap<String, Ident>,
    /// Field name per `(type, wire property name)`.
    pub fields: HashMap<(TypeId, String), Ident>,
    /// The synthetic `#[serde(flatten)]` overflow-map field ident per struct that has a typed
    /// `additionalProperties`/`patternProperties` map. Allocated in the struct's field scope
    /// (reserved after the declared fields) so it can never collide with a declared property named
    /// `additional`.
    pub struct_overflow: HashMap<TypeId, Ident>,
    /// Variant name per `(type, wire variant value)`.
    pub variants: HashMap<(TypeId, String), Ident>,
    /// Builder type name per declared server, by index.
    pub servers: Vec<Ident>,
    /// Enum type name per `(server index, variable name)`, for a variable with a closed `enum`.
    pub server_variable_enums: HashMap<(usize, String), Ident>,
    /// Enum variant name per `(server index, variable name, value)`.
    pub server_variable_variants: HashMap<(usize, String, String), Ident>,
    /// Setter/field name per `(server index, variable name)`.
    pub server_variable_fields: HashMap<(usize, String), Ident>,
    /// Header-struct type name per `(operation, status label)`.
    pub response_header_structs: HashMap<(OperationId, String), Ident>,
    /// Field name per `(operation, status label, header name)`.
    pub response_header_fields: HashMap<(OperationId, String, String), Ident>,
}

/// Generator-owned bindings emitted inside one operation method.
#[derive(Debug)]
pub struct OperationBindings {
    /// The optional-parameters struct argument, when one is emitted.
    pub params: Option<Ident>,
    /// The request-body argument, when one is emitted.
    pub body: Option<Ident>,
    /// Mutable path assembled before URL construction.
    pub path: Ident,
    /// Mutable query-pair collection.
    pub query: Ident,
    /// Serialized whole-query value for an `in: querystring` parameter.
    pub raw_query: Ident,
    /// Fully constructed request URL.
    pub url: Ident,
    /// Mutable request builder, then the built request.
    pub request: Ident,
    /// Clone of a streaming request retained for opt-in SSE reconnects.
    pub reconnect_request: Ident,
    /// Mutable cookie-fragment collection.
    pub cookies: Ident,
}

/// Options that change how identifiers are allocated.
#[derive(Debug, Clone, Copy, Default)]
pub struct NameOptions {
    /// Refuse to disambiguate a public identifier with a hash suffix: every such collision is
    /// reported as `E025` instead, listing the positions that claim the same name.
    pub strict: bool,
}

/// Whether a type is spelled out at its use sites instead of receiving an alias name: a structural
/// kind (whose Rust spelling is a plain type expression) at a position no document name covers.
/// `field_only` holds the types used exclusively as struct field types.
fn is_inline(api: &Api, id: TypeId, kind: &TypeKind, field_only: &HashSet<TypeId>) -> bool {
    if api.types.is_named(id) {
        return false;
    }
    match kind {
        TypeKind::Primitive(_)
        | TypeKind::Array(_)
        | TypeKind::Tuple(_)
        | TypeKind::Bytes
        | TypeKind::Null
        | TypeKind::Any => true,
        // A map-only object (`additionalProperties: <schema>`, no properties) is a map.
        TypeKind::Struct(object) => {
            object.fields.is_empty() && matches!(object.additional, AdditionalProps::Typed(_))
        }
        TypeKind::Enum(enumeration) => match enumeration.repr {
            ScalarRepr::Int | ScalarRepr::Bool => true,
            // A string constant is a checked `String` field or parameter; elsewhere (an item, a
            // variant, a header) it stays a one-variant enum, which is what enforces its value.
            ScalarRepr::String => enumeration.variants.len() == 1 && field_only.contains(&id),
        },
        TypeKind::Never | TypeKind::Union(_) => false,
    }
}

/// The live types referenced only as the type of a struct field or an operation parameter, never
/// as an item, variant, map value, body or header.
fn field_only_types(api: &Api, live: &HashSet<TypeId>) -> HashSet<TypeId> {
    let mut fields = HashSet::new();
    let mut elsewhere = HashSet::new();
    for operation in &api.operations {
        // A parameter is a checked `String` too: the client rejects any other value before
        // sending, exactly the values its one-variant enum could express.
        fields.extend(operation.params.iter().map(|param| param.ty.id));
        if let Some(ty) = operation.request_body.as_ref().and_then(|body| body.ty) {
            elsewhere.insert(ty.id);
        }
        for response in operation
            .responses
            .by_status
            .iter()
            .map(|(_, response)| response)
            .chain(operation.responses.default.as_ref())
        {
            elsewhere.extend(response.body.map(|ty| ty.id));
            elsewhere.extend(response.headers.iter().map(|header| header.ty.id));
        }
    }
    for (id, def) in api.types.iter() {
        if !live.contains(&id) {
            continue;
        }
        match &def.kind {
            TypeKind::Struct(object) => {
                fields.extend(object.fields.iter().map(|field| field.ty.id));
                if let AdditionalProps::Typed(ty) = &object.additional {
                    elsewhere.insert(ty.id);
                }
            }
            TypeKind::Array(ty) => {
                elsewhere.insert(ty.id);
            }
            TypeKind::Tuple(items) => elsewhere.extend(items.iter().map(|ty| ty.id)),
            TypeKind::Union(union) => {
                elsewhere.extend(union.variants.iter().map(|variant| variant.ty.id));
            }
            _ => {}
        }
    }
    fields.retain(|id| !elsewhere.contains(id));
    fields
}

/// Every type the generated API can reach: the document-named types, plus everything an
/// operation's parameters, request body, response bodies and response headers use, closed over
/// fields, items, map values and union variants. Anything else is a lowering by-product (for
/// example the member a single-member nullable union re-emits under its own position) that no
/// generated signature or model mentions, so it gets no name and no item.
fn live_types(api: &Api) -> HashSet<TypeId> {
    let mut stack: Vec<TypeId> = api
        .types
        .iter()
        .filter(|(id, _)| api.types.is_named(*id))
        .map(|(id, _)| id)
        .collect();
    for operation in &api.operations {
        stack.extend(operation.params.iter().map(|param| param.ty.id));
        if let Some(ty) = operation.request_body.as_ref().and_then(|body| body.ty) {
            stack.push(ty.id);
        }
        for response in operation
            .responses
            .by_status
            .iter()
            .map(|(_, response)| response)
            .chain(operation.responses.default.as_ref())
        {
            stack.extend(response.body.map(|ty| ty.id));
            stack.extend(response.headers.iter().map(|header| header.ty.id));
        }
    }
    let mut live = HashSet::new();
    while let Some(id) = stack.pop() {
        if !live.insert(id) {
            continue;
        }
        match api.types.get(id).map(|def| &def.kind) {
            Some(TypeKind::Struct(object)) => {
                stack.extend(object.fields.iter().map(|field| field.ty.id));
                if let AdditionalProps::Typed(ty) = &object.additional {
                    stack.push(ty.id);
                }
            }
            Some(TypeKind::Array(ty)) => stack.push(ty.id),
            Some(TypeKind::Tuple(items)) => stack.extend(items.iter().map(|ty| ty.id)),
            Some(TypeKind::Union(union)) => {
                stack.extend(union.variants.iter().map(|variant| variant.ty.id));
            }
            _ => {}
        }
    }
    live
}

/// Collects strict-mode collisions from every public scope, labelled by what the scope names.
#[derive(Default)]
struct CollisionLog {
    /// `(scope index, what the scope names, collision)`. The same identifier in two different
    /// scopes (two enums that each have a clash) is two separate problems.
    entries: Vec<(usize, &'static str, Collision)>,
    scopes: usize,
}

impl CollisionLog {
    fn drain(&mut self, what: &'static str, scope: &mut Scope) {
        let index = self.scopes;
        self.scopes += 1;
        self.entries.extend(
            scope
                .take_collisions()
                .into_iter()
                .map(|collision| (index, what, collision)),
        );
    }

    /// One `E025` per contested identifier in a scope, listing every position that claims it.
    fn report(self, diags: &mut Diagnostics) {
        type Key = (usize, &'static str, String);
        let mut groups: indexmap::IndexMap<Key, Vec<JsonPointer>> = indexmap::IndexMap::new();
        for (index, what, collision) in self.entries {
            let positions = groups.entry((index, what, collision.ident)).or_default();
            if let Some(first) = collision.first {
                if !positions.contains(&first) {
                    positions.push(first);
                }
            }
            if !positions.contains(&collision.other) {
                positions.push(collision.other);
            }
        }
        for ((_, what, ident), positions) in groups {
            let at = positions.last().cloned().unwrap_or_else(JsonPointer::root);
            let listed = positions
                .iter()
                .map(|pointer| {
                    if pointer.as_str().is_empty() {
                        "`#` (document root)".to_owned()
                    } else {
                        format!("`#{}`", pointer.as_str())
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            Diagnostic::error(Code::GeneratedNameCollision, Provenance::new(at, None))
                .message(format!(
                    "{} positions would all generate the {what} name `{ident}`, so all but one \
                     would need a hash-suffixed name: {listed}",
                    positions.len().max(2)
                ))
                .remedy(
                    "give each position its own name in the document (for a schema, a \
                     distinct `components/schemas` entry; for a header, a \
                     `components/headers` entry), or turn `strict_names` off to accept \
                     hash-suffixed names",
                )
                .emit(diags);
        }
    }
}

/// Allocate every identifier the API needs, in one deterministic pass. Naming conflicts
/// that cannot be resolved are reported through `diags`: with [`NameOptions::strict`], every
/// public identifier that would need a hash suffix is an `E025` error.
pub fn allocate(api: &Api, options: &NameOptions, diags: &mut Diagnostics) -> Names {
    let mut names = Names::default();
    let strict = options.strict;
    let mut log = CollisionLog::default();

    // Servers live in their own module, so they get their own scopes and can never collide with a
    // generated model or operation name. A lone unnamed server is simply `Server`; several unnamed
    // servers can only be told apart by position.
    let mut server_scope = Scope::strict(strict);
    let mut server_enum_scope = Scope::strict(strict);
    let single_server = api.servers.len() == 1;
    for (index, server) in api.servers.iter().enumerate() {
        let hint = server.name.clone().unwrap_or_else(|| {
            if single_server {
                "server".to_owned()
            } else {
                format!("server{index}")
            }
        });
        let pointer = JsonPointer::root();
        let position = JsonPointer::root().push("servers").index(index);
        names
            .servers
            .push(server_scope.alloc_at(&hint, IdentRole::Type, &pointer, &position));
        let mut field_scope = Scope::strict(strict);
        for (variable_name, variable) in &server.variables {
            names.server_variable_fields.insert(
                (index, variable_name.clone()),
                field_scope.alloc(variable_name, IdentRole::Field, &pointer),
            );
            if variable.enum_values.is_empty() {
                continue;
            }
            names.server_variable_enums.insert(
                (index, variable_name.clone()),
                server_enum_scope.alloc(
                    &format!("{hint} {variable_name}"),
                    IdentRole::Type,
                    &pointer,
                ),
            );
            let mut variant_scope = Scope::strict(strict);
            for value in &variable.enum_values {
                names.server_variable_variants.insert(
                    (index, variable_name.clone(), value.clone()),
                    variant_scope.alloc(value, IdentRole::Variant, &pointer),
                );
            }
            log.drain("server variable value", &mut variant_scope);
        }
        log.drain("server variable", &mut field_scope);
    }
    log.drain("server", &mut server_scope);
    log.drain("server variable enum", &mut server_enum_scope);

    let live = live_types(api);
    let field_only = field_only_types(api, &live);
    let mut type_scope = Scope::strict(strict);
    for (id, def) in api.types.iter() {
        if !live.contains(&id) {
            continue;
        }
        if is_inline(api, id, &def.kind, &field_only) {
            if let TypeKind::Enum(enumeration) = &def.kind {
                if let [ScalarValue::String(value)] = enumeration.variants.as_slice() {
                    names.consts.insert(id, value.clone());
                }
            }
            names.inline.insert(id, def.kind.clone());
            continue;
        }
        names.types.insert(
            id,
            type_scope.alloc(&def.name_hint, IdentRole::Type, &def.provenance.pointer),
        );
    }

    // Response-header structs live in the same scope as the other per-operation types, so a
    // documented header can never collide with a generated model.
    for operation in &api.operations {
        let responses = operation
            .responses
            .by_status
            .iter()
            .map(|(spec, response)| (status_label(Some(*spec)), response))
            .chain(
                operation
                    .responses
                    .default
                    .as_ref()
                    .map(|response| (status_label(None), response)),
            );
        for (label, response) in responses {
            if response.headers.is_empty() {
                continue;
            }
            let hint = format!("{} {label} headers", operation.id.0);
            let pointer = JsonPointer::root();
            let position = operation.provenance.pointer.push("responses");
            names.response_header_structs.insert(
                (operation.id.clone(), label.clone()),
                type_scope.alloc_at(&hint, IdentRole::Type, &pointer, &position),
            );
            let mut field_scope = Scope::strict(strict);
            for header in &response.headers {
                names.response_header_fields.insert(
                    (operation.id.clone(), label.clone(), header.name.clone()),
                    field_scope.alloc_at(
                        &header.name,
                        IdentRole::Field,
                        &pointer,
                        &position.push("headers").push(&header.name),
                    ),
                );
            }
            log.drain("response header field", &mut field_scope);
        }
    }
    log.drain("type", &mut type_scope);
    if strict {
        for (id, def) in api.types.iter() {
            let (Some(reason), Some(ident)) = (&def.positional, names.types.get(&id)) else {
                continue;
            };
            if api.types.is_named(id) {
                continue;
            }
            Diagnostic::error(
                Code::GeneratedPositionalName,
                Provenance::new(def.provenance.pointer.clone(), None),
            )
            .message(format!(
                "`{}` would be named by position: {reason}",
                ident.as_str()
            ))
            .remedy(
                "give the schema its own `components/schemas` entry and reference it, or turn \
                 `strict_names` off to accept positional names",
            )
            .emit(diags);
        }
    }

    // Checker helpers are private functions, allocated in value order so their spellings do not
    // depend on where the constants appear.
    let values: std::collections::BTreeSet<&String> = names.consts.values().collect();
    let mut checker_scope = Scope::default();
    for value in values {
        let ident = checker_scope.alloc(
            &format!("const {value}"),
            IdentRole::Method,
            &JsonPointer::root().push(value),
        );
        names.const_checkers.insert(value.clone(), ident);
    }

    let mut operation_scope = Scope::strict(strict);
    let mut params_scope = Scope::strict(strict);
    for operation in &api.operations {
        names.operations.insert(
            operation.id.clone(),
            operation_scope.alloc(
                &operation.id.0,
                IdentRole::Method,
                &operation.provenance.pointer,
            ),
        );
        names.params_structs.insert(
            operation.id.clone(),
            params_scope.alloc(
                &format!("{} params", operation.id.0),
                IdentRole::Type,
                &operation.provenance.pointer,
            ),
        );

        // Required parameters are fixed by the generated public surface. Reserve their natural
        // spellings, then allocate every generator-owned binding in the same lexical scope so the
        // implementation yields on collision without renaming ordinary arguments. These bindings
        // are local to the method body, not public names, so this scope is never strict.
        let mut binding_scope = Scope::default();
        for parameter in operation
            .params
            .iter()
            .filter(|parameter| parameter.required)
        {
            binding_scope.reserve(&parameter.name, IdentRole::Param);
        }
        let pointer = &operation.provenance.pointer;
        let params = operation
            .params
            .iter()
            .any(|parameter| !parameter.required)
            .then(|| binding_scope.alloc("params", IdentRole::Param, pointer));
        let body = operation
            .request_body
            .as_ref()
            .and_then(|request_body| request_body.ty)
            .map(|_| binding_scope.alloc("body", IdentRole::Param, pointer));
        names.operation_bindings.insert(
            operation.id.clone(),
            OperationBindings {
                params,
                body,
                path: binding_scope.alloc("path", IdentRole::Param, pointer),
                query: binding_scope.alloc("query", IdentRole::Param, pointer),
                raw_query: binding_scope.alloc("raw_query", IdentRole::Param, pointer),
                url: binding_scope.alloc("url", IdentRole::Param, pointer),
                request: binding_scope.alloc("request", IdentRole::Param, pointer),
                reconnect_request: binding_scope.alloc(
                    "reconnect_request",
                    IdentRole::Param,
                    pointer,
                ),
                cookies: binding_scope.alloc("cookies", IdentRole::Param, pointer),
            },
        );
    }

    log.drain("client method", &mut operation_scope);
    log.drain("parameters struct", &mut params_scope);

    for (id, def) in api.types.iter() {
        if !live.contains(&id) {
            continue;
        }
        match &def.kind {
            // Inline types have no fields or variants of their own to name.
            _ if names.inline.contains_key(&id) => {}
            TypeKind::Struct(object) => {
                let mut scope = Scope::strict(strict);
                for field in &object.fields {
                    names.fields.insert(
                        (id, field.name.wire.clone()),
                        scope.alloc_at(
                            &field.name.wire,
                            IdentRole::Field,
                            &def.provenance.pointer,
                            &def.provenance
                                .pointer
                                .push("properties")
                                .push(&field.name.wire),
                        ),
                    );
                }
                // The flatten overflow field shares the struct's field scope, so it is disambiguated
                // against any declared property (e.g. one named `additional`) instead of emitting a
                // second literal `additional` field that would fail to compile.
                if matches!(object.additional, AdditionalProps::Typed(_)) {
                    names.struct_overflow.insert(
                        id,
                        scope.alloc("additional", IdentRole::Field, &def.provenance.pointer),
                    );
                }
                log.drain("field", &mut scope);
            }
            TypeKind::Enum(enumeration) => {
                let mut scope = Scope::strict(strict);
                for (index, variant) in enumeration.variants.iter().enumerate() {
                    let value = match variant {
                        ScalarValue::Bool(value) => value.to_string(),
                        ScalarValue::Int(value) => value.to_string(),
                        ScalarValue::String(value) => value.clone(),
                    };
                    names.variants.insert(
                        (id, value.clone()),
                        scope.alloc_at(
                            &value,
                            IdentRole::Variant,
                            &def.provenance.pointer,
                            &def.provenance.pointer.push("enum").index(index),
                        ),
                    );
                }
                log.drain("enum variant", &mut scope);
            }
            TypeKind::Union(union) => {
                // Union variants share the scalar-enum `variants` table, keyed by `(TypeId, hint)`.
                // A type id is either an enum or a union, so the two never collide; hints are made
                // unique per union at lowering time, keeping this allocation injective in scope.
                let mut scope = Scope::strict(strict);
                for variant in &union.variants {
                    names.variants.insert(
                        (id, variant.name_hint.clone()),
                        scope.alloc(
                            &variant.name_hint,
                            IdentRole::Variant,
                            &def.provenance.pointer,
                        ),
                    );
                }
                log.drain("union variant", &mut scope);
            }
            _ => {}
        }
    }

    log.report(diags);
    names
}

/// The stable label for one documented status, shared by naming and codegen so a header struct and
/// its response variant always agree.
pub fn status_label(spec: Option<crate::ir::StatusSpec>) -> String {
    match spec {
        Some(crate::ir::StatusSpec::Exact(code)) => format!("Status{code}"),
        Some(crate::ir::StatusSpec::Range(0)) | None => "Default".to_owned(),
        Some(crate::ir::StatusSpec::Range(prefix)) => format!("Status{prefix}xx"),
    }
}
