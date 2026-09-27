//! Internal token builders. Each produces a deterministically-ordered fragment of the output;
//! [`generate`](super::generate) assembles and formats them.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use std::collections::{BTreeMap, BTreeSet};

use crate::ir::{
    AdditionalProps, Api, ApiKeyLoc, DisjointFeature, Docs, ErrorShape, Field, HttpScheme,
    JsonCategory, MediaType, Operation, ParamLoc, Prim, ScalarRepr, ScalarValue, SecurityScheme,
    SuccessShape, Ty, TypeDef, TypeId, TypeKind, UnionMode, UnionStrategy,
};
use crate::name::{Names, OperationBindings};

use super::CodegenOptions;

/// Emit the `types` (models) module for every type in the graph, in deterministic order.
pub(crate) fn emit_models(api: &Api, names: &Names, options: &CodegenOptions) -> TokenStream {
    let requests = request_model_types(api);
    let closed_enums = request_only_types(api);
    // Only named types get an item. Inline types are spelled out where they are used, and a type
    // with no name at all is a lowering by-product nothing in the API reaches.
    let items = api
        .types
        .iter()
        .filter(|(id, _)| names.types.contains_key(id))
        .map(|(id, def)| {
            emit_type_def(
                id,
                def,
                api,
                names,
                options,
                requests.contains(&id),
                !closed_enums.contains(&id),
            )
        });
    let presence_helper = api.types.iter().any(|(_, def)| matches!(def.kind, TypeKind::Struct(_))).then(|| {
        quote! {
            // Serde's field default handles absence. For a present field, deserialize its actual
            // schema type first, preserving null only when that type permits it.
            fn deserialize_request_field<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
            where
                D: serde::Deserializer<'de>,
                T: Deserialize<'de>,
            {
                T::deserialize(deserializer).map(Some)
            }
        }
    });
    // The RFC 3339 newtypes live beside `types`, so bring them into scope under the same bare names
    // `prim_tokens` emits; at the generated root the prelude re-export supplies them instead.
    let datetime_import = (options.feature_time && api.uses_time()).then(|| {
        quote! { use super::{Date, DateTime}; }
    });
    let const_helpers = emit_const_helpers(api, names, &requests);
    let mut field_helpers = BTreeMap::new();
    for (id, def) in api.types.iter() {
        if !names.types.contains_key(&id) {
            continue;
        }
        if let TypeKind::Struct(object) = &def.kind {
            if let AdditionalProps::Typed(item) = &object.additional {
                if let Some((helper, tokens)) = emit_overflow_helper(**item, api, names, options) {
                    field_helpers.entry(helper.to_string()).or_insert(tokens);
                }
            }
            for field in &object.fields {
                if let Some((helper, tokens)) =
                    emit_field_helper(field, api, names, options, requests.contains(&id))
                {
                    field_helpers.entry(helper.to_string()).or_insert(tokens);
                }
            }
        }
    }
    let field_helpers = field_helpers.into_values();
    quote! {
        #[forbid(unsafe_code)]
        #[allow(dead_code, unused_imports, clippy::deref_addrof, clippy::needless_borrow, clippy::needless_borrows_for_generic_args, clippy::redundant_closure_call, clippy::get_first, clippy::explicit_auto_deref)]
        pub mod types {
            use serde::{Deserialize, Serialize};
            use std::collections::BTreeMap;
            #datetime_import
            #presence_helper
            #const_helpers
            #(#field_helpers)*

            #(#items)*
        }
    }
}

/// Follow the selected request bodies through references, containers and unions once. Shared
/// request/response models keep one identity; response-only models retain their existing API.
fn request_model_types(api: &Api) -> BTreeSet<TypeId> {
    let mut pending: Vec<_> = api
        .operations
        .iter()
        .filter_map(|operation| operation.request_body.as_ref()?.ty.map(|ty| ty.id))
        .collect();
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        match api.types.get(id).map(|def| &def.kind) {
            Some(TypeKind::Struct(object)) => {
                pending.extend(object.fields.iter().map(|field| field.ty.id));
                if let AdditionalProps::Typed(ty) = &object.additional {
                    pending.push(ty.id);
                }
            }
            Some(TypeKind::Array(ty)) => pending.push(ty.id),
            Some(TypeKind::Tuple(items)) => pending.extend(items.iter().map(|ty| ty.id)),
            Some(TypeKind::Union(union)) => {
                pending.extend(union.variants.iter().map(|variant| variant.ty.id));
            }
            _ => {}
        }
    }
    seen
}

/// The types reached from the given roots through fields, overflow maps, items, variants and
/// union constraints.
fn reachable_types(roots: impl IntoIterator<Item = TypeId>, api: &Api) -> BTreeSet<TypeId> {
    let mut pending: Vec<TypeId> = roots.into_iter().collect();
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        match api.types.get(id).map(|def| &def.kind) {
            Some(TypeKind::Struct(object)) => {
                pending.extend(object.fields.iter().map(|field| field.ty.id));
                if let AdditionalProps::Typed(ty) = &object.additional {
                    pending.push(ty.id);
                }
            }
            Some(TypeKind::Array(ty)) => pending.push(ty.id),
            Some(TypeKind::Tuple(items)) => pending.extend(items.iter().map(|ty| ty.id)),
            Some(TypeKind::Union(union)) => {
                pending.extend(union.variants.iter().map(|variant| variant.ty.id));
                // A value also passes through each constraint (`allOf [<union>, <constraint>]`).
                pending.extend(union.constraints.iter().map(|ty| ty.id));
            }
            _ => {}
        }
    }
    seen
}

/// The types only requests use (bodies and parameters), never a response body or header. Their
/// string enums stay closed: a client sends only the values the contract lists. Every other string
/// enum of two or more values is open, so a value the server adds later still decodes.
fn request_only_types(api: &Api) -> BTreeSet<TypeId> {
    let requests = reachable_types(
        api.operations.iter().flat_map(|operation| {
            let body = operation
                .request_body
                .as_ref()
                .and_then(|body| body.ty.map(|ty| ty.id));
            body.into_iter()
                .chain(operation.params.iter().map(|param| param.ty.id))
        }),
        api,
    );
    let responses = reachable_types(
        api.operations.iter().flat_map(|operation| {
            operation
                .responses
                .by_status
                .iter()
                .map(|(_, response)| response)
                .chain(operation.responses.default.as_ref())
                .flat_map(|response| {
                    response
                        .body
                        .map(|ty| ty.id)
                        .into_iter()
                        .chain(response.headers.iter().map(|header| header.ty.id))
                })
                .collect::<Vec<_>>()
        }),
        api,
    );
    requests.difference(&responses).copied().collect()
}

/// The declared security schemes, rendered as rustdoc for `with_credential`.
///
/// This is the one place a caller chooses what to register, so it is where the scheme's own
/// documentation belongs: the bearer format, the flows that mint a token, the OpenID Connect
/// discovery URL, and whether the scheme is deprecated.
fn scheme_doc_lines(api: &Api) -> Vec<TokenStream> {
    let documented: Vec<&String> = api
        .security_schemes
        .values()
        .flat_map(|scheme| scheme.docs.iter())
        .collect();
    if documented.is_empty() {
        return Vec::new();
    }
    let mut lines = vec![
        String::new(),
        "# Declared schemes".to_owned(),
        String::new(),
    ];
    lines.extend(documented.into_iter().cloned());
    lines
        .into_iter()
        .map(|line| quote! { #[doc = #line] })
        .collect()
}

/// Emit the `Client` struct and its `new` / `with_client` constructors.
pub(crate) fn emit_client(api: &Api, names: &Names, options: &CodegenOptions) -> TokenStream {
    let scheme_docs = scheme_doc_lines(api);
    let params = api
        .operations
        .iter()
        .filter(|operation| operation.params.iter().any(|param| !param.required))
        .map(|operation| emit_params_struct(operation, names, options));
    let errors = api
        .operations
        .iter()
        .map(|operation| emit_error_enum(operation, names, options));
    let response_enums = api
        .operations
        .iter()
        .map(|operation| emit_response_enum(operation, names, options));
    let methods = api
        .operations
        .iter()
        .map(|operation| emit_operation(operation, api, names, options));
    let response_headers = api
        .operations
        .iter()
        .map(|operation| emit_response_headers(operation, api, names, options));
    let client_docs = client_doc_tokens(api);
    let error_body_cap = options.error_body_cap;
    let servers = emit_servers(api, names);
    let default_server = (!api.servers.is_empty()).then(|| {
        quote! {
            /// Build a client on the first server the specification declares, with every server
            /// variable at its declared default.
            pub fn with_default_server() -> Result<Self, support::Error<std::convert::Infallible>> {
                Self::new(&servers::default_url())
            }
        }
    });
    quote! {
        #(#params)*
        #(#errors)*
        #(#response_enums)*
        #(#response_headers)*
        #servers

        #client_docs
        #[allow(dead_code)]
        pub struct Client {
            core: support::ClientCore,
        }

        #[forbid(unsafe_code)]
        #[allow(dead_code, unused_mut, unused_variables, clippy::result_large_err, clippy::deref_addrof, clippy::needless_borrow, clippy::needless_borrows_for_generic_args, clippy::redundant_closure_call, clippy::get_first, clippy::explicit_auto_deref)]
        impl Client {
            pub fn new(base_url: &str) -> Result<Self, support::Error<std::convert::Infallible>> {
                Self::with_client(reqwest::Client::new(), base_url)
            }

            #default_server

            pub fn with_client(
                client: reqwest::Client,
                base_url: &str,
            ) -> Result<Self, support::Error<std::convert::Infallible>> {
                let mut core = support::ClientCore::with_client(client, base_url)?;
                core.config_mut().max_error_body = #error_body_cap;
                Ok(Self { core })
            }

            /// Build a client over a caller-supplied transport backend — the injection point for
            /// retry, middleware, or a non-reqwest transport. Requests are still built on a default
            /// `reqwest::Client`; only the execute step goes through the backend.
            pub fn with_backend(
                backend: std::sync::Arc<dyn support::HttpBackend>,
                base_url: &str,
            ) -> Result<Self, support::Error<std::convert::Infallible>> {
                let mut core = support::ClientCore::with_backend(backend, base_url)?;
                core.config_mut().max_error_body = #error_body_cap;
                Ok(Self { core })
            }

            pub fn core(&self) -> &support::ClientCore {
                &self.core
            }

            /// Register a credential for a named security scheme. Operations whose `security`
            /// requirement cannot be satisfied by the registered credentials fail with a
            /// request-construction error before anything is sent.
            #(#scheme_docs)*
            #[must_use]
            pub fn with_credential(
                mut self,
                scheme: &str,
                credential: support::Credential,
            ) -> Self {
                self.core.set_credential(scheme, credential);
                self
            }

            #(#methods)*
        }
    }
}

/// Emit one operation method — a thin `#[inline]` shim over the non-generic `support` dispatch
/// routines, so per-operation code stays tiny.
pub(crate) fn emit_operation(
    operation: &Operation,
    api: &Api,
    names: &Names,
    options: &CodegenOptions,
) -> TokenStream {
    let bindings = operation_bindings(operation, names);
    let path_binding = &bindings.path;
    let query_binding = &bindings.query;
    let raw_query_binding = &bindings.raw_query;
    let url_binding = &bindings.url;
    let request_binding = &bindings.request;
    let reconnect_request_binding = &bindings.reconnect_request;
    let cookies_binding = &bindings.cookies;
    let method_ident = names
        .operations
        .get(&operation.id)
        .expect("operation name allocated");
    let error_ident = format_ident!("{}Error", to_pascal(method_ident.as_str()));
    let reqwest_method = reqwest_method(&operation.method);
    let success_ty = success_type(operation, names, options);
    let error_ty = quote! { #error_ident };
    let docs = doc_tokens(&operation.docs);
    // Required parameters are positional method arguments with no attribute slot of their own, so a
    // required parameter's `default` is surfaced in the method rustdoc instead.
    let param_default_docs = param_default_docs_tokens(operation);
    let deprecated = operation.deprecated.then(|| quote! { #[deprecated] });

    // The typed argument list (required params, the optional-params struct, the body) is shared
    // verbatim with the blocking shim so the two signatures can never drift.
    let (args, _arg_names) = operation_args(operation, names, options);

    // A constant parameter is a plain `String`; anything but the constant is refused before a
    // request is built, so only the value the one-variant enum could express is ever sent.
    let const_checks: Vec<TokenStream> = operation
        .params
        .iter()
        .filter_map(|param| {
            let value = names.consts.get(&param.ty.id)?;
            let message = format!("parameter `{}` must be the constant {value:?}", param.name);
            let refuse_other = |binding: TokenStream| {
                quote! {
                    if #binding != #value {
                        return Err(support::Error::request_message(#message));
                    }
                }
            };
            if param.required {
                let ident = param_ident(param, crate::name::IdentRole::Param);
                if param.ty.nullable {
                    Some(quote! {
                        if #ident.as_deref().is_some_and(|value| value != #value) {
                            return Err(support::Error::request_message(#message));
                        }
                    })
                } else {
                    Some(refuse_other(quote! { #ident }))
                }
            } else {
                let ident = param_ident(param, crate::name::IdentRole::Field);
                let params_binding = bindings
                    .params
                    .as_ref()
                    .expect("optional parameters argument allocated");
                // An optional parameter's field is a single `Option<String>` whether or not it is
                // nullable (absent and `null` both collapse to `None`), so the value bound below is
                // already a plain `&String`.
                let inner = refuse_other(quote! { value });
                Some(quote! {
                    if let Some(value) = #params_binding
                        .as_ref()
                        .and_then(|params| params.#ident.as_ref())
                    {
                        #inner
                    }
                })
            }
        })
        .collect();
    let path_init = operation.path.raw.clone();
    let path_replacements = operation
        .params
        .iter()
        .filter(|param| param.location == ParamLoc::Path)
        .map(|param| {
            let placeholder = format!("{{{}}}", param.name);
            let ident = param_ident(param, crate::name::IdentRole::Param);
            let value = param_value_tokens(param, quote! { &#ident });
            quote! {
                #path_binding = #path_binding.replace(#placeholder, &#value);
            }
        });
    let required_query = operation
        .params
        .iter()
        .filter(|param| param.required && param.location == ParamLoc::Query)
        .map(|param| {
            let name = param.name.clone();
            let ident = param_ident(param, crate::name::IdentRole::Param);
            query_param_tokens(param, &name, quote! { &#ident }, query_binding)
        });
    let optional_query = operation
        .params
        .iter()
        .filter(|param| !param.required && param.location == ParamLoc::Query)
        .map(|param| {
            let name = param.name.clone();
            let ident = param_ident(param, crate::name::IdentRole::Field);
            let params_binding = bindings
                .params
                .as_ref()
                .expect("optional parameters argument allocated");
            let serialize = query_param_tokens(param, &name, quote! { value }, query_binding);
            quote! {
                if let Some(value) = #params_binding
                    .as_ref()
                    .and_then(|params| params.#ident.as_ref())
                {
                    #serialize
                }
            }
        });
    let uses_querystring = operation
        .params
        .iter()
        .any(|parameter| parameter.location == ParamLoc::QueryString);
    let uses_json_querystring = operation.params.iter().any(|parameter| {
        parameter.location == ParamLoc::QueryString
            && matches!(
                &parameter.style,
                crate::ir::ParamStyle::Content(MediaType::Json)
            )
    });
    let raw_query_init = uses_querystring.then(|| {
        if uses_json_querystring {
            quote! { let mut #raw_query_binding: Option<String> = None; }
        } else {
            quote! { let #raw_query_binding: Option<String> = None; }
        }
    });
    let required_querystring = operation
        .params
        .iter()
        .filter(|parameter| parameter.required && parameter.location == ParamLoc::QueryString)
        .map(|parameter| {
            let ident = param_ident(parameter, crate::name::IdentRole::Param);
            querystring_param_tokens(
                parameter,
                quote! { &#ident },
                query_binding,
                raw_query_binding,
            )
        });
    let optional_querystring = operation
        .params
        .iter()
        .filter(|parameter| !parameter.required && parameter.location == ParamLoc::QueryString)
        .map(|parameter| {
            let ident = param_ident(parameter, crate::name::IdentRole::Field);
            let params_binding = bindings
                .params
                .as_ref()
                .expect("optional parameters argument allocated");
            let serialize = querystring_param_tokens(
                parameter,
                quote! { value },
                query_binding,
                raw_query_binding,
            );
            quote! {
                if let Some(value) = #params_binding
                    .as_ref()
                    .and_then(|params| params.#ident.as_ref())
                {
                    #serialize
                }
            }
        });
    let required_headers = operation
        .params
        .iter()
        .filter(|param| param.required && param.location == ParamLoc::Header)
        .map(|param| {
            let name = param.name.clone();
            let ident = param_ident(param, crate::name::IdentRole::Param);
            let value = param_value_tokens(param, quote! { &#ident });
            quote! { #request_binding = #request_binding.header(#name, #value); }
        });
    let optional_headers = operation
        .params
        .iter()
        .filter(|param| !param.required && param.location == ParamLoc::Header)
        .map(|param| {
            let name = param.name.clone();
            let ident = param_ident(param, crate::name::IdentRole::Field);
            let value = param_value_tokens(param, quote! { value });
            let params_binding = bindings
                .params
                .as_ref()
                .expect("optional parameters argument allocated");
            quote! {
                if let Some(value) = #params_binding
                    .as_ref()
                    .and_then(|params| params.#ident.as_ref())
                {
                    #request_binding = #request_binding.header(#name, #value);
                }
            }
        });
    let has_cookies = operation
        .params
        .iter()
        .any(|param| param.location == ParamLoc::Cookie);
    let cookie_init =
        has_cookies.then(|| quote! { let mut #cookies_binding: Vec<String> = Vec::new(); });
    let required_cookies = operation
        .params
        .iter()
        .filter(|param| param.required && param.location == ParamLoc::Cookie)
        .map(|param| {
            let name = param.name.clone();
            let ident = param_ident(param, crate::name::IdentRole::Param);
            cookie_param_tokens(param, &name, quote! { &#ident }, cookies_binding)
        });
    let optional_cookies = operation
        .params
        .iter()
        .filter(|param| !param.required && param.location == ParamLoc::Cookie)
        .map(|param| {
            let name = param.name.clone();
            let ident = param_ident(param, crate::name::IdentRole::Field);
            let params_binding = bindings
                .params
                .as_ref()
                .expect("optional parameters argument allocated");
            let serialize = cookie_param_tokens(param, &name, quote! { value }, cookies_binding);
            quote! {
                if let Some(value) = #params_binding
                    .as_ref()
                    .and_then(|params| params.#ident.as_ref())
                {
                    #serialize
                }
            }
        });
    let cookie_attach = has_cookies.then(|| {
        quote! {
            if !#cookies_binding.is_empty() {
                #request_binding = #request_binding.header(
                    reqwest::header::COOKIE,
                    #cookies_binding.join("; "),
                );
            }
        }
    });
    // A body the specification marks `required: false` arrives as `Option<&T>`, so the whole send
    // block runs only when the caller supplied one. Only a body that lowered to a type takes an
    // argument at all, so an untyped body is never wrapped.
    let optional_body_binding = operation
        .request_body
        .as_ref()
        .filter(|body| !body.required && body.ty.is_some())
        .and(bindings.body.as_ref());
    let body_send = if let Some((ty, media, encoding)) = operation
        .request_body
        .as_ref()
        .and_then(|body| body.ty.map(|ty| (ty, body.media, body.encoding.clone())))
    {
        let body_binding = bindings
            .body
            .as_ref()
            .expect("request body argument allocated");
        let content_type = &operation
            .request_body
            .as_ref()
            .expect("request body exists")
            .content_type;
        // A raw byte body (`bytes::Bytes`, from `format: binary` / `contentEncoding: base64`) is sent
        // as-is regardless of the declared media — `Bytes` is not `Display`, so it can never go
        // through `.to_string()`. This must be checked before the media match so a `text/plain` (or
        // any) media over a `Bytes` schema does not miscompile.
        if matches!(
            api.types.get(ty.id).map(|def| &def.kind),
            Some(TypeKind::Bytes)
        ) {
            if media == MediaType::MultipartRelated {
                // The required Content-Type operation parameter was attached above and includes
                // the boundary that frames these caller-preencoded bytes. Replacing it with the
                // bare media essence here would make the multipart message invalid.
                quote! {
                    #request_binding = #request_binding.body(#body_binding.clone());
                }
            } else {
                quote! {
                    #request_binding = #request_binding
                        .header(reqwest::header::CONTENT_TYPE, #content_type)
                        .body(#body_binding.clone());
                }
            }
        } else {
            match media {
                MediaType::Json => {
                    quote! { #request_binding = #request_binding.json(#body_binding); }
                }
                // XML: serialize the typed body to an XML string via the runtime's quick-xml helper
                // and set it as the body with the XML content-type. `to_xml` yields
                // `Error<Infallible>`, widened to the operation's error type.
                MediaType::Xml => quote! {
                    let #body_binding = support::to_xml(#body_binding)
                        .map_err(support::Error::widen)?;
                    #request_binding = #request_binding
                        .header(reqwest::header::CONTENT_TYPE, "application/xml")
                        .body(#body_binding);
                },
                MediaType::FormUrlEncoded => {
                    // Rendered property by property through the resolved Encoding Object rather
                    // than `RequestBuilder::form`, whose encoder rejects arrays and objects at
                    // runtime and cannot express a per-property content type.
                    let properties = form_properties_tokens(&encoding);
                    quote! {
                        const FORM_PROPERTIES: &[support::FormProperty] = &[#(#properties),*];
                        let #body_binding = support::serialize_form_body(
                            #body_binding,
                            FORM_PROPERTIES,
                        )
                        .map_err(support::Error::request_construction)?;
                        #request_binding = #request_binding
                            .header(
                                reqwest::header::CONTENT_TYPE,
                                "application/x-www-form-urlencoded",
                            )
                            .body(#body_binding);
                    }
                }
                MediaType::Text => quote! {
                    #request_binding = #request_binding
                        .header(reqwest::header::CONTENT_TYPE, #content_type)
                        .body(#body_binding.to_string());
                },
                MediaType::OctetStream => {
                    quote! { #request_binding = #request_binding.body(#body_binding.clone()); }
                }
                MediaType::Multipart => {
                    emit_multipart_body(ty, api, names, request_binding, body_binding, &encoding)
                }
                // Lowering requires multipart/related to be raw bytes, handled before this match.
                MediaType::MultipartRelated => quote! {},
                // Streaming media are response-only; a streaming request body is rejected during
                // lowering (narrowed `E009`), so this arm is unreachable for any emitted operation.
                MediaType::EventStream | MediaType::Ndjson | MediaType::JsonSequence => quote! {},
            }
        }
    } else {
        quote! {}
    };
    let body_send = match optional_body_binding {
        None => body_send,
        Some(body_binding) => quote! {
            if let Some(#body_binding) = #body_binding {
                #body_send
            }
        },
    };
    let attach_auth = if operation.security.is_empty() {
        quote! {}
    } else {
        let alternatives = operation.security.iter().map(|requirement| {
            let schemes = requirement.0.iter().map(|(id, _scopes)| {
                let scheme = api
                    .security_schemes
                    .get(id)
                    .expect("security scheme validated during lowering");
                let name = &id.0;
                let kind = match &scheme.kind {
                    // Caller-supplied oauth2/oidc tokens attach as bearer credentials.
                    SecurityScheme::Http(HttpScheme::Bearer)
                    | SecurityScheme::OAuth2
                    | SecurityScheme::OpenIdConnect => quote! { support::AuthKind::Bearer },
                    SecurityScheme::Http(HttpScheme::Basic) => quote! { support::AuthKind::Basic },
                    // Satisfied by the transport's client certificate; nothing to attach.
                    SecurityScheme::MutualTls => quote! { support::AuthKind::MutualTls },
                    SecurityScheme::ApiKey { location, name } => match location {
                        ApiKeyLoc::Header => quote! { support::AuthKind::ApiKeyHeader(#name) },
                        ApiKeyLoc::Query => quote! { support::AuthKind::ApiKeyQuery(#name) },
                        ApiKeyLoc::Cookie => quote! { support::AuthKind::ApiKeyCookie(#name) },
                    },
                };
                quote! { support::AuthScheme { name: #name, kind: #kind } }
            });
            quote! { &[#(#schemes),*][..] }
        });
        quote! {
            #request_binding = support::attach_auth(
                &self.core,
                #request_binding,
                &[#(#alternatives),*],
            )
                .await
                .map_err(support::Error::widen)?;
        }
    };
    let error_shape = operation.responses.error();
    let error_branch = match &error_shape {
        ErrorShape::None => quote! {
            Err(support::unexpected_status::<#error_ty>(&self.core, response).await)
        },
        // A single documented error body: classify against the documented status table into the
        // aliased `E` (or `Error::UnexpectedStatus` for an undocumented status).
        ErrorShape::Single(body_ty) => {
            let mut documented = operation
                .responses
                .by_status
                .iter()
                .filter(|(status, response)| !status.is_success() && response.body.is_some())
                .map(|(status, _)| match status {
                    crate::ir::StatusSpec::Exact(code) => {
                        quote! { support::StatusSpec::Exact(#code) }
                    }
                    crate::ir::StatusSpec::Range(prefix) => {
                        quote! { support::StatusSpec::Range(#prefix) }
                    }
                })
                .collect::<Vec<_>>();
            if operation
                .responses
                .default
                .as_ref()
                .is_some_and(|default| default.body.is_some())
            {
                documented.push(quote! { support::StatusSpec::Any });
            }
            let classify = if is_bytes_ty(api, *body_ty) {
                quote! {
                    support::classify_error_bytes(
                        &self.core,
                        response,
                        &[#(#documented),*],
                    )
                    .await
                }
            } else {
                match operation.responses.single_error_media() {
                    Some(MediaType::Xml) => quote! {
                        support::classify_error_xml::<#error_ty>(
                            &self.core,
                            response,
                            &[#(#documented),*],
                        )
                        .await
                    },
                    Some(MediaType::Text) => quote! {
                        support::classify_error_text::<#error_ty>(
                            &self.core,
                            response,
                            &[#(#documented),*],
                        )
                        .await
                    },
                    _ => quote! {
                        support::classify_error::<#error_ty>(
                            &self.core,
                            response,
                            &[#(#documented),*],
                        )
                        .await
                    },
                }
            };
            quote! {
                Err(#classify)
            }
        }
        // Multiple documented error bodies: read the capped body once, then dispatch by status in
        // precedence order (exact before range before default) into the matching enum variant →
        // `Error::Api`; a parse failure → `Error::Decode`; an undocumented status →
        // `Error::UnexpectedStatus` (capped body preserved either way).
        ErrorShape::Enum(entries) => {
            let arms = entries.iter().map(|(spec, ty)| {
                let spec_tokens = runtime_status_spec(*spec);
                let variant_ident = status_variant_ident(*spec);
                match ty {
                    // Bodied status: decode into the variant's type → `Api`, or `Decode` on failure.
                    Some(ty) => {
                        let body_ty = *ty;
                        let ty = response_payload_ty_tokens(body_ty, names, options, true);
                        let decode = if is_bytes_ty(api, body_ty) {
                            quote! { Ok::<#ty, String>(Box::new(body.clone())) }
                        } else if response_media_for_spec(&operation.responses, *spec)
                            == Some(MediaType::Text)
                        {
                            quote! { support::decode_text_body::<#ty>(&body) }
                        } else {
                            quote! {
                                serde_json::from_slice::<#ty>(&body)
                                    .map_err(|error| error.to_string())
                            }
                        };
                        quote! {
                            if #spec_tokens.matches(status) {
                                return Err(match #decode {
                                    Ok(value) => support::Error::Api(support::ResponseValue::new(
                                        status,
                                        headers,
                                        #error_ident::#variant_ident(value),
                                    )),
                                    Err(path) => support::Error::Decode {
                                        path,
                                        body,
                                        truncated,
                                    },
                                });
                            }
                        }
                    }
                    // Documented bodyless error status: the unit variant → `Api`, no body parse.
                    None => quote! {
                        if #spec_tokens.matches(status) {
                            return Err(support::Error::Api(support::ResponseValue::new(
                                status,
                                headers,
                                #error_ident::#variant_ident,
                            )));
                        }
                    },
                }
            });
            quote! {
                let (status, headers, body, truncated) =
                    match support::read_error_body::<#error_ty>(&self.core, response).await {
                        Ok(parts) => parts,
                        Err(error) => return Err(error),
                    };
                #(#arms)*
                Err(support::Error::UnexpectedStatus { status, headers, body })
            }
        }
    };
    let success_decode = match operation.responses.success() {
        SuccessShape::Unit => quote! {
            let status = response.status();
            let headers = response.headers().clone();
            Ok(support::ResponseValue::new(status, headers, ()))
        },
        // A single success body: decode into the aliased `T` through its selected wire codec.
        SuccessShape::Plain(body_ty) => {
            let decode = if is_bytes_ty(api, body_ty) {
                quote! { support::decode_success_bytes(&self.core, response) }
            } else {
                match operation.responses.single_success_media() {
                    Some(MediaType::Xml) => {
                        quote! { support::decode_success_xml::<#success_ty>(&self.core, response) }
                    }
                    Some(MediaType::Text) => {
                        quote! { support::decode_success_text::<#success_ty>(&self.core, response) }
                    }
                    _ => quote! { support::decode_success::<#success_ty>(&self.core, response) },
                }
            };
            quote! {
                #decode.await.map_err(support::Error::widen)
            }
        }
        // Multi-status success: read the body once, then dispatch by status in precedence order
        // (exact before range before default) into the matching variant. A success status matching
        // no documented variant is an unexpected-status error — there is no untyped fallback.
        SuccessShape::Enum(entries) => {
            let method_ident = names
                .operations
                .get(&operation.id)
                .expect("operation name allocated");
            let enum_ident = success_enum_ident(method_ident);
            let arms = entries.iter().map(|(spec, ty)| {
                let spec_tokens = runtime_status_spec(*spec);
                let variant_ident = status_variant_ident(*spec);
                match ty {
                    // Bodied status: parse the read body into the variant's type.
                    Some(ty) => {
                        let body_ty = *ty;
                        let ty = response_payload_ty_tokens(body_ty, names, options, true);
                        let decode = if is_bytes_ty(api, body_ty) {
                            quote! { Ok::<#ty, String>(Box::new(body.clone())) }
                        } else if response_media_for_spec(&operation.responses, *spec)
                            == Some(MediaType::Text)
                        {
                            quote! { support::decode_text_body::<#ty>(&body) }
                        } else {
                            quote! {
                                serde_json::from_slice::<#ty>(&body)
                                    .map_err(|error| error.to_string())
                            }
                        };
                        quote! {
                            if #spec_tokens.matches(status) {
                                let value = #decode
                                    .map_err(|path| support::Error::<#error_ty>::Decode {
                                        path,
                                        body: body.clone(),
                                        truncated: false,
                                    })?;
                                return Ok(support::ResponseValue::new(
                                    status,
                                    headers,
                                    #enum_ident::#variant_ident(value),
                                ));
                            }
                        }
                    }
                    // Documented bodyless status (e.g. `204`): the unit variant, no body parse.
                    None => quote! {
                        if #spec_tokens.matches(status) {
                            return Ok(support::ResponseValue::new(
                                status,
                                headers,
                                #enum_ident::#variant_ident,
                            ));
                        }
                    },
                }
            });
            quote! {
                let (status, headers, body) = support::read_success_body(response)
                    .await
                    .map_err(support::Error::widen)?;
                #(#arms)*
                Err(support::Error::<#error_ty>::UnexpectedStatus { status, headers, body })
            }
        }
    };

    // A streaming success response (`text/event-stream` / `application/x-ndjson`) returns an
    // `EventStream<T>` instead of a `ResponseValue<T>`: on success the whole `response` is handed
    // to the stream with its framing mode, and items are decoded lazily as the caller pulls them.
    // The error path is unchanged (streaming error bodies are out of scope). `success_ty` is the
    // streamed item type `T` — `stream_success` fires only in the single-success-body case, where
    // `success()` is `Plain(T)` and `success_type` renders `T`.
    let stream_framing = operation
        .responses
        .stream_success()
        .map(|(framing, _)| framing);
    let success_decode = match stream_framing {
        Some(framing) => {
            let framing_tokens = match framing {
                crate::ir::Framing::Sse => quote! { support::Framing::Sse },
                crate::ir::Framing::SseEvent => quote! { support::Framing::SseEvent },
                crate::ir::Framing::SseJsonData => quote! { support::Framing::SseJsonData },
                crate::ir::Framing::Ndjson => quote! { support::Framing::Ndjson },
                crate::ir::Framing::JsonSequence => quote! { support::Framing::JsonSequence },
            };
            quote! {
                Ok(support::EventStream::new_reconnectable(
                    response,
                    #framing_tokens,
                    self.core.clone(),
                    #reconnect_request_binding,
                ))
            }
        }
        None => success_decode,
    };
    let reconnect_request_init = stream_framing.map(|_| {
        quote! {
            let #reconnect_request_binding = #request_binding.try_clone();
        }
    });
    // The return type is shared with the blocking shim so both surfaces stay identical.
    let (return_ok_ty, _) = operation_return_ty(operation, names, options);

    // An Operation or Path Item Object may override the document's `servers`. The runtime's
    // `*_on` entry points take that override: absolute, it replaces the client's base URL;
    // relative, it is joined onto it. `None` keeps the client's base.
    let server_override = match &operation.server {
        Some(server) => quote! { Some(#server) },
        None => quote! { None },
    };
    let build_url = if uses_querystring {
        quote! {
            support::build_url_with_query_string_on(
                &self.core,
                #server_override,
                &#path_binding,
                &#query_binding,
                #raw_query_binding.as_deref(),
            )
        }
    } else {
        quote! {
            support::build_url_on(&self.core, #server_override, &#path_binding, &#query_binding)
        }
    };

    quote! {
        #docs
        #(#param_default_docs)*
        #deprecated
        #[inline]
        pub async fn #method_ident(
            &self,
            #(#args),*
        ) -> Result<#return_ok_ty, support::Error<#error_ty>> {
            #(#const_checks)*
            let mut #path_binding = #path_init.to_owned();
            #(#path_replacements)*
            let mut #query_binding: Vec<String> = Vec::new();
            #raw_query_init
            #(#required_query)*
            #(#optional_query)*
            #(#required_querystring)*
            #(#optional_querystring)*
            let #url_binding = #build_url
                .map_err(support::Error::widen)?;
            let mut #request_binding = self.core.http().request(#reqwest_method, #url_binding);
            #(#required_headers)*
            #(#optional_headers)*
            #cookie_init
            #(#required_cookies)*
            #(#optional_cookies)*
            #cookie_attach
            #body_send
            #attach_auth
            let #request_binding = #request_binding
                .build()
                .map_err(support::Error::request_construction)?;
            #reconnect_request_init
            let response = support::send(&self.core, #request_binding)
                .await
                .map_err(support::Error::widen)?;
            if response.status().is_success() {
                #success_decode
            } else {
                #error_branch
            }
        }
    }
}

/// The typed method arguments and their bare forwarding names for an operation. Shared by
/// [`emit_operation`] (the async method) and [`emit_blocking_operation`] (its synchronous shim) so
/// the two signatures are constructed from one source and can never drift. The first vector holds
/// `name: Type` argument declarations; the second holds just the `name`s, in the same order, for the
/// shim's `self.inner.<op>(<names>)` forwarding call. Body args are already `&T`, so the forwarding
/// name (`body`) passes the reference straight through.
fn operation_args(
    operation: &Operation,
    names: &Names,
    options: &CodegenOptions,
) -> (Vec<TokenStream>, Vec<TokenStream>) {
    let bindings = operation_bindings(operation, names);
    let params_ident = names
        .params_structs
        .get(&operation.id)
        .expect("params name allocated");
    let mut args = Vec::new();
    let mut forwards = Vec::new();
    for param in operation.params.iter().filter(|param| param.required) {
        let ident = param_ident(param, crate::name::IdentRole::Param);
        let ty = ty_tokens(param.ty, names, options, true);
        args.push(quote! { #ident: #ty });
        forwards.push(quote! { #ident });
    }
    if let Some(params_binding) = &bindings.params {
        args.push(quote! { #params_binding: Option<#params_ident> });
        forwards.push(quote! { #params_binding });
    }
    if let Some((ty, required)) = operation.request_body.as_ref().and_then(|body| {
        body.ty
            .map(|ty| (ty_tokens(ty, names, options, true), body.required))
    }) {
        let body_binding = bindings
            .body
            .as_ref()
            .expect("request body argument allocated");
        // A body the specification marks `required: false` may legitimately be omitted, so the
        // caller says so in the type rather than inventing an empty value.
        if required {
            args.push(quote! { #body_binding: &#ty });
        } else {
            args.push(quote! { #body_binding: Option<&#ty> });
        }
        forwards.push(quote! { #body_binding });
    }
    (args, forwards)
}

/// The `Ok`/`Err` types of an operation's `Result` return: `(return_ok_ty, error_ty)`. A streaming
/// success yields `EventStream<T>`, every other success `ResponseValue<T>`. Shared with the blocking
/// shim so the async and sync return types stay identical.
fn operation_return_ty(
    operation: &Operation,
    names: &Names,
    options: &CodegenOptions,
) -> (TokenStream, TokenStream) {
    let method_ident = names
        .operations
        .get(&operation.id)
        .expect("operation name allocated");
    let error_ident = format_ident!("{}Error", to_pascal(method_ident.as_str()));
    let success_ty = success_type(operation, names, options);
    let return_ok_ty = match operation.responses.stream_success() {
        Some(_) => quote! { support::EventStream<#success_ty> },
        None => quote! { support::ResponseValue<#success_ty> },
    };
    (return_ok_ty, quote! { #error_ident })
}

/// The rustdoc `#[doc = …]` notes carrying required parameters' spec `default`s. Required params are
/// positional arguments with no attribute slot of their own, so their defaults are documented on the
/// method. Shared verbatim between the async method and its blocking shim.
fn param_default_docs_tokens(operation: &Operation) -> Vec<TokenStream> {
    operation
        .params
        .iter()
        .filter(|param| param.required)
        .filter_map(|param| {
            param.default_display.as_ref().map(|default| {
                let note =
                    normalize_rustdoc(&format!("Parameter `{}` default: `{default}`.", param.name));
                quote! { #[doc = #note] }
            })
        })
        .collect()
}

/// Emit the `BlockingClient`: a synchronous facade, gated on the generated crate's `blocking`
/// feature, that owns the async [`Client`] plus a current-thread tokio runtime and drives each async
/// operation to completion with `block_on`. It reuses the whole async dispatch — every method is a
/// thin shim — so there is zero logic duplication. Constructors mirror the async client's.
///
/// A `BlockingClient` must not be built or used from inside another async runtime (tokio's
/// `block_on` panics when nested); the constructor building its own current-thread runtime is the
/// standard shape for a non-async caller.
pub(crate) fn emit_blocking_client(
    api: &Api,
    names: &Names,
    options: &CodegenOptions,
) -> TokenStream {
    let methods = api
        .operations
        .iter()
        .map(|operation| emit_blocking_operation(operation, names, options));
    let doc = "A synchronous client: owns the async `Client` plus a current-thread tokio runtime \
        and `block_on`s each operation. Enable the crate's `blocking` feature to use it.\n\n\
        Must NOT be constructed or called from inside another async runtime — tokio's `block_on` \
        panics when nested. Build one on a plain thread (e.g. `std::thread` or \
        `tokio::task::spawn_blocking`).";
    quote! {
        // In module/include!/macro output, `feature = "blocking"` resolves against the consumer
        // crate. Keep the cfgs inside an ungated lexical lint scope so crates that do not declare
        // that optional feature stay warning-free under `unexpected_cfgs` (including `-D warnings`).
        #[allow(unexpected_cfgs, unused_imports)]
        mod __spargen_blocking {
            use super::*;

            #[cfg(all(feature = "blocking", not(target_arch = "wasm32")))]
            #[doc = #doc]
            #[allow(dead_code)]
            pub struct BlockingClient {
                inner: Client,
                runtime: support::BlockingRuntime,
            }

            #[cfg(all(feature = "blocking", not(target_arch = "wasm32")))]
            #[forbid(unsafe_code)]
            #[allow(dead_code, unused_mut, unused_variables, clippy::result_large_err, clippy::deref_addrof, clippy::needless_borrow, clippy::needless_borrows_for_generic_args, clippy::redundant_closure_call, clippy::get_first, clippy::explicit_auto_deref)]
            impl BlockingClient {
            /// Build a blocking client over a fresh default `reqwest::Client`.
            pub fn new(base_url: &str) -> Result<Self, support::Error<std::convert::Infallible>> {
                let inner = Client::new(base_url)?;
                let runtime = support::BlockingRuntime::new()
                    .map_err(support::Error::request_construction)?;
                Ok(Self { inner, runtime })
            }

            /// Build a blocking client over a caller-supplied `reqwest::Client`.
            pub fn with_client(
                client: reqwest::Client,
                base_url: &str,
            ) -> Result<Self, support::Error<std::convert::Infallible>> {
                let inner = Client::with_client(client, base_url)?;
                let runtime = support::BlockingRuntime::new()
                    .map_err(support::Error::request_construction)?;
                Ok(Self { inner, runtime })
            }

            /// Build a blocking client over a caller-supplied transport backend.
            pub fn with_backend(
                backend: std::sync::Arc<dyn support::HttpBackend>,
                base_url: &str,
            ) -> Result<Self, support::Error<std::convert::Infallible>> {
                let inner = Client::with_backend(backend, base_url)?;
                let runtime = support::BlockingRuntime::new()
                    .map_err(support::Error::request_construction)?;
                Ok(Self { inner, runtime })
            }

            /// Borrow the wrapped async client.
            pub fn inner(&self) -> &Client {
                &self.inner
            }

            /// Borrow the client's shared core (base URL, credentials, transport).
            pub fn core(&self) -> &support::ClientCore {
                self.inner.core()
            }

            /// Register a credential for a named security scheme (mirrors the async client).
            #[must_use]
            pub fn with_credential(
                mut self,
                scheme: &str,
                credential: support::Credential,
            ) -> Self {
                self.inner = self.inner.with_credential(scheme, credential);
                self
            }

                #(#methods)*
            }
        }

        #[allow(unused_imports)]
        pub use __spargen_blocking::*;
    }
}

/// Emit one blocking operation method: the async method's signature minus `async`, whose body drives
/// the async method to completion on the owned runtime. Same docs, deprecation, argument list, and
/// return types as the async method (all built from the shared signature helpers).
fn emit_blocking_operation(
    operation: &Operation,
    names: &Names,
    options: &CodegenOptions,
) -> TokenStream {
    let method_ident = names
        .operations
        .get(&operation.id)
        .expect("operation name allocated");
    let docs = doc_tokens(&operation.docs);
    let param_default_docs = param_default_docs_tokens(operation);
    let deprecated = operation.deprecated.then(|| quote! { #[deprecated] });
    let (args, forwards) = operation_args(operation, names, options);
    let (return_ok_ty, error_ty) = operation_return_ty(operation, names, options);
    quote! {
        #docs
        #(#param_default_docs)*
        #deprecated
        // A deprecated blocking wrapper intentionally forwards to its deprecated async twin. This
        // suppresses only that internal call; `#deprecated` still warns consumers of this method.
        #[allow(deprecated)]
        #[inline]
        pub fn #method_ident(
            &self,
            #(#args),*
        ) -> Result<#return_ok_ty, support::Error<#error_ty>> {
            self.runtime.block_on(self.inner.#method_ident(#(#forwards),*))
        }
    }
}

/// Emit the `body_send` for a `multipart/form-data` request body: build a `reqwest::multipart::Form`
/// from the typed body struct, one part per field in declaration order (part order is deterministic).
/// A binary field (`bytes::Bytes`) becomes a file/bytes part; a scalar becomes a text part via
/// `Display`; an object/array/union becomes a JSON-encoded text part. Optional fields (`Option<T>`)
/// only add their part when `Some`. Lowering guarantees a multipart body is an object schema, so a
/// non-struct body cannot reach here (it is rejected as `E009`); the fallback stays a no-op.
/// Render one resolved [`BodyEncoding`] as `support::FormProperty` const entries.
fn form_properties_tokens(encoding: &crate::ir::BodyEncoding) -> Vec<TokenStream> {
    encoding
        .properties
        .iter()
        .map(|property| {
            let name = property.name.clone();
            let mode = form_mode_tokens(&property.mode);
            quote! { support::FormProperty { name: #name, mode: #mode } }
        })
        .collect()
}

/// Render one property's encoding mode.
fn form_mode_tokens(mode: &crate::ir::EncodingMode) -> TokenStream {
    match mode {
        crate::ir::EncodingMode::Media { codec, .. } => match codec {
            MediaType::Json => quote! { support::FormMode::Json },
            _ => quote! { support::FormMode::Text },
        },
        crate::ir::EncodingMode::Style {
            style,
            explode,
            allow_reserved,
        } => {
            let style = match style {
                crate::ir::ParamStyle::Delimited(delimiter) => {
                    let delimiter = delimiter_tokens(*delimiter);
                    quote! { support::FormStyle::Delimited(#delimiter) }
                }
                crate::ir::ParamStyle::DeepObject => quote! { support::FormStyle::DeepObject },
                _ => quote! { support::FormStyle::Form },
            };
            let encoding = if *allow_reserved {
                quote! { support::PercentEncoding::Reserved }
            } else {
                quote! { support::PercentEncoding::Form }
            };
            quote! {
                support::FormMode::Style {
                    style: #style,
                    explode: #explode,
                    encoding: #encoding,
                }
            }
        }
    }
}

/// Emit one typed header struct per documented status that declares response headers.
///
/// Reading headers is an explicitly-called second step rather than part of the return type: the
/// body has already been decoded and handed back before a caller opts in, so a malformed or absent
/// header can never turn a successful call into a failed one. Every header also stays reachable
/// raw through `ResponseValue::headers`.
fn emit_response_headers(
    operation: &Operation,
    api: &Api,
    names: &Names,
    options: &CodegenOptions,
) -> TokenStream {
    let _ = api;
    let responses = operation
        .responses
        .by_status
        .iter()
        .map(|(spec, response)| (crate::name::status_label(Some(*spec)), response))
        .chain(
            operation
                .responses
                .default
                .as_ref()
                .map(|response| (crate::name::status_label(None), response)),
        );
    let structs = responses.filter_map(|(label, response)| {
        if response.headers.is_empty() {
            return None;
        }
        let ident = names
            .response_header_structs
            .get(&(operation.id.clone(), label.clone()))?;
        let ident = proc_macro2::Ident::new(ident.as_str(), proc_macro2::Span::call_site());
        let fields = response.headers.iter().map(|header| {
            let field = names
                .response_header_fields
                .get(&(operation.id.clone(), label.clone(), header.name.clone()))
                .expect("response header field allocated");
            let field = proc_macro2::Ident::new(field.as_str(), proc_macro2::Span::call_site());
            // Header structs live beside `Client`, not inside `types`, so the type path must be
            // qualified — a header whose schema lowered to a named type would not resolve here.
            let ty = ty_tokens(header.ty, names, options, true);
            // An optional header is absent-able; a required one is documented as always present.
            let ty = if header.required {
                quote! { #ty }
            } else {
                quote! { Option<#ty> }
            };
            let docs = doc_tokens(&header.docs);
            let deprecated = header.deprecated.then(|| quote! { #[deprecated] });
            quote! { #docs #deprecated pub #field: #ty }
        });
        let reads = response.headers.iter().map(|header| {
            let field = names
                .response_header_fields
                .get(&(operation.id.clone(), label.clone(), header.name.clone()))
                .expect("response header field allocated");
            let field = proc_macro2::Ident::new(field.as_str(), proc_macro2::Span::call_site());
            let name = header.name.clone();
            let shape = match header.shape {
                crate::ir::HeaderShape::Scalar => quote! { support::HeaderShape::Scalar },
                crate::ir::HeaderShape::Array => quote! { support::HeaderShape::Array },
                crate::ir::HeaderShape::Object => quote! { support::HeaderShape::Object },
                crate::ir::HeaderShape::Json => quote! { support::HeaderShape::Json },
                crate::ir::HeaderShape::SetCookie => quote! { support::HeaderShape::SetCookie },
            };
            let explode = header.explode;
            let call = if header.required {
                quote! { support::require_header(headers, #name, #shape, #explode)? }
            } else {
                quote! { support::parse_header(headers, #name, #shape, #explode)? }
            };
            quote! { #field: #call }
        });
        let doc = format!(
            "Documented response headers for `{}` `{label}`.",
            operation.id.0
        );
        Some(quote! {
            #[doc = #doc]
            #[allow(dead_code)]
            #[derive(Debug, Clone)]
            pub struct #ident {
                #(#fields),*
            }

            #[allow(dead_code, deprecated, clippy::deref_addrof, clippy::needless_borrow, clippy::needless_borrows_for_generic_args, clippy::redundant_closure_call, clippy::get_first, clippy::explicit_auto_deref)]
            impl #ident {
                /// Read the documented headers out of a raw header map.
                pub fn from_headers(
                    headers: &reqwest::header::HeaderMap,
                ) -> Result<Self, support::HeaderError> {
                    Ok(Self { #(#reads),* })
                }

                /// Read the documented headers out of a returned response value.
                pub fn from_response<T>(
                    response: &support::ResponseValue<T>,
                ) -> Result<Self, support::HeaderError> {
                    Self::from_headers(response.headers())
                }
            }
        })
    });
    quote! { #(#structs)* }
}

/// Emit the `servers` module: one builder per declared server, plus the default base URL.
///
/// A Server Variable `default` is sent when the caller supplies no alternative, so every server
/// resolves to a concrete URL with no arguments; a variable that declares an `enum` gets a typed
/// enum so an illegal value cannot be constructed at all.
fn emit_servers(api: &Api, names: &Names) -> TokenStream {
    if api.servers.is_empty() {
        return quote! {};
    }
    // One fixed, unnamed server has nothing to configure, so a builder type would only add a
    // positional name. `default_url` is the whole surface.
    if let [server] = api.servers.as_slice() {
        if server.name.is_none() && server.variables.is_empty() {
            let url = &server.url;
            let mut doc = format!("The declared server, `{url}`.");
            if let Some(description) = &server.description {
                doc.push_str("\n\n");
                doc.push_str(description);
            }
            let doc = normalize_rustdoc(&doc);
            return quote! {
                /// Base URLs declared by the API description.
                #[allow(dead_code)]
                pub mod servers {
                    #[doc = #doc]
                    pub fn default_url() -> String {
                        #url.to_owned()
                    }
                }
            };
        }
    }
    let builders = api.servers.iter().enumerate().map(|(index, server)| {
        let ident = names
            .servers
            .get(index)
            .expect("server name allocated")
            .clone();
        let ident = proc_macro2::Ident::new(ident.as_str(), proc_macro2::Span::call_site());
        let mut docs = vec![format!("Server `{}`.", server.url)];
        if let Some(description) = &server.description {
            docs.push(description.clone());
        }
        let doc_attrs = docs.iter().map(|line| quote! { #[doc = #line] });

        let variable_enums = server.variables.iter().filter_map(|(name, variable)| {
            let enum_ident = names.server_variable_enums.get(&(index, name.clone()))?;
            let enum_ident =
                proc_macro2::Ident::new(enum_ident.as_str(), proc_macro2::Span::call_site());
            let variants = variable.enum_values.iter().map(|value| {
                let variant = names
                    .server_variable_variants
                    .get(&(index, name.clone(), value.clone()))
                    .expect("server variable variant allocated");
                let variant =
                    proc_macro2::Ident::new(variant.as_str(), proc_macro2::Span::call_site());
                let default = (value == &variable.default).then(|| quote! { #[default] });
                quote! { #default #variant }
            });
            let arms = variable.enum_values.iter().map(|value| {
                let variant = names
                    .server_variable_variants
                    .get(&(index, name.clone(), value.clone()))
                    .expect("server variable variant allocated");
                let variant =
                    proc_macro2::Ident::new(variant.as_str(), proc_macro2::Span::call_site());
                quote! { Self::#variant => #value }
            });
            let doc = format!("Permitted values of the `{name}` server variable.");
            Some(quote! {
                #[doc = #doc]
                #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
                pub enum #enum_ident {
                    #(#variants),*
                }

                impl #enum_ident {
                    /// The value substituted into the server URL.
                    pub fn as_str(self) -> &'static str {
                        match self {
                            #(#arms),*
                        }
                    }
                }
            })
        });

        let fields = server.variables.keys().map(|name| {
            let field = names
                .server_variable_fields
                .get(&(index, name.clone()))
                .expect("server variable field allocated");
            let field = proc_macro2::Ident::new(field.as_str(), proc_macro2::Span::call_site());
            match names.server_variable_enums.get(&(index, name.clone())) {
                Some(enum_ident) => {
                    let enum_ident = proc_macro2::Ident::new(
                        enum_ident.as_str(),
                        proc_macro2::Span::call_site(),
                    );
                    quote! { #field: #enum_ident }
                }
                None => quote! { #field: String },
            }
        });

        let defaults = server.variables.iter().map(|(name, variable)| {
            let field = names
                .server_variable_fields
                .get(&(index, name.clone()))
                .expect("server variable field allocated");
            let field = proc_macro2::Ident::new(field.as_str(), proc_macro2::Span::call_site());
            match names.server_variable_enums.get(&(index, name.clone())) {
                // The `#[default]` variant is the declared default, so `Default` is exact.
                Some(enum_ident) => {
                    let enum_ident = proc_macro2::Ident::new(
                        enum_ident.as_str(),
                        proc_macro2::Span::call_site(),
                    );
                    quote! { #field: <#enum_ident as Default>::default() }
                }
                None => {
                    let default = variable.default.clone();
                    quote! { #field: #default.to_owned() }
                }
            }
        });

        let setters = server.variables.iter().map(|(name, variable)| {
            let field = names
                .server_variable_fields
                .get(&(index, name.clone()))
                .expect("server variable field allocated");
            let field = proc_macro2::Ident::new(field.as_str(), proc_macro2::Span::call_site());
            let mut doc = format!("Set the `{name}` server variable.");
            if let Some(description) = &variable.description {
                doc.push(' ');
                doc.push_str(description);
            }
            match names.server_variable_enums.get(&(index, name.clone())) {
                Some(enum_ident) => {
                    let enum_ident = proc_macro2::Ident::new(
                        enum_ident.as_str(),
                        proc_macro2::Span::call_site(),
                    );
                    quote! {
                        #[doc = #doc]
                        #[must_use]
                        pub fn #field(mut self, value: #enum_ident) -> Self {
                            self.#field = value;
                            self
                        }
                    }
                }
                None => quote! {
                    #[doc = #doc]
                    #[must_use]
                    pub fn #field(mut self, value: impl Into<String>) -> Self {
                        self.#field = value.into();
                        self
                    }
                },
            }
        });

        let pieces = server.segments.iter().map(|segment| match segment {
            // A one-character literal goes through `push`: `push_str` with a single-char literal
            // trips `clippy::single_char_add_str`, and generated code must pass `-D warnings` in
            // the consuming crate.
            crate::ir::UrlSegment::Literal(text) => match text.chars().count() {
                1 => {
                    let character = text.chars().next().expect("one character");
                    quote! { url.push(#character); }
                }
                _ => quote! { url.push_str(#text); },
            },
            crate::ir::UrlSegment::Variable(name) => {
                let field = names
                    .server_variable_fields
                    .get(&(index, name.clone()))
                    .expect("server variable field allocated");
                let field = proc_macro2::Ident::new(field.as_str(), proc_macro2::Span::call_site());
                match names.server_variable_enums.get(&(index, name.clone())) {
                    Some(_) => quote! { url.push_str(self.#field.as_str()); },
                    None => quote! { url.push_str(&self.#field); },
                }
            }
        });

        // `Default` is derivable exactly when every field's declared default is what the derive
        // would produce: an enum variable pins its default with `#[default]`, and a server with no
        // variables has nothing to default. A free-form variable carries a spec-declared string,
        // which the derive would replace with `""`.
        let derivable_default = server.variables.iter().all(|(name, _)| {
            names
                .server_variable_enums
                .contains_key(&(index, name.clone()))
        });
        let (derive_default, default_impl) = if derivable_default {
            (quote! { , Default }, quote! {})
        } else {
            (
                quote! {},
                quote! {
                    impl Default for #ident {
                        fn default() -> Self {
                            Self { #(#defaults),* }
                        }
                    }
                },
            )
        };
        quote! {
            #(#variable_enums)*

            #(#doc_attrs)*
            #[derive(Debug, Clone #derive_default)]
            pub struct #ident {
                #(#fields),*
            }

            #default_impl

            impl #ident {
                /// Every server variable at its declared default.
                pub fn new() -> Self {
                    Self::default()
                }

                #(#setters)*

                /// The server URL with every variable substituted.
                pub fn url(&self) -> String {
                    let mut url = String::new();
                    #(#pieces)*
                    url
                }
            }
        }
    });
    let first = names.servers.first().expect("at least one server").clone();
    let first = proc_macro2::Ident::new(first.as_str(), proc_macro2::Span::call_site());
    quote! {
        /// Base URLs declared by the API description.
        #[allow(dead_code)]
        pub mod servers {
            #(#builders)*

            /// The first declared server, with every server variable at its declared default.
            pub fn default_url() -> String {
                #first::new().url()
            }
        }
    }
}

/// Emit a `multipart/form-data` body.
///
/// Every part carries the Content-Type its Encoding Object resolves to — explicit, or defaulted
/// from the property's type by the specification's table — and any header the Encoding Object pins
/// to a literal value. How the value is turned into bytes follows the property's own lowered type,
/// which is the only thing the generator can know for a media type it has no codec for.
fn emit_multipart_body(
    ty: Ty,
    api: &Api,
    names: &Names,
    request_binding: &crate::name::Ident,
    body_binding: &crate::name::Ident,
    encoding: &crate::ir::BodyEncoding,
) -> TokenStream {
    let Some(TypeKind::Struct(object)) = api.types.get(ty.id).map(|def| &def.kind) else {
        return quote! {};
    };
    let parts = object.fields.iter().map(|field| {
        let wire = &field.name.wire;
        let field_ident = names
            .fields
            .get(&(ty.id, field.name.wire.clone()))
            .expect("multipart body field name allocated");
        // Request fields have separate presence and nullability wrappers, mirroring `emit_field`.
        let optional = !field.required || field.ty.nullable;
        let kind = api.types.get(field.ty.id).map(|def| &def.kind);
        let property = encoding
            .properties
            .iter()
            .find(|property| property.name == field.name.wire);
        let headers = property
            .map(|property| property.headers.as_slice())
            .unwrap_or(&[]);
        let extra_headers = (!headers.is_empty()).then(|| {
            let inserts = headers.iter().map(|(name, value)| {
                quote! {
                    if let (Ok(name), Ok(value)) = (
                        reqwest::header::HeaderName::try_from(#name),
                        reqwest::header::HeaderValue::try_from(#value),
                    ) {
                        part_headers.insert(name, value);
                    }
                }
            });
            quote! {
                let mut part_headers = reqwest::header::HeaderMap::new();
                #(#inserts)*
                part = part.headers(part_headers);
            }
        });
        // `receiver` is the value for method calls (`.to_vec()`/`.to_string()` auto-ref); `reference`
        // is an explicit `&value` for `serde_json::to_string`, which takes `&T`. Splitting them keeps
        // the emitted code free of `clippy::needless_borrow` on the method-call receivers.
        let add_part = |receiver: &TokenStream, reference: &TokenStream| {
            let build = match &property.map(|property| &property.mode) {
                // RFC 6570 mode: the part value is style-serialized, and an array becomes one part
                // per item under the same name. `contentType` is inert in this mode.
                Some(crate::ir::EncodingMode::Style {
                    style,
                    explode,
                    ..
                }) => {
                    let style = match style {
                        crate::ir::ParamStyle::Delimited(delimiter) => {
                            let delimiter = delimiter_tokens(*delimiter);
                            quote! { support::FormStyle::Delimited(#delimiter) }
                        }
                        crate::ir::ParamStyle::DeepObject => {
                            quote! { support::FormStyle::DeepObject }
                        }
                        _ => quote! { support::FormStyle::Form },
                    };
                    return quote! {
                        for value in support::serialize_multipart_values(#reference, #style, #explode)
                            .map_err(support::Error::request_construction)?
                        {
                            form = form.part(#wire, reqwest::multipart::Part::text(value));
                        }
                    };
                }
                _ => match kind {
                    // A binary/bytes property → a file/bytes part carrying the raw bytes.
                    Some(TypeKind::Bytes) => quote! {
                        let mut part = reqwest::multipart::Part::bytes(#receiver.to_vec());
                    },
                    // A scalar property → a text part rendered through `Display`.
                    Some(TypeKind::Primitive(_) | TypeKind::Enum(_)) => quote! {
                        let mut part = reqwest::multipart::Part::text(#receiver.to_string());
                    },
                    // Any composite property → a JSON-encoded text part.
                    _ => quote! {
                        let mut part = reqwest::multipart::Part::text(
                            serde_json::to_string(#reference)
                                .map_err(support::Error::request_construction)?,
                        );
                    },
                },
            };
            let content_type = match property.map(|property| &property.mode) {
                Some(crate::ir::EncodingMode::Media { content_type, .. }) => {
                    Some(content_type.clone())
                }
                _ => None,
            };
            // `mime_str` consumes the part, so a malformed content type from the spec surfaces as
            // a request-construction error rather than being silently dropped.
            let set_type = content_type.map(|content_type| {
                quote! {
                    part = part
                        .mime_str(#content_type)
                        .map_err(support::Error::request_construction)?;
                }
            });
            quote! {
                #build
                #set_type
                #extra_headers
                form = form.part(#wire, part);
            }
        };
        if optional {
            // Multipart has no generic JSON-null part representation. Keep its existing behavior
            // of omitting null parts, while accepting the request model's separate presence layer.
            let value = if !field.required && field.ty.nullable {
                quote! { #body_binding.#field_ident.as_ref().and_then(Option::as_ref) }
            } else {
                quote! { #body_binding.#field_ident.as_ref() }
            };
            let stmt = add_part(&quote! { value }, &quote! { value });
            quote! {
                if let Some(value) = #value {
                    #stmt
                }
            }
        } else {
            add_part(
                &quote! { #body_binding.#field_ident },
                &quote! { &#body_binding.#field_ident },
            )
        }
    });
    quote! {
        let mut form = reqwest::multipart::Form::new();
        #(#parts)*
        #request_binding = #request_binding.multipart(form);
    }
}

fn operation_bindings<'a>(operation: &Operation, names: &'a Names) -> &'a OperationBindings {
    names
        .operation_bindings
        .get(&operation.id)
        .expect("operation bindings allocated")
}

fn param_ident(param: &crate::ir::Parameter, role: crate::name::IdentRole) -> proc_macro2::Ident {
    escaped_token(&param.name, role)
}

/// Build the `proc_macro2::Ident` for an escaped name, PRESERVING raw escaping: a keyword like
/// `type` escapes to `r#type`, which must become a raw identifier token (`Ident::new_raw`) — NOT a
/// bare `type` (an invalid keyword token that fails to parse). This is the token equivalent of the
/// name subsystem's `Ident` `ToTokens`; use it wherever an escaped param/field name is turned into
/// a `proc_macro2::Ident` directly instead of going through a `name::Ident`.
fn escaped_token(name: &str, role: crate::name::IdentRole) -> proc_macro2::Ident {
    let escaped = crate::name::escape(name, role);
    let span = proc_macro2::Span::call_site();
    match escaped.as_str().strip_prefix("r#") {
        Some(raw) => proc_macro2::Ident::new_raw(raw, span),
        None => proc_macro2::Ident::new(escaped.as_str(), span),
    }
}

/// The percent-encoding set for one parameter, derived from its location, style, and
/// `allowReserved`.
///
/// This is the single place that mapping exists. Headers and OpenAPI 3.2's `style: cookie` are
/// sent verbatim — the specification says values there must not be percent-encoded, and that
/// escaping is the caller's job. `in: cookie` with `style: form` *does* encode: Appendix D
/// describes that pairing as the way to opt into automatic encoding, with `allowReserved` as its
/// escape hatch.
fn percent_encoding_tokens(param: &crate::ir::Parameter) -> TokenStream {
    let variant = if param.location == ParamLoc::Header
        || matches!(param.style, crate::ir::ParamStyle::Cookie)
    {
        "Passthrough"
    } else if param.location == ParamLoc::Path {
        if param.allow_reserved {
            "ReservedPath"
        } else {
            "Unreserved"
        }
    } else if param.allow_reserved {
        "Reserved"
    } else {
        "Form"
    };
    let ident = proc_macro2::Ident::new(variant, proc_macro2::Span::call_site());
    quote! { support::PercentEncoding::#ident }
}

/// The runtime `Delimiter` for a non-RFC 6570 query style.
fn delimiter_tokens(delimiter: crate::ir::Delimiter) -> TokenStream {
    match delimiter {
        crate::ir::Delimiter::Space => quote! { support::Delimiter::Space },
        crate::ir::Delimiter::Pipe => quote! { support::Delimiter::Pipe },
    }
}

/// Render a path/header parameter value from a borrowed expression. Schema-typed parameters use
/// their declared OpenAPI style; `content`-typed parameters retain their media codec.
///
/// Path values are percent-encoded here, at serialization time. Splicing a raw value into the
/// path template would let a value containing `/`, `?`, or `#` silently re-target the request.
fn param_value_tokens(param: &crate::ir::Parameter, value: TokenStream) -> TokenStream {
    if let crate::ir::ParamStyle::Content(media) = &param.style {
        return match media {
            MediaType::Json => quote! {
                serde_json::to_string(#value).map_err(support::Error::request_construction)?
            },
            // Text is the only other media a `content` parameter may carry; lowering rejects the
            // rest, so this arm never sees a codec it cannot render.
            _ => quote! {
                support::serialize_simple(#value, false, support::PercentEncoding::Passthrough)
                    .map_err(support::Error::request_construction)?
            },
        };
    }
    let explode = param.explode;
    let encoding = percent_encoding_tokens(param);
    let name = param.name.clone();
    match &param.style {
        crate::ir::ParamStyle::Matrix => quote! {
            support::serialize_matrix(#name, #value, #explode, #encoding)
                .map_err(support::Error::request_construction)?
        },
        crate::ir::ParamStyle::Label => quote! {
            support::serialize_label(#value, #explode, #encoding)
                .map_err(support::Error::request_construction)?
        },
        _ => quote! {
            support::serialize_simple(#value, #explode, #encoding)
                .map_err(support::Error::request_construction)?
        },
    }
}

/// Emit serialization of one query parameter into the operation's fragment vector.
///
/// Fragments arrive at `build_url` already percent-encoded, so the style's delimiters stay
/// literal and remain distinguishable from the same character inside a value.
fn query_param_tokens(
    param: &crate::ir::Parameter,
    name: &str,
    value: TokenStream,
    query_binding: &crate::name::Ident,
) -> TokenStream {
    let encoding = percent_encoding_tokens(param);
    match &param.style {
        crate::ir::ParamStyle::Form | crate::ir::ParamStyle::Cookie => {
            let explode = param.explode;
            quote! {
                #query_binding.extend(
                    support::serialize_form(#name, #value, #explode, #encoding)
                        .map_err(support::Error::request_construction)?,
                );
            }
        }
        crate::ir::ParamStyle::Delimited(delimiter) => {
            let delimiter = delimiter_tokens(*delimiter);
            quote! {
                #query_binding.extend(
                    support::serialize_delimited(#name, #value, #delimiter, #encoding)
                        .map_err(support::Error::request_construction)?,
                );
            }
        }
        crate::ir::ParamStyle::DeepObject => quote! {
            #query_binding.extend(
                support::serialize_deep_object(#name, #value, #encoding)
                    .map_err(support::Error::request_construction)?,
            );
        },
        crate::ir::ParamStyle::Content(_) => {
            let rendered = param_value_tokens(param, value);
            quote! {
                #query_binding.push(format!(
                    "{}={}",
                    support::encode(#name, #encoding),
                    support::encode(&#rendered, #encoding),
                ));
            }
        }
        crate::ir::ParamStyle::Simple
        | crate::ir::ParamStyle::Matrix
        | crate::ir::ParamStyle::Label => quote! {},
    }
}

fn querystring_param_tokens(
    param: &crate::ir::Parameter,
    value: TokenStream,
    query_binding: &crate::name::Ident,
    raw_query_binding: &crate::name::Ident,
) -> TokenStream {
    match &param.style {
        // A JSON whole-query value is one opaque, fully-encoded token.
        crate::ir::ParamStyle::Content(MediaType::Json) => quote! {
            #raw_query_binding = Some(support::encode(
                &serde_json::to_string(#value)
                    .map_err(support::Error::request_construction)?,
                support::PercentEncoding::Form,
            ));
        },
        crate::ir::ParamStyle::Content(MediaType::FormUrlEncoded) => quote! {
            #query_binding.extend(
                support::serialize_form("", #value, true, support::PercentEncoding::Form)
                    .map_err(support::Error::request_construction)?,
            );
        },
        _ => quote! {},
    }
}

/// Emit serialization of one cookie parameter into the operation's cookie fragments.
///
/// `serialize_form` already returns `name=value` fragments; the caller joins them with `"; "`.
fn cookie_param_tokens(
    param: &crate::ir::Parameter,
    name: &str,
    value: TokenStream,
    cookies_binding: &crate::name::Ident,
) -> TokenStream {
    let encoding = percent_encoding_tokens(param);
    match &param.style {
        crate::ir::ParamStyle::Form | crate::ir::ParamStyle::Cookie => {
            let explode = param.explode;
            quote! {
                #cookies_binding.extend(
                    support::serialize_form(#name, #value, #explode, #encoding)
                        .map_err(support::Error::request_construction)?,
                );
            }
        }
        crate::ir::ParamStyle::Content(_) => {
            let rendered = param_value_tokens(param, value);
            quote! { #cookies_binding.push(format!("{}={}", #name, #rendered)); }
        }
        crate::ir::ParamStyle::Simple
        | crate::ir::ParamStyle::Matrix
        | crate::ir::ParamStyle::Label
        | crate::ir::ParamStyle::Delimited(_)
        | crate::ir::ParamStyle::DeepObject => quote! {},
    }
}

/// Turn lowered documentation into `#[doc = …]` attributes so IDE hover shows the API docs.
fn doc_tokens(docs: &crate::ir::Docs) -> TokenStream {
    let mut paragraphs: Vec<&str> = Vec::new();
    let summary = docs
        .summary
        .as_deref()
        .filter(|text| !text.trim().is_empty());
    if let Some(summary) = summary {
        paragraphs.push(summary);
    }
    if let Some(description) = docs
        .description
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        if summary != Some(description) {
            paragraphs.push(description);
        }
    }
    if paragraphs.is_empty() {
        if let Some(title) = docs.title.as_deref().filter(|text| !text.trim().is_empty()) {
            paragraphs.push(title);
        }
    }
    if paragraphs.is_empty() {
        return quote! {};
    }
    let text = normalize_rustdoc(&paragraphs.join("\n\n"));
    quote! { #[doc = #text] }
}

/// Normalize spec-authored prose for Rust's documentation lint surface. Tabs are visually
/// ambiguous in rustdoc and trip Clippy's `tabs_in_doc_comments`; four spaces preserve table/code
/// alignment without altering the API's semantic documentation.
fn normalize_rustdoc(text: &str) -> String {
    #[derive(Clone, Copy)]
    enum Continuation {
        None,
        List(usize),
        Quote,
    }

    fn list_indent(line: &str) -> Option<usize> {
        let leading = line.len() - line.trim_start_matches(' ').len();
        let text = &line[leading..];
        if matches!(text.as_bytes().first(), Some(b'-' | b'*' | b'+')) {
            let spacing = text.as_bytes()[1..]
                .iter()
                .take_while(|byte| **byte == b' ')
                .count();
            return (spacing > 0).then_some(leading + 1 + spacing);
        }
        let digits = text.bytes().take_while(u8::is_ascii_digit).count();
        let marker = *text.as_bytes().get(digits)?;
        if digits == 0 || !matches!(marker, b'.' | b')') {
            return None;
        }
        let spacing = text.as_bytes()[digits + 1..]
            .iter()
            .take_while(|byte| **byte == b' ')
            .count();
        (spacing > 0).then_some(leading + digits + 1 + spacing)
    }

    let text = text.replace('\t', "    ");
    let mut continuation = Continuation::None;
    let mut fence: Option<char> = None;
    let mut output = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start_matches(' ');
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let marker = trimmed.chars().next().expect("non-empty fence");
            fence = match fence {
                Some(active) if active == marker => None,
                None => Some(marker),
                active => active,
            };
            continuation = Continuation::None;
            output.push(line.to_owned());
            continue;
        }
        if fence.is_some() {
            output.push(line.to_owned());
            continue;
        }
        if trimmed.is_empty() {
            continuation = Continuation::None;
            output.push(String::new());
        } else if trimmed.starts_with('>') {
            continuation = Continuation::Quote;
            output.push(line.to_owned());
        } else if let Some(indent) = list_indent(line) {
            continuation = Continuation::List(indent);
            output.push(line.to_owned());
        } else {
            match continuation {
                Continuation::None => output.push(line.to_owned()),
                Continuation::Quote => output.push(format!("> {line}")),
                Continuation::List(indent) => {
                    let leading = line.len() - trimmed.len();
                    output.push(format!(
                        "{}{line}",
                        " ".repeat(indent.saturating_sub(leading))
                    ));
                }
            }
        }
    }
    output.join("\n")
}

/// Document the generated `Client` with the API identity and its declared servers.
fn client_doc_tokens(api: &Api) -> TokenStream {
    let mut text = format!("Client for {} v{}.", api.info.title, api.info.version);
    if let Some(description) = api
        .info
        .description
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        text.push_str("\n\n");
        text.push_str(description);
    }
    if !api.servers.is_empty() {
        text.push_str("\n\nServers declared by the spec:");
        for server in &api.servers {
            text.push_str("\n- `");
            text.push_str(&server.url);
            text.push('`');
            if let Some(description) = server
                .description
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                text.push_str(" — ");
                text.push_str(description);
            }
        }
    }
    let text = normalize_rustdoc(&text);
    quote! { #[doc = #text] }
}

/// Emit an operation's optional-parameters `…Params` struct (deriving `Default`, public fields)
/// plus an `impl` of fluent `#[must_use]` consuming setters — one per optional param, named after
/// its field — so callers can write `…Params::default().foo(x).bar(y)` instead of a struct literal.
pub(crate) fn emit_params_struct(
    operation: &Operation,
    names: &Names,
    options: &CodegenOptions,
) -> TokenStream {
    let ident = names
        .params_structs
        .get(&operation.id)
        .expect("params name allocated");
    let optional: Vec<&crate::ir::Parameter> = operation
        .params
        .iter()
        .filter(|param| !param.required)
        .collect();
    // The setter method reuses the field ident verbatim (same escaping/keyword handling), so
    // build it once per param.
    let field_ident =
        |param: &crate::ir::Parameter| escaped_token(&param.name, crate::name::IdentRole::Field);
    let fields = optional.iter().map(|param| {
        let ident = field_ident(param);
        let wire = &param.name;
        // Every struct param is optional, so the field is always an `Option`. `ty_tokens`
        // already wraps a nullable param (`"null"` in its type array) in `Option`, so only wrap
        // again when it did not — otherwise a nullable optional param becomes `Option<Option<T>>`
        // and the query/header `value.to_string()` serialization would not compile
        // (`Option<T>: !Display`). Absent and `null` both collapse to `None`.
        let ty = if param.ty.nullable {
            ty_tokens(param.ty, names, options, true)
        } else {
            let inner = ty_tokens(param.ty, names, options, true);
            quote! { Option<#inner> }
        };
        let mut notes: Vec<String> = Vec::new();
        if param.deprecated {
            notes.push("Deprecated per the spec.".to_owned());
        }
        if let Some(default) = &param.default_display {
            notes.push(format!("Default: `{default}`."));
        }
        let notes = notes
            .iter()
            .map(|note| normalize_rustdoc(note))
            .map(|note| quote! { #[doc = #note] });
        quote! {
            #(#notes)*
            #[serde(rename = #wire, skip_serializing_if = "Option::is_none")]
            pub #ident: #ty,
        }
    });
    // Fluent consuming setters, one per optional param, in field order. Each takes the field's
    // inner `T` by value (never the `Option` wrapper) and stores `Some(T)`: a nullable optional
    // param's field is `Option<T>` for the same reason an ordinary optional param's is, so both
    // accept `T`. `T`-by-value (not `impl Into<T>`) keeps inference/coherence trivial for every
    // generated field type.
    let setters = optional.iter().map(|param| {
        let ident = field_ident(param);
        let inner = ty_tokens(
            Ty {
                nullable: false,
                ..param.ty
            },
            names,
            options,
            true,
        );
        let doc = format!("Set the `{}` parameter.", param.name);
        quote! {
            #[doc = #doc]
            #[must_use]
            pub fn #ident(mut self, value: #inner) -> Self {
                self.#ident = Some(value);
                self
            }
        }
    });
    quote! {
        #[allow(dead_code)]
        #[derive(Debug, Clone, Default, serde::Serialize)]
        pub struct #ident {
            #(#fields)*
        }

        // Setters named after fields can trip `wrong_self_convention` when a param is named
        // `is_*`/`to_*`/etc.; a consuming builder setter is the intended shape, so allow it here.
        #[allow(dead_code, clippy::wrong_self_convention)]
        impl #ident {
            #(#setters)*
        }
    }
}

/// Emit an operation's multi-status success response enum, one variant per documented success
/// status (empty unless [`crate::ir::Responses::success`] is [`SuccessShape::Enum`]). The variant
/// is selected by HTTP status at decode time, so the enum derives only `Debug, Clone` — no
/// whole-enum `Deserialize`, no `serde(untagged)`.
pub(crate) fn emit_response_enum(
    operation: &Operation,
    names: &Names,
    options: &CodegenOptions,
) -> TokenStream {
    match operation.responses.success() {
        SuccessShape::Enum(entries) => {
            let method_ident = names
                .operations
                .get(&operation.id)
                .expect("operation name allocated");
            let ident = success_enum_ident(method_ident);
            let variants = entries
                .iter()
                .map(|(spec, ty)| response_variant_def(*spec, *ty, names, options));
            quote! {
                #[allow(dead_code)]
                #[derive(Debug, Clone)]
                pub enum #ident {
                    #(#variants)*
                }
            }
        }
        _ => quote! {},
    }
}

/// One response-enum variant definition: a payload-carrying `Status2xx(types::T)` for a bodied
/// status, or a payload-free `Status204` unit variant for a documented bodyless status.
fn response_variant_def(
    spec: crate::ir::StatusSpec,
    ty: Option<Ty>,
    names: &Names,
    options: &CodegenOptions,
) -> TokenStream {
    let variant_ident = status_variant_ident(spec);
    match ty {
        Some(ty) => {
            let ty = response_payload_ty_tokens(ty, names, options, true);
            quote! { #variant_ident(#ty), }
        }
        None => quote! { #variant_ident, },
    }
}

/// Emit an operation's typed error enum (or type alias for a single error body).
pub(crate) fn emit_error_enum(
    operation: &Operation,
    names: &Names,
    options: &CodegenOptions,
) -> TokenStream {
    let method_ident = names
        .operations
        .get(&operation.id)
        .expect("operation name allocated");
    let error_ident = format_ident!("{}Error", to_pascal(method_ident.as_str()));
    match operation.responses.error() {
        // Multiple documented error bodies → a payload-carrying enum, one variant per status. The
        // variant is chosen by HTTP status at classification time, so it derives no whole-enum
        // `Deserialize` (and never `serde(untagged)`); each variant's body is decoded on its own.
        ErrorShape::Enum(entries) => {
            let variants = entries
                .iter()
                .map(|(spec, ty)| response_variant_def(*spec, *ty, names, options));
            quote! {
                #[allow(dead_code)]
                #[derive(Debug, Clone)]
                pub enum #error_ident {
                    #(#variants)*
                }
            }
        }
        // A single documented error body: a plain alias to that type.
        ErrorShape::Single(ty) => {
            let ty = ty_tokens(ty, names, options, true);
            quote! {
                #[allow(dead_code)]
                pub type #error_ident = #ty;
            }
        }
        // No documented error body: every non-success status is Error::UnexpectedStatus, and the
        // uninhabited alias makes Error::Api impossible to construct.
        ErrorShape::None => quote! {
            #[allow(dead_code)]
            pub type #error_ident = std::convert::Infallible;
        },
    }
}

/// Emit the private `support` module by embedding the freestanding runtime source verbatim, under
/// `#![forbid(unsafe_code)]`. When `uses_xml` is set (the API has an `application/xml` / `text/xml`
/// body), the feature-gated XML codec module is embedded and its helpers re-exported; otherwise it
/// is omitted entirely, so a non-XML output carries no `quick-xml` reference. `uses_time` embeds
/// the RFC 3339 `DateTime`/`Date` newtypes on the same terms.
pub(crate) fn emit_support(uses_xml: bool, uses_streams: bool, uses_time: bool) -> TokenStream {
    let embed = |file: &crate::support::SupportFile| {
        let stem = file.name.trim_end_matches(".rs");
        let ident = format_ident!("{}", stem);
        // Each runtime file keeps its `#[cfg(test)]` module last; strip it at embed time — the
        // runtime is tested in the support-runtime crate, and test-only `crate::` imports would
        // not survive the module renesting.
        let source = file
            .contents
            .split("#[cfg(test)]")
            .next()
            .expect("split yields at least one part")
            .replace("crate::", "super::");
        let tokens: TokenStream = source
            .parse()
            .expect("embedded support runtime parses as Rust tokens");
        quote! {
            mod #ident {
                #tokens
            }
        }
    };
    let modules = crate::support::runtime_files().iter().map(embed);
    let stream_module = uses_streams.then(|| embed(&crate::support::stream_runtime_file()));
    let stream_reexport = uses_streams.then(|| {
        quote! {
            pub use stream::{
                EventStream, Framing, ReconnectPolicy, ReconnectReason, ReconnectWait, StreamError,
            };
        }
    });
    // The XML codec module is embedded only when the API uses an XML body, and only then does the
    // dependency audit require `quick-xml` of the consumer. A non-XML output never references it.
    let xml_module = uses_xml.then(|| embed(&crate::support::xml_runtime_file()));
    let xml_reexport = uses_xml.then(|| {
        quote! { pub use xml::{classify_error_xml, decode_success_xml, to_xml}; }
    });
    // The RFC 3339 newtypes are embedded only when a date-typed primitive survives lowering with the
    // `time` mapping enabled; only then does the audit require `time` of the consumer.
    let datetime_module = uses_time.then(|| embed(&crate::support::datetime_runtime_file()));
    let datetime_reexport = uses_time.then(|| {
        quote! { pub use datetime::{serialize_millis, Date, DateTime}; }
    });
    // The blocking facade (`BlockingRuntime`) is embedded unconditionally but gated on the
    // `blocking` feature AND `not(target_arch = "wasm32")` at the module level: the tokio-dependent
    // code compiles only when a consumer opts in on a native target, so a default build carries no
    // tokio reference and a wasm build never pulls tokio even with the feature on (tokio's blocking
    // runtime cannot run on the single-threaded browser). A consumer opts in by declaring its own
    // `blocking` feature wired to an optional, non-wasm tokio dependency — which is what
    // `spargen deps` prints, commented out, and what the audit then requires.
    let blocking_inner = embed(&crate::support::blocking_runtime_file());
    let blocking_module = quote! {
        #[cfg(all(feature = "blocking", not(target_arch = "wasm32")))]
        #blocking_inner
    };
    let blocking_reexport = quote! {
        #[cfg(all(feature = "blocking", not(target_arch = "wasm32")))]
        pub use blocking::BlockingRuntime;
    };
    quote! {
        /// The freestanding runtime embedded verbatim into this output; no spargen crate exists
        /// at runtime.
        #[forbid(unsafe_code)]
        #[allow(dead_code, unexpected_cfgs, unused_imports, clippy::result_large_err)]
        mod support {
            #(#modules)*
            #stream_module
            #xml_module
            #datetime_module
            #blocking_module

            pub use auth::{AuthError, AuthKind, AuthScheme, Credential, ExposeSecret, SecretString, TokenFuture, TokenProvider};
            pub use client::{ClientConfig, ClientCore};
            pub use dispatch::{attach_auth, build_url, build_url_on, build_url_with_query_string, build_url_with_query_string_on, classify_error, classify_error_bytes, classify_error_text, decode_success, decode_success_bytes, decode_success_text, decode_text_body, read_error_body, read_success_body, send, unexpected_status, StatusSpec};
            pub use error::{Error, ProtocolError, RedirectError, RequestError, TimeoutKind, TransportError};
            pub use middleware::{Middleware, MiddlewareBackend, Next};
            pub use header::{parse_header, require_header, HeaderError, HeaderShape};
            pub use json::{check_known_members, deserialize_normalized, integral, is_strict, retained_members, strictly};
            pub use parameter::{encode, serialize_deep_object, serialize_delimited, serialize_form, serialize_form_body, serialize_label, serialize_matrix, serialize_multipart_values, serialize_simple, Delimiter, FormMode, FormProperty, FormStyle, ParameterError, PercentEncoding};
            pub use paginate::{next_link, LinkPaginator};
            pub use response::ResponseValue;
            pub use retry::{exponential_backoff, RetryBackend, RetryOutcome, RetryPolicy, RetryWait};
            pub use transport::{ExecuteFuture, HttpBackend, ReqwestBackend};
            pub use wasm::{MaybeSend, MaybeSync};
            #stream_reexport
            #xml_reexport
            #datetime_reexport
            #blocking_reexport
        }
    }
}

fn emit_type_def(
    id: crate::ir::TypeId,
    def: &TypeDef,
    api: &Api,
    names: &Names,
    options: &CodegenOptions,
    request_model: bool,
    open_enum: bool,
) -> TokenStream {
    let ident = names.types.get(&id).expect("type name allocated");
    let docs = doc_tokens(&def.docs);
    let deprecated = def.docs.deprecated.then(|| quote! { #[deprecated] });
    match &def.kind {
        TypeKind::Struct(object) => {
            // A closed object (`additionalProperties: false`) still ignores members it does not
            // declare, so a response carrying a member added later decodes; only a trial union's
            // exact pass (`support::strictly`) counts them against the object.
            let closed = matches!(object.additional, AdditionalProps::Deny).then(|| {
                let known = object.fields.iter().map(|field| field.name.wire.as_str());
                quote! {
                    impl<'de> serde::Deserialize<'de> for #ident {
                        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                        where
                            D: serde::Deserializer<'de>,
                        {
                            if super::support::is_strict() {
                                let value = serde_json::Value::deserialize(deserializer)?;
                                super::support::check_known_members::<D::Error>(
                                    &value,
                                    &[#(#known),*],
                                )?;
                                #ident::deserialize(value).map_err(serde::de::Error::custom)
                            } else {
                                #ident::deserialize(deserializer)
                            }
                        }
                    }
                    impl serde::Serialize for #ident {
                        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                        where
                            S: serde::Serializer,
                        {
                            #ident::serialize(self, serializer)
                        }
                    }
                }
            });
            let remote = closed
                .is_some()
                .then(|| quote! { #[serde(remote = "Self")] });
            let fields = object
                .fields
                .iter()
                .map(|field| emit_field(id, field, api, names, options, request_model));

            let additional = match &object.additional {
                AdditionalProps::Typed(item) => {
                    let ty = ty_tokens(**item, names, options, false);
                    let overflow = names
                        .struct_overflow
                        .get(&id)
                        .expect("overflow field name allocated");
                    let normalized =
                        emit_overflow_helper(**item, api, names, options).map(|(helper, _)| {
                            let helper = helper.to_string();
                            quote! { deserialize_with = #helper, }
                        });
                    quote! { #[serde(flatten, #normalized)] pub #overflow: BTreeMap<String, #ty>, }
                }
                // An open object keeps the members it does not declare, so they round-trip. The
                // XML codec cannot write a flattened map, so an XML body keeps its declared shape.
                AdditionalProps::Allow if xml_types(api).contains(&id) => quote! {},
                AdditionalProps::Allow => {
                    let overflow = names
                        .struct_overflow
                        .get(&id)
                        .expect("overflow field name allocated");
                    quote! {
                        /// Members the schema does not declare, kept as received.
                        #[serde(flatten)]
                        pub #overflow: BTreeMap<String, serde_json::Value>,
                    }
                }
                AdditionalProps::Deny => quote! {},
            };
            quote! {
                #docs
                #deprecated
                #[derive(Debug, Clone, Serialize, Deserialize)]
                #remote
                pub struct #ident {
                    #(#fields)*
                    #additional
                }
                #closed
            }
        }
        TypeKind::Enum(enumeration) if enumeration.repr == ScalarRepr::String => {
            let values: Vec<(&String, &crate::name::Ident)> = enumeration
                .variants
                .iter()
                .map(|variant| {
                    let value = match variant {
                        ScalarValue::String(value) => value,
                        _ => unreachable!("string repr has string variants"),
                    };
                    let variant_ident = names
                        .variants
                        .get(&(id, value.clone()))
                        .expect("variant name allocated");
                    (value, variant_ident)
                })
                .collect();
            let display_arms = values.iter().map(|(value, variant_ident)| {
                quote! { #ident::#variant_ident => #value, }
            });
            // A one-value enum is a constant: it stays exactly that value, as constant fields do.
            if !open_enum || values.len() == 1 {
                let variants = values.iter().map(|(value, variant_ident)| {
                    quote! { #[serde(rename = #value)] #variant_ident, }
                });
                return quote! {
                    #docs
                    #deprecated
                    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
                    #[non_exhaustive]
                    pub enum #ident {
                        #(#variants)*
                    }

                    impl std::fmt::Display for #ident {
                        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                            f.write_str(match self {
                                #(#display_arms)*
                            })
                        }
                    }
                };
            }
            // An open enum: the values the contract lists, plus one holding any other string, so a
            // value the server adds later decodes and serializes back unchanged.
            let unknown = unknown_variant_ident(values.iter().map(|(_, ident)| ident.as_str()));
            let variants = values.iter().map(|(value, variant_ident)| {
                let doc = format!("`{value}`");
                quote! { #[doc = #doc] #variant_ident, }
            });
            // `Self`, not the type's name, so equal enums stay equal item for item.
            let parse_arms = values.iter().map(|(value, variant_ident)| {
                quote! { #value => Ok(Self::#variant_ident), }
            });
            let str_arms = values.iter().map(|(value, variant_ident)| {
                quote! { Self::#variant_ident => #value, }
            });
            let known: Vec<&String> = values.iter().map(|(value, _)| *value).collect();
            let expected = format!(
                "one of {}",
                known
                    .iter()
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            quote! {
                #docs
                #deprecated
                #[derive(Debug, Clone, PartialEq, Eq)]
                #[non_exhaustive]
                pub enum #ident {
                    #(#variants)*
                    /// A value this version of the client does not know, kept as received.
                    #unknown(String),
                }

                impl #ident {
                    /// The value as it appears on the wire.
                    pub fn as_str(&self) -> &str {
                        match self {
                            #(#str_arms)*
                            Self::#unknown(value) => value,
                        }
                    }
                }

                impl std::fmt::Display for #ident {
                    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                        f.write_str(self.as_str())
                    }
                }

                impl serde::Serialize for #ident {
                    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                    where
                        S: serde::Serializer,
                    {
                        serializer.serialize_str(self.as_str())
                    }
                }

                impl<'de> serde::Deserialize<'de> for #ident {
                    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                    where
                        D: serde::Deserializer<'de>,
                    {
                        let value = String::deserialize(deserializer)?;
                        match value.as_str() {
                            #(#parse_arms)*
                            // A trial union's exact pass counts an unlisted value against the
                            // variant.
                            _ if super::support::is_strict() => Err(serde::de::Error::invalid_value(
                                serde::de::Unexpected::Str(&value),
                                &#expected,
                            )),
                            _ => Ok(Self::#unknown(value)),
                        }
                    }
                }
            }
        }
        TypeKind::Enum(enumeration) => {
            let ty = match enumeration.repr {
                ScalarRepr::String => quote! { String },
                ScalarRepr::Int => quote! { i64 },
                ScalarRepr::Bool => quote! { bool },
            };
            quote! { #docs pub type #ident = #ty; }
        }
        TypeKind::Never => {
            let error = format!("no JSON value can inhabit schema {}", ident.as_str());
            quote! {
                #docs
                #deprecated
                #[derive(Debug, Clone)]
                pub enum #ident {}

                impl<'de> serde::Deserialize<'de> for #ident {
                    fn deserialize<D>(_deserializer: D) -> Result<Self, D::Error>
                    where
                        D: serde::Deserializer<'de>,
                    {
                        Err(serde::de::Error::custom(#error))
                    }
                }

                impl serde::Serialize for #ident {
                    fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
                    where
                        S: serde::Serializer,
                    {
                        Err(serde::ser::Error::custom(#error))
                    }
                }
            }
        }
        TypeKind::Union(union) => {
            // A union narrowed by `allOf` constraint components checks each constraint against the
            // buffered JSON value, in both directions, instead of copying every member.
            let constraint_checks: Vec<TokenStream> = union
                .constraints
                .iter()
                .map(|constraint| {
                    let ty = ty_tokens(
                        crate::ir::Ty {
                            boxed: false,
                            nullable: false,
                            ..*constraint
                        },
                        names,
                        options,
                        false,
                    );
                    quote! {
                        <#ty as serde::Deserialize>::deserialize(&value)
                            .map_err(serde::de::Error::custom)?;
                    }
                })
                .collect();
            match &union.strategy {
                // Strategy A: a discriminator → a custom `Deserialize`/`Serialize` over a buffered
                // `serde_json::Value`. NOT serde `#[serde(tag = ...)]`: internal tagging consumes the tag
                // field out of the buffer, so a variant struct that declares the discriminator as a
                // (usually required) property would fail with "missing field". Instead the WHOLE value is
                // handed to the selected variant (it keeps its own tag field), and on serialize the tag is
                // re-inserted only when the variant did not already write it. No `untagged`, no `Value`
                // degrade.
                UnionStrategy::Discriminated {
                    tag_field,
                    tags,
                    categories,
                    default_variant,
                } => {
                    let variant_defs = union.variants.iter().map(|variant| {
                        let variant_ident = names
                            .variants
                            .get(&(id, variant.name_hint.clone()))
                            .expect("union variant name allocated");
                        let ty = union_variant_ty_tokens(variant.ty, names, options);
                        quote! { #variant_ident(#ty), }
                    });
                    let category_arms =
                        union
                            .variants
                            .iter()
                            .zip(categories)
                            .filter_map(|(variant, category)| {
                                let category = category.as_ref()?;
                                let variant_ident = names
                                    .variants
                                    .get(&(id, variant.name_hint.clone()))
                                    .expect("union variant name allocated");
                                let predicate = match category {
                                    JsonCategory::String => quote! { value.is_string() },
                                    JsonCategory::Number => quote! { value.is_number() },
                                    JsonCategory::Boolean => quote! { value.is_boolean() },
                                    JsonCategory::Array => quote! { value.is_array() },
                                    JsonCategory::Object => quote! { value.is_object() },
                                };
                                let normalize = variant_normalizer(api, names, variant.ty);
                                Some(quote! {
                                    if #predicate {
                                        #normalize
                                        return serde_json::from_value(value)
                                            .map(#ident::#variant_ident)
                                            .map_err(serde::de::Error::custom);
                                    }
                                })
                            });
                    let de_arms = union
                        .variants
                        .iter()
                        .zip(tags)
                        .filter_map(|(variant, tag)| {
                            let tag = tag.as_ref()?;
                            let variant_ident = names
                                .variants
                                .get(&(id, variant.name_hint.clone()))
                                .expect("union variant name allocated");
                            let normalize = variant_normalizer(api, names, variant.ty);
                            Some(quote! {
                                #tag => {
                                    #normalize
                                    serde_json::from_value(value)
                                        .map(#ident::#variant_ident)
                                        .map_err(serde::de::Error::custom)
                                }
                            })
                        });
                    let ser_arms = union.variants.iter().zip(tags).map(|(variant, tag)| {
                        let variant_ident = names
                            .variants
                            .get(&(id, variant.name_hint.clone()))
                            .expect("union variant name allocated");
                        let tag = match tag {
                            Some(tag) => quote! { Some(#tag) },
                            None => quote! { None },
                        };
                        quote! {
                            #ident::#variant_ident(inner) => (
                                serde_json::to_value(inner).map_err(serde::ser::Error::custom)?,
                                #tag,
                            ),
                        }
                    });
                    let missing_tag = format!(
                        "missing discriminator field `{tag_field}` for union {}",
                        ident.as_str()
                    );
                    let unknown_tag =
                        format!("unknown discriminator value for union {}", ident.as_str());
                    let non_object = format!(
                        "tagged variant of union {} did not serialize as an object",
                        ident.as_str()
                    );
                    // OpenAPI 3.2 `defaultMapping`: an absent or unrecognized tag falls back to a
                    // named variant instead of failing.
                    let fallback = default_variant.map(|index| {
                        let variant = &union.variants[index];
                        let variant_ident = names
                            .variants
                            .get(&(id, variant.name_hint.clone()))
                            .expect("union variant name allocated");
                        quote! {
                            serde_json::from_value(value)
                                .map(#ident::#variant_ident)
                                .map_err(serde::de::Error::custom)
                        }
                    });
                    let missing_tag_arm = match &fallback {
                        Some(fallback) => fallback.clone(),
                        None => quote! { Err(serde::de::Error::custom(#missing_tag)) },
                    };
                    let unknown_tag_arm = match &fallback {
                        Some(fallback) => fallback.clone(),
                        None => quote! { Err(serde::de::Error::custom(#unknown_tag)) },
                    };
                    let serialize_impl = union_serialize_impl(
                        ident,
                        quote! {
                                let (mut value, tag): (serde_json::Value, Option<&str>) = match self {
                                    #(#ser_arms)*
                                };
                                if let Some(tag) = tag {
                                    let serde_json::Value::Object(map) = &mut value else {
                                        return Err(serde::ser::Error::custom(#non_object));
                                    };
                                    map.entry(#tag_field.to_owned()).or_insert_with(|| {
                                        serde_json::Value::String(tag.to_owned())
                                    });
                                }
                                value.serialize(serializer)
                        },
                        &constraint_checks,
                    );
                    quote! {
                        #docs
                        #deprecated
                        #[derive(Debug, Clone)]
                        pub enum #ident {
                            #(#variant_defs)*
                        }

                        impl<'de> serde::Deserialize<'de> for #ident {
                            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                            where
                                D: serde::Deserializer<'de>,
                            {
                                let value = serde_json::Value::deserialize(deserializer)?;
                                #(#constraint_checks)*
                                #(#category_arms)*
                                let tag = value
                                    .get(#tag_field)
                                    .and_then(serde_json::Value::as_str)
                                    .map(std::borrow::ToOwned::to_owned);
                                let Some(tag) = tag else {
                                    return #missing_tag_arm;
                                };
                                match tag.as_str() {
                                    #(#de_arms)*
                                    _ => #unknown_tag_arm,
                                }
                            }
                        }

                        #serialize_impl
                    }
                }
                // Strategy B: no discriminator but statically-disjoint variants → an enum with a custom
                // content-inspecting `Deserialize` (buffer the value, dispatch on the proven feature) and
                // a `Serialize` that emits just the active variant's inner value (no wrapper, no tag).
                UnionStrategy::Disjoint { features } => {
                    let variant_defs = union.variants.iter().map(|variant| {
                        let variant_ident = names
                            .variants
                            .get(&(id, variant.name_hint.clone()))
                            .expect("union variant name allocated");
                        let ty = union_variant_ty_tokens(variant.ty, names, options);
                        quote! { #variant_ident(#ty), }
                    });
                    let de_arms = union
                        .variants
                        .iter()
                        .zip(features)
                        .map(|(variant, feature)| {
                            let variant_ident = names
                                .variants
                                .get(&(id, variant.name_hint.clone()))
                                .expect("union variant name allocated");
                            let predicate = match feature {
                                DisjointFeature::JsonType(category) => match category {
                                    JsonCategory::String => quote! { value.is_string() },
                                    JsonCategory::Number => quote! { value.is_number() },
                                    JsonCategory::Boolean => quote! { value.is_boolean() },
                                    JsonCategory::Array => quote! { value.is_array() },
                                    JsonCategory::Object => quote! { value.is_object() },
                                },
                                DisjointFeature::RequiredKey(key) => {
                                    quote! { value.get(#key).is_some() }
                                }
                            };
                            let normalize = variant_normalizer(api, names, variant.ty);
                            quote! {
                                if #predicate {
                                    #normalize
                                    return serde_json::from_value(value)
                                        .map(#ident::#variant_ident)
                                        .map_err(serde::de::Error::custom);
                                }
                            }
                        });
                    let ser_arms = union.variants.iter().map(|variant| {
                        let variant_ident = names
                            .variants
                            .get(&(id, variant.name_hint.clone()))
                            .expect("union variant name allocated");
                        quote! { #ident::#variant_ident(inner) => inner.serialize(serializer), }
                    });
                    let error_message =
                        format!("data did not match any variant of union {}", ident.as_str());
                    let serialize_impl = union_serialize_impl(
                        ident,
                        quote! {
                                match self {
                                    #(#ser_arms)*
                                }
                        },
                        &constraint_checks,
                    );
                    quote! {
                        #docs
                        #deprecated
                        #[derive(Debug, Clone)]
                        pub enum #ident {
                            #(#variant_defs)*
                        }

                        impl<'de> serde::Deserialize<'de> for #ident {
                            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                            where
                                D: serde::Deserializer<'de>,
                            {
                                let value = serde_json::Value::deserialize(deserializer)?;
                                #(#constraint_checks)*
                                #(#de_arms)*
                                Err(serde::de::Error::custom(#error_message))
                            }
                        }

                        #serialize_impl
                    }
                }
                UnionStrategy::Trial { mode, priorities } => {
                    // A variant pinning a property to one value (a `const` such as
                    // `platform: "sms"`) is chosen by that value before any variant that would
                    // only match through open or undeclared members.
                    let keyed = union
                        .variants
                        .iter()
                        .any(|variant| constants_carried(api, variant.ty).is_some());
                    let variant_defs = union.variants.iter().map(|variant| {
                        let variant_ident = names
                            .variants
                            .get(&(id, variant.name_hint.clone()))
                            .expect("union variant name allocated");
                        let ty = union_variant_ty_tokens(variant.ty, names, options);
                        quote! { #variant_ident(#ty), }
                    });
                    let attempts =
                        union
                            .variants
                            .iter()
                            .zip(priorities)
                            .map(|(variant, priority)| {
                                let variant_ident = names
                                    .variants
                                    .get(&(id, variant.name_hint.clone()))
                                    .expect("union variant name allocated");
                                let ty = union_variant_ty_tokens(variant.ty, names, options);
                                let mut attempt = variant_attempt(api, names, variant.ty, &ty);
                                // In the keyed pass only the variants whose constants the value
                                // carries are tried.
                                if keyed {
                                    attempt = match constants_carried(api, variant.ty) {
                                        Some(carried) => quote! {
                                            (!keyed || #carried).then(|| #attempt).flatten()
                                        },
                                        None => quote! { (!keyed).then(|| #attempt).flatten() },
                                    };
                                }
                                quote! {
                                    if let Some(inner) = #attempt {
                                        match_count += 1;
                                        // Without an exact match, prefer the variant that keeps the most
                                        // of the value's members.
                                        let retained = if strict {
                                            0
                                        } else {
                                            super::support::retained_members(&value, &inner)
                                        };
                                        let rank = (retained, #priority);
                                        let replace = match &selected {
                                            Some((selected_rank, _)) => rank > *selected_rank,
                                            None => true,
                                        };
                                        if replace {
                                            selected = Some((rank, #ident::#variant_ident(inner)));
                                        }
                                    }
                                }
                            });
                    let ser_arms = union.variants.iter().map(|variant| {
                        let variant_ident = names
                            .variants
                            .get(&(id, variant.name_hint.clone()))
                            .expect("union variant name allocated");
                        quote! {
                            #ident::#variant_ident(inner) => {
                                serde_json::to_value(inner).map_err(serde::ser::Error::custom)?
                            }
                        }
                    });
                    let validations = union.variants.iter().map(|variant| {
                        let ty = union_variant_ty_tokens(variant.ty, names, options);
                        let attempt = variant_attempt(api, names, variant.ty, &ty);
                        quote! {
                            if #attempt.is_some() {
                                match_count += 1;
                            }
                        }
                    });
                    let expected = match mode {
                        UnionMode::OneOf => "exactly one",
                        UnionMode::AnyOf => "at least one",
                    };
                    let de_valid = match mode {
                        UnionMode::OneOf => quote! { match_count == 1 },
                        UnionMode::AnyOf => quote! { match_count >= 1 },
                    };
                    let ser_valid = de_valid.clone();
                    let de_error = format!(
                        "data must match {expected} typed variant of union {}",
                        ident.as_str()
                    );
                    let ser_error = format!(
                        "serialized value must match {expected} typed variant of union {}",
                        ident.as_str()
                    );
                    let de_invalid = match mode {
                        UnionMode::OneOf => quote! { match_count != 1 },
                        UnionMode::AnyOf => quote! { match_count == 0 },
                    };
                    let pass_and_select = if keyed {
                        let carried = union
                            .variants
                            .iter()
                            .filter_map(|variant| constants_carried(api, variant.ty));
                        quote! {
                            let pass = |strict: bool, keyed: bool| -> (usize, Selected) {
                                let mut match_count = 0_usize;
                                let mut selected: Selected = None;
                                #(#attempts)*
                                (match_count, selected)
                            };
                            // Each pass is exact first, where members and enum values a variant
                            // does not know count against it; if no variant matches exactly, the
                            // best variant that reads the value, so a response carrying additions
                            // still decodes. The keyed pass tries only the variants whose
                            // constants the value carries; the others are the fallback.
                            let select = |keyed: bool| -> (usize, Selected) {
                                let (mut match_count, mut selected) = pass(true, keyed);
                                if match_count == 0 && !super::support::is_strict() {
                                    let (count, lenient) = pass(false, keyed);
                                    match_count = count.min(1);
                                    selected = lenient;
                                }
                                (match_count, selected)
                            };
                            let (mut match_count, mut selected) = (0_usize, None);
                            if #(#carried)||* {
                                (match_count, selected) = select(true);
                            }
                            if #de_invalid {
                                (match_count, selected) = select(false);
                            }
                        }
                    } else {
                        quote! {
                            let pass = |strict: bool| -> (usize, Selected) {
                                let mut match_count = 0_usize;
                                let mut selected: Selected = None;
                                #(#attempts)*
                                (match_count, selected)
                            };
                            // An exact pass first, where members and enum values a variant
                            // does not know count against it; if no variant matches exactly,
                            // the best variant that reads the value, so a response carrying
                            // additions still decodes.
                            let (mut match_count, mut selected) = pass(true);
                            if match_count == 0 && !super::support::is_strict() {
                                let (count, lenient) = pass(false);
                                match_count = count.min(1);
                                selected = lenient;
                            }
                        }
                    };
                    let serialize_impl = union_serialize_impl(
                        ident,
                        quote! {
                                let value = match self {
                                    #(#ser_arms),*
                                };
                                let count = |strict: bool| -> usize {
                                    let mut match_count = 0_usize;
                                    #(#validations)*
                                    match_count
                                };
                                let mut match_count = count(true);
                                if match_count == 0 && !super::support::is_strict() {
                                    match_count = count(false).min(1);
                                }
                                if #ser_valid {
                                    value.serialize(serializer)
                                } else {
                                    Err(serde::ser::Error::custom(#ser_error))
                                }
                        },
                        &constraint_checks,
                    );
                    quote! {
                        #docs
                        #deprecated
                        #[derive(Debug, Clone)]
                        pub enum #ident {
                            #(#variant_defs)*
                        }

                        impl<'de> serde::Deserialize<'de> for #ident {
                            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                            where
                                D: serde::Deserializer<'de>,
                            {
                                let value = serde_json::Value::deserialize(deserializer)?;
                                #(#constraint_checks)*
                                // The chosen variant, ranked by (members kept, priority).
                                type Selected = Option<((usize, u32), #ident)>;
                                #pass_and_select
                                if #de_valid {
                                    selected
                                        .map(|(_, value)| value)
                                        .ok_or_else(|| serde::de::Error::custom(#de_error))
                                } else {
                                    Err(serde::de::Error::custom(#de_error))
                                }
                            }
                        }

                        #serialize_impl
                    }
                }
            }
        }
        _ => {
            let ty = type_kind_tokens(&def.kind, api, names, options);
            quote! { #docs pub type #ident = #ty; }
        }
    }
}

fn emit_field(
    id: crate::ir::TypeId,
    field: &Field,
    api: &Api,
    names: &Names,
    options: &CodegenOptions,
    request_model: bool,
) -> TokenStream {
    let ident = names
        .fields
        .get(&(id, field.name.wire.clone()))
        .expect("field name allocated");
    // An `xml.name`/`xml.attribute` hint overrides the serde wire name for XML bodies (an attribute
    // uses quick-xml's `@name` convention); otherwise the plain property wire name is used. The Rust
    // identifier and the names-table key stay keyed off the original property name, so only the wire
    // string changes.
    let wire = field
        .xml
        .wire_override(&field.name.wire)
        .unwrap_or_else(|| field.name.wire.clone());
    // `ty_tokens` represents nullability; an optional member gets an independent outer `Option` for
    // absence. Where the member is nullable (or in a request model) the two stay apart: None omits
    // the key, Some(None) is null, Some(Some(value)) is a value, so a decoded value serializes back
    // unchanged. An optional non-nullable member of a response-only model reads `null` as absent.
    let mut ty = ty_tokens(field.ty, names, options, false);
    if !field.required {
        ty = quote! { Option<#ty> };
    }
    let presence = field_presence(field, request_model);
    let validation = super::normalize::Normalization { api, names };
    let normalized = !validation
        .normalizer(
            Ty {
                nullable: false,
                ..field.ty
            },
            quote! { value },
            0,
        )
        .is_empty();
    let deserialize = if let Some(value) = names.consts.get(&field.ty.id) {
        // A string constant is a plain `String` whose value is checked in both directions, so it
        // accepts and produces exactly what the one-variant enum it replaces did. The checked
        // deserializers keep the presence rules below (a required field stays required; an
        // optional request field keeps `deserialize_request_field`'s null handling).
        let (de, ser) = const_helper_idents(names, value, presence);
        let (de, ser) = (de.to_string(), ser.to_string());
        quote! { deserialize_with = #de, serialize_with = #ser, }
    } else if normalized {
        let (helper, _) = emit_field_helper(field, api, names, options, request_model)
            .expect("normalized field has a helper");
        let helper = helper.to_string();
        quote! { deserialize_with = #helper, }
    } else if field.required && (!field.ty.nullable || !request_model) {
        // Ordinary required types already reject absence. Avoid generating redundant serde
        // wrappers for those fields; only Option's special missing-field behavior needs overriding
        // (a response-only model keeps reading a missing nullable member as `None`).
        quote! {}
    } else if !field.required && !presence {
        // An optional non-nullable member of a response-only model: `None` is absent or null.
        quote! {}
    } else if field.required {
        // An explicit deserializer makes serde reject a missing field even when its type is Option.
        quote! { deserialize_with = "serde::Deserialize::deserialize", }
    } else {
        quote! { deserialize_with = "deserialize_request_field", }
    };
    // A `date-time` whose pattern demands milliseconds is written with exactly three fraction
    // digits, alone or as the items of an array.
    let millis = {
        let direct = api.types.get(field.ty.id);
        let item = match direct.map(|def| &def.kind) {
            Some(TypeKind::Array(item)) if names.inline.contains_key(&field.ty.id) => {
                api.types.get(item.id)
            }
            _ => None,
        };
        direct
            .into_iter()
            .chain(item)
            .any(super::normalize::renders_millis)
    };
    let deserialize = if millis && options.feature_time {
        quote! { #deserialize serialize_with = "super::support::serialize_millis", }
    } else {
        deserialize
    };
    // An absent optional field deserializes as `None` and `None` is not serialized, even when the
    // spec declares a `default`: the default is documented, and the service applies it.
    let serde_default = if field.required {
        // A response-only model reads a missing nullable member as `None`, also through a helper.
        if normalized && field.ty.nullable && !request_model {
            quote! { default, }
        } else {
            quote! {}
        }
    } else {
        quote! { default, skip_serializing_if = "Option::is_none", }
    };
    let mut notes: Vec<String> = Vec::new();
    if field.deprecated {
        notes.push("Deprecated per the spec.".to_owned());
    }
    if field.read_only {
        notes.push("Read-only: set by the server; ignored in requests.".to_owned());
    }
    if field.write_only {
        notes.push("Write-only: sent in requests; absent from responses.".to_owned());
    }
    if let Some(default) = &field.default {
        notes.push(default.doc_note.clone());
    }
    let notes: Vec<_> = notes
        .iter()
        .map(|note| normalize_rustdoc(note))
        .map(|note| quote! { #[doc = #note] })
        .collect();
    // An inline type has no item to carry the property's documentation, so the field carries it.
    // A blank line separates it from the notes, so prose ending in a list or block quote cannot
    // swallow them as a lazy continuation.
    // A named type carries its own docs; the field adds the property's own docs only when they
    // differ (a description beside a `$ref`, or on a nullable reference).
    let docs = match api.types.get(field.ty.id) {
        Some(def) if names.inline.contains_key(&field.ty.id) => Some(doc_tokens(&def.docs)),
        Some(def) if field.docs != Docs::default() && field.docs != def.docs => {
            Some(doc_tokens(&field.docs))
        }
        _ => None,
    }
    .filter(|docs| !docs.is_empty());
    let separator = (docs.is_some() && !notes.is_empty()).then(|| quote! { #[doc = ""] });
    quote! {
        #docs
        #separator
        #(#notes)*
        #[serde(rename = #wire, #serde_default #deserialize)]
        pub #ident: #ty,
    }
}

/// Whether an optional member keeps absence and `null` apart (`Option<Option<T>>` when nullable):
/// always in a request model, and for a nullable member everywhere.
fn field_presence(field: &Field, request_model: bool) -> bool {
    !field.required && (request_model || field.ty.nullable)
}

/// The checked (de)serializer function names for a string-constant field: one generic pair per
/// constant value (and per presence mode), shared by every field with that value.
fn const_helper_idents(
    names: &Names,
    value: &str,
    presence: bool,
) -> (proc_macro2::Ident, proc_macro2::Ident) {
    let stem = names
        .const_checkers
        .get(value)
        .expect("constant checker allocated");
    let stem = stem.as_str().trim_start_matches("r#");
    let mode = if presence { "_present" } else { "" };
    (
        format_ident!("{}_de{}", stem, mode),
        format_ident!("{}_ser", stem),
    )
}

/// The checked (de)serializers for every string-constant field, plus the trait they share.
fn emit_const_helpers(api: &Api, names: &Names, requests: &BTreeSet<TypeId>) -> TokenStream {
    let mut used: BTreeSet<(String, bool)> = BTreeSet::new();
    for (id, def) in api.types.iter() {
        if !names.types.contains_key(&id) {
            continue;
        }
        let TypeKind::Struct(object) = &def.kind else {
            continue;
        };
        for field in &object.fields {
            if let Some(value) = names.consts.get(&field.ty.id) {
                used.insert((value.clone(), field_presence(field, requests.contains(&id))));
            }
        }
    }
    if used.is_empty() {
        return quote! {};
    }
    let values: BTreeSet<&String> = used.iter().map(|(value, _)| value).collect();
    let serializers = values.iter().map(|value| {
        let (_, ser) = const_helper_idents(names, value, false);
        let error = format!("expected the constant {value:?}");
        quote! {
            fn #ser<S, T>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
                T: Serialize + ConstField,
            {
                if value.matches_const(#value) {
                    value.serialize(serializer)
                } else {
                    Err(serde::ser::Error::custom(#error))
                }
            }
        }
    });
    let deserializers = used.iter().map(|(value, presence)| {
        let (de, _) = const_helper_idents(names, value, *presence);
        let error = format!("expected the constant {value:?}");
        let (output, read) = if *presence {
            (
                quote! { Option<T> },
                quote! { deserialize_request_field(deserializer)? },
            )
        } else {
            (quote! { T }, quote! { T::deserialize(deserializer)? })
        };
        quote! {
            fn #de<'de, D, T>(deserializer: D) -> Result<#output, D::Error>
            where
                D: serde::Deserializer<'de>,
                T: Deserialize<'de> + ConstField,
            {
                let value = #read;
                if value.matches_const(#value) {
                    Ok(value)
                } else {
                    Err(serde::de::Error::custom(#error))
                }
            }
        }
    });
    quote! {
        // A string constant field holds a plain `String`; these check it against the constant.
        // Absence and `null` stay governed by the field's own optionality and nullability.
        trait ConstField {
            fn matches_const(&self, expected: &str) -> bool;
        }
        impl ConstField for String {
            fn matches_const(&self, expected: &str) -> bool {
                self == expected
            }
        }
        impl<T: ConstField> ConstField for Option<T> {
            fn matches_const(&self, expected: &str) -> bool {
                match self {
                    Some(value) => value.matches_const(expected),
                    None => true,
                }
            }
        }
        #(#serializers)*
        #(#deserializers)*
    }
}

/// Statements normalizing the JSON value `value` (a `mut serde_json::Value`) for a union variant of
/// type `ty`, returning a deserialization error from the enclosing function when it is refused.
fn variant_normalizer(api: &Api, names: &Names, ty: Ty) -> TokenStream {
    let validation = super::normalize::Normalization { api, names };
    let normalize = validation.normalizer(
        Ty {
            nullable: false,
            ..ty
        },
        quote! { value },
        0,
    );
    if normalize.is_empty() {
        return quote! {};
    }
    quote! {
        let mut value = value;
        (|value: &mut serde_json::Value| -> Result<(), String> {
            if value.is_null() {
                return Ok(());
            }
            #normalize
            Ok(())
        })(&mut value)
        .map_err(serde::de::Error::custom)?;
    }
}

/// An `Option<#rust>` expression: the buffered `value` read as a trial-union variant of type `ty`,
/// normalized first, so a variant matches exactly when the value decodes as it. With `strict` (a
/// `bool` in scope) true, members and enum values the variant does not know count against it.
/// The constants a variant of a union declares: each required property of an object whose type
/// admits exactly one value (`const`, or an `enum` of one value), with that value. A variant that is
/// itself a union declares the constants all of its own variants share.
fn variant_constants(api: &Api, ty: Ty) -> Vec<(String, ScalarValue)> {
    variant_constants_within(api, ty, &mut BTreeSet::new())
}

fn variant_constants_within(
    api: &Api,
    ty: Ty,
    visiting: &mut BTreeSet<TypeId>,
) -> Vec<(String, ScalarValue)> {
    if !visiting.insert(ty.id) {
        return Vec::new();
    }
    let constants = match api.types.get(ty.id).map(|def| &def.kind) {
        Some(TypeKind::Struct(structure)) => structure
            .fields
            .iter()
            .filter(|field| field.required && !field.ty.nullable)
            .filter_map(
                |field| match api.types.get(field.ty.id).map(|def| &def.kind) {
                    Some(TypeKind::Enum(scalar)) if scalar.variants.len() == 1 => {
                        Some((field.name.wire.clone(), scalar.variants[0].clone()))
                    }
                    _ => None,
                },
            )
            .collect(),
        Some(TypeKind::Union(union)) => {
            let mut members = union
                .variants
                .iter()
                .map(|variant| variant_constants_within(api, variant.ty, visiting));
            let first = members.next().unwrap_or_default();
            members.fold(first, |shared, other| {
                shared
                    .into_iter()
                    .filter(|constant| other.contains(constant))
                    .collect()
            })
        }
        _ => Vec::new(),
    };
    visiting.remove(&ty.id);
    constants
}

/// A boolean expression over `value`: whether it carries every constant of the variant `ty`;
/// `None` for a variant without constants.
fn constants_carried(api: &Api, ty: Ty) -> Option<TokenStream> {
    let checks = variant_constants(api, ty)
        .into_iter()
        .map(|(wire, constant)| {
            let read = match constant {
                ScalarValue::String(text) => {
                    quote! { .and_then(serde_json::Value::as_str) == Some(#text) }
                }
                ScalarValue::Int(number) => {
                    quote! { .and_then(serde_json::Value::as_i64) == Some(#number) }
                }
                ScalarValue::Bool(flag) => {
                    quote! { .and_then(serde_json::Value::as_bool) == Some(#flag) }
                }
            };
            quote! { value.get(#wire) #read }
        })
        .collect::<Vec<_>>();
    (!checks.is_empty()).then(|| quote! { #(#checks)&&* })
}

fn variant_attempt(api: &Api, names: &Names, ty: Ty, rust: &TokenStream) -> TokenStream {
    let validation = super::normalize::Normalization { api, names };
    let normalize = validation.normalizer(
        Ty {
            nullable: false,
            ..ty
        },
        quote! { candidate },
        0,
    );
    let normalize = if normalize.is_empty() {
        quote! { let candidate = value.clone(); }
    } else {
        quote! {
            let mut candidate = value.clone();
            let normalized = (|candidate: &mut serde_json::Value| -> Result<(), String> {
                if candidate.is_null() {
                    return Ok(());
                }
                #normalize
                Ok(())
            })(&mut candidate);
            if normalized.is_err() {
                return None;
            }
        }
    };
    quote! {
        (|| -> Option<#rust> {
            #normalize
            if strict {
                super::support::strictly(|| serde_json::from_value::<#rust>(candidate)).ok()
            } else {
                serde_json::from_value::<#rust>(candidate).ok()
            }
        })()
    }
}

/// The variant holding an unlisted value of an open enum: `Unknown`, or the first free
/// `Unknown<n>` when the contract already lists a value named that way.
fn unknown_variant_ident<'a>(taken: impl Iterator<Item = &'a str>) -> proc_macro2::Ident {
    let taken: BTreeSet<&str> = taken.collect();
    let name = std::iter::once("Unknown".to_owned())
        .chain((2..).map(|index| format!("Unknown{index}")))
        .find(|name| !taken.contains(name.as_str()))
        .expect("a free name");
    format_ident!("{}", name)
}

/// The types an XML request or response body reaches.
fn xml_types(api: &Api) -> std::collections::HashSet<TypeId> {
    let mut stack: Vec<TypeId> = Vec::new();
    for operation in &api.operations {
        if let Some(body) = operation.request_body.as_ref() {
            if body.media == MediaType::Xml {
                stack.extend(body.ty.map(|ty| ty.id));
            }
        }
        for response in operation
            .responses
            .by_status
            .iter()
            .map(|(_, response)| response)
            .chain(operation.responses.default.as_ref())
        {
            if response.media == Some(MediaType::Xml) {
                stack.extend(response.body.map(|ty| ty.id));
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id) {
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
    seen
}

/// The deserializer that normalizes a field's JSON before decoding it, named by its content so equal
/// helpers (and the models that use them) share one item.
pub(crate) fn emit_field_helper(
    field: &Field,
    api: &Api,
    names: &Names,
    options: &CodegenOptions,
    request_model: bool,
) -> Option<(proc_macro2::Ident, TokenStream)> {
    if names.consts.contains_key(&field.ty.id) {
        return None;
    }
    let validation = super::normalize::Normalization { api, names };
    let normalize = validation.normalizer(
        Ty {
            nullable: false,
            ..field.ty
        },
        quote! { value },
        0,
    );
    if normalize.is_empty() {
        return None;
    }
    let ty = ty_tokens(field.ty, names, options, false);
    let (output, inner, wrap) = if field.required {
        (ty.clone(), ty, quote! {})
    } else if field_presence(field, request_model) {
        (quote! { Option<#ty> }, ty, quote! { .map(Some) })
    } else {
        (quote! { Option<#ty> }, quote! { Option<#ty> }, quote! {})
    };
    Some(normalizing_helper(output, inner, wrap, normalize))
}

/// The deserializer that normalizes the members a struct does not declare (its typed overflow map)
/// before decoding them, named by its content like [`emit_field_helper`].
pub(crate) fn emit_overflow_helper(
    item: Ty,
    api: &Api,
    names: &Names,
    options: &CodegenOptions,
) -> Option<(proc_macro2::Ident, TokenStream)> {
    let validation = super::normalize::Normalization { api, names };
    let normalize = validation.normalizer(item, quote! { v0 }, 1);
    if normalize.is_empty() {
        return None;
    }
    let ty = ty_tokens(item, names, options, false);
    let map = quote! { BTreeMap<String, #ty> };
    let normalize = quote! {
        if let serde_json::Value::Object(members) = &mut *value {
            for v0 in members.values_mut() {
                #normalize
            }
        }
    };
    Some(normalizing_helper(map.clone(), map, quote! {}, normalize))
}

/// A deserializer for `output` that normalizes the JSON with `normalize` (statements over
/// `value: &mut serde_json::Value`), decodes it as `inner` and applies `wrap`.
fn normalizing_helper(
    output: TokenStream,
    inner: TokenStream,
    wrap: TokenStream,
    normalize: TokenStream,
) -> (proc_macro2::Ident, TokenStream) {
    let key = {
        let text = format!("{output}|{inner}|{wrap}|{normalize}");
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for byte in text.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        hash
    };
    let helper = format_ident!("deserialize_{key:016x}");
    let tokens = quote! {
        fn #helper<'de, D>(deserializer: D) -> Result<#output, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            fn normalize(value: &mut serde_json::Value) -> Result<(), String> {
                if value.is_null() {
                    return Ok(());
                }
                #normalize
                Ok(())
            }
            super::support::deserialize_normalized::<D, #inner>(deserializer, normalize)#wrap
        }
    };
    (helper, tokens)
}

fn type_kind_tokens(
    kind: &TypeKind,
    _api: &Api,
    names: &Names,
    options: &CodegenOptions,
) -> TokenStream {
    match kind {
        TypeKind::Primitive(prim) => prim_tokens(*prim, options),
        TypeKind::Array(ty) => {
            let ty = ty_tokens(**ty, names, options, false);
            quote! { Vec<#ty> }
        }
        TypeKind::Tuple(items) => {
            let items = items.iter().map(|ty| ty_tokens(*ty, names, options, false));
            // A trailing comma keeps a one-element tuple a tuple.
            quote! { (#(#items,)*) }
        }
        TypeKind::Bytes => quote! { bytes::Bytes },
        TypeKind::Null => quote! { () },
        TypeKind::Any => quote! { serde_json::Value },
        TypeKind::Struct(_) | TypeKind::Enum(_) | TypeKind::Never | TypeKind::Union(_) => {
            unreachable!("named definitions emitted separately")
        }
    }
}

pub(crate) fn ty_tokens(
    ty: Ty,
    names: &Names,
    options: &CodegenOptions,
    qualified: bool,
) -> TokenStream {
    let mut tokens = match names.inline.get(&ty.id) {
        Some(kind) => inline_kind_tokens(kind, names, options, qualified),
        None => {
            let ident = names.types.get(&ty.id).expect("type name allocated");
            if qualified {
                quote! { types::#ident }
            } else {
                quote! { #ident }
            }
        }
    };
    if ty.boxed {
        tokens = quote! { Box<#tokens> };
    }
    if ty.nullable {
        tokens = quote! { Option<#tokens> };
    }
    tokens
}

/// A union's `Serialize` impl around its strategy-specific `body` (which writes to `serializer`).
/// With constraint checks, the body renders into a JSON value first, and the value must pass every
/// check before it is written.
fn union_serialize_impl(
    ident: &crate::name::Ident,
    body: TokenStream,
    constraint_checks: &[TokenStream],
) -> TokenStream {
    if constraint_checks.is_empty() {
        return quote! {
            impl serde::Serialize for #ident {
                fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                where
                    S: serde::Serializer,
                {
                    #body
                }
            }
        };
    }
    quote! {
        impl serde::Serialize for #ident {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                let rendered = (|| -> Result<serde_json::Value, serde_json::Error> {
                    #[allow(unused_variables)]
                    let serializer = serde_json::value::Serializer;
                    #body
                })();
                let value = rendered.map_err(serde::ser::Error::custom)?;
                let checked = (|| -> Result<(), serde_json::Error> {
                    #(#constraint_checks)*
                    Ok(())
                })();
                checked.map_err(serde::ser::Error::custom)?;
                value.serialize(serializer)
            }
        }
    }
}

/// The Rust spelling of an inline (unnamed structural) type. Element types recurse through
/// [`ty_tokens`] so a named element keeps its `types::` qualification at the crate root.
fn inline_kind_tokens(
    kind: &TypeKind,
    names: &Names,
    options: &CodegenOptions,
    qualified: bool,
) -> TokenStream {
    match kind {
        TypeKind::Primitive(prim) => prim_tokens(*prim, options),
        TypeKind::Array(item) => {
            let item = ty_tokens(**item, names, options, qualified);
            quote! { Vec<#item> }
        }
        TypeKind::Tuple(items) => {
            let items = items
                .iter()
                .map(|ty| ty_tokens(*ty, names, options, qualified));
            // A trailing comma keeps a one-element tuple a tuple.
            quote! { (#(#items,)*) }
        }
        TypeKind::Bytes => quote! { bytes::Bytes },
        TypeKind::Null => quote! { () },
        TypeKind::Any => quote! { serde_json::Value },
        TypeKind::Enum(enumeration) => match enumeration.repr {
            // Only a field-only string constant is inline; its field checks the value.
            ScalarRepr::String => quote! { String },
            ScalarRepr::Int => quote! { i64 },
            ScalarRepr::Bool => quote! { bool },
        },
        // A map-only object: exactly the overflow map a named wrapper struct would flatten.
        TypeKind::Struct(object) => match &object.additional {
            AdditionalProps::Typed(value) => {
                let value = ty_tokens(**value, names, options, qualified);
                quote! { std::collections::BTreeMap<String, #value> }
            }
            AdditionalProps::Allow | AdditionalProps::Deny => {
                unreachable!("only map-only objects are inline")
            }
        },
        TypeKind::Never | TypeKind::Union(_) => {
            unreachable!("nominal types are always named")
        }
    }
}

/// Union payloads are uniformly indirect so an API's largest object variant cannot inflate every
/// value of the enum (or trip strict `large_enum_variant` linting). Existing recursive boxing is a
/// boolean representation flag, so setting it again never produces `Box<Box<T>>`.
fn union_variant_ty_tokens(ty: Ty, names: &Names, options: &CodegenOptions) -> TokenStream {
    ty_tokens(Ty { boxed: true, ..ty }, names, options, false)
}

/// Multi-status response payloads are uniformly indirect for the same bounded-enum-size reason as
/// schema unions. Single-body response aliases remain allocation-free.
fn response_payload_ty_tokens(
    ty: Ty,
    names: &Names,
    options: &CodegenOptions,
    qualified: bool,
) -> TokenStream {
    ty_tokens(Ty { boxed: true, ..ty }, names, options, qualified)
}

fn prim_tokens(prim: Prim, options: &CodegenOptions) -> TokenStream {
    match prim {
        Prim::Bool => quote! { bool },
        Prim::String => quote! { String },
        Prim::I32 => quote! { i32 },
        Prim::I64 => quote! { i64 },
        Prim::F64 => quote! { f64 },
        Prim::Uuid if options.feature_uuid => quote! { uuid::Uuid },
        // The embedded newtypes, not `time`'s own types: OpenAPI fixes these to RFC 3339, which is
        // not what `time`'s `Serialize`/`Display` produce. Named bare so one spelling works at the
        // generated root (via the prelude re-export) and inside `types` (via its `use super::`).
        Prim::DateTime if options.feature_time => quote! { DateTime },
        Prim::Date if options.feature_time => quote! { Date },
        Prim::Uuid | Prim::DateTime | Prim::Date => quote! { String },
    }
}

fn success_type(operation: &Operation, names: &Names, options: &CodegenOptions) -> TokenStream {
    match operation.responses.success() {
        SuccessShape::Unit => quote! { () },
        SuccessShape::Plain(ty) => ty_tokens(ty, names, options, true),
        SuccessShape::Enum(_) => {
            let method_ident = names
                .operations
                .get(&operation.id)
                .expect("operation name allocated");
            let ident = success_enum_ident(method_ident);
            quote! { #ident }
        }
    }
}

/// The type name of an operation's multi-status success response enum, derived from the method name
/// (mirroring how the error enum is named `{Method}Error`).
fn success_enum_ident(method_ident: &crate::name::Ident) -> proc_macro2::Ident {
    format_ident!("{}Response", to_pascal(method_ident.as_str()))
}

/// The `PascalCase` variant identifier for a documented status selector: `Status200` for an exact
/// code, `Status2xx` for a range, and `Default` for the `default` response (carried as the
/// `Range(0)` sentinel by [`Responses::error`]). Deterministic and, within one enum, unique by
/// construction (each selector appears once). Routed through the `name` escaper for validity.
fn status_variant_ident(spec: crate::ir::StatusSpec) -> proc_macro2::Ident {
    let raw = match spec {
        crate::ir::StatusSpec::Exact(code) => format!("Status{code}"),
        crate::ir::StatusSpec::Range(0) => "Default".to_owned(),
        crate::ir::StatusSpec::Range(prefix) => format!("Status{prefix}xx"),
    };
    format_ident!(
        "{}",
        crate::name::escape(&raw, crate::name::IdentRole::Variant).as_str()
    )
}

/// The runtime [`support::StatusSpec`] tokens for a documented status selector. The `default`
/// sentinel (`Range(0)`) maps to `Any`, matching how the single-error-body path builds its table.
fn runtime_status_spec(spec: crate::ir::StatusSpec) -> TokenStream {
    match spec {
        crate::ir::StatusSpec::Exact(code) => quote! { support::StatusSpec::Exact(#code) },
        crate::ir::StatusSpec::Range(0) => quote! { support::StatusSpec::Any },
        crate::ir::StatusSpec::Range(prefix) => quote! { support::StatusSpec::Range(#prefix) },
    }
}

fn response_media_for_spec(
    responses: &crate::ir::Responses,
    spec: crate::ir::StatusSpec,
) -> Option<MediaType> {
    if spec == crate::ir::StatusSpec::Range(0) {
        return responses
            .default
            .as_ref()
            .and_then(|response| response.media);
    }
    responses
        .by_status
        .iter()
        .find(|(candidate, _)| *candidate == spec)
        .and_then(|(_, response)| response.media)
}

fn is_bytes_ty(api: &Api, ty: Ty) -> bool {
    matches!(
        api.types.get(ty.id).map(|definition| &definition.kind),
        Some(TypeKind::Bytes)
    )
}

fn reqwest_method(method: &crate::ir::Method) -> TokenStream {
    match method {
        crate::ir::Method::Get => quote! { reqwest::Method::GET },
        crate::ir::Method::Put => quote! { reqwest::Method::PUT },
        crate::ir::Method::Post => quote! { reqwest::Method::POST },
        crate::ir::Method::Delete => quote! { reqwest::Method::DELETE },
        crate::ir::Method::Options => quote! { reqwest::Method::OPTIONS },
        crate::ir::Method::Head => quote! { reqwest::Method::HEAD },
        crate::ir::Method::Patch => quote! { reqwest::Method::PATCH },
        crate::ir::Method::Trace => quote! { reqwest::Method::TRACE },
        // reqwest has no `QUERY` constant (OpenAPI 3.2's new fixed method), so build it from the
        // token bytes. `QUERY` is a valid HTTP method token, so `from_bytes` never fails here.
        crate::ir::Method::Query => quote! {
            reqwest::Method::from_bytes(b"QUERY").expect("QUERY is a valid HTTP method token")
        },
        crate::ir::Method::Custom(method) => {
            let bytes = proc_macro2::Literal::byte_string(method.as_bytes());
            quote! {
                reqwest::Method::from_bytes(#bytes)
                    .expect("validated additionalOperations key is a valid HTTP method token")
            }
        }
    }
}

fn to_pascal(value: &str) -> String {
    crate::name::to_pascal_case(value.trim_start_matches("r#"))
}
