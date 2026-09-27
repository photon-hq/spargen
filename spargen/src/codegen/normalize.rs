//! JSON a Rust type would refuse although the contract allows it.
//!
//! Before a value is decoded, its JSON is normalized for the target type: JSON Schema's `integer`
//! is any number with a zero fractional part, so integral floats (`1.0`) at integer positions are
//! rewritten to their integer spelling, which serde's integer types accept.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::ir::{AdditionalProps, Api, Prim, ScalarRepr, Ty, TypeKind};
use crate::name::Names;

pub(crate) struct Normalization<'a> {
    pub api: &'a Api,
    pub names: &'a Names,
}

thread_local! {
    /// The structural types whose normalization is being spelled out.
    static ACTIVE: std::cell::RefCell<Vec<crate::ir::TypeId>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Spell out normalization for `ty` unless it is already being spelled out further up. Named
/// models normalize themselves, so only a cycle made of structural types alone (an array of itself,
/// say) comes back here; such a type has no finite Rust spelling, so its repeat adds nothing.
fn guarded(ty: Ty, emit: impl FnOnce() -> TokenStream) -> TokenStream {
    // The nullable form only unwraps and comes back for the same type.
    if ty.nullable {
        return emit();
    }
    if ACTIVE.with(|active| active.borrow().contains(&ty.id)) {
        return TokenStream::new();
    }
    ACTIVE.with(|active| active.borrow_mut().push(ty.id));
    let tokens = emit();
    ACTIVE.with(|active| active.borrow_mut().pop());
    tokens
}

/// Whether a `date-time` type's `pattern` demands exactly three fractional digits, so it renders
/// as [`DateTime::to_millis_string`] rather than RFC 3339's shortest form.
pub(crate) fn renders_millis(def: &crate::ir::TypeDef) -> bool {
    matches!(def.kind, TypeKind::Primitive(Prim::DateTime))
        && def.constraints.iter().any(|constraints| {
            constraints
                .pattern
                .as_deref()
                .is_some_and(|pattern| pattern.contains(r"\.\d{3}"))
        })
}

impl Normalization<'_> {
    /// Statements normalizing a JSON value (`value: &mut serde_json::Value`) for deserialization
    /// into `ty` (spelled inside the `types` module). Empty when nothing applies.
    pub fn normalizer(&self, ty: Ty, value: TokenStream, depth: usize) -> TokenStream {
        guarded(ty, || self.normalizer_unguarded(ty, value, depth))
    }

    fn normalizer_unguarded(&self, ty: Ty, value: TokenStream, depth: usize) -> TokenStream {
        if ty.nullable {
            let inner = self.normalizer(
                Ty {
                    nullable: false,
                    ..ty
                },
                value.clone(),
                depth,
            );
            if inner.is_empty() {
                return quote! {};
            }
            return quote! {
                if !(#value).is_null() {
                    #inner
                }
            };
        }
        let Some(def) = self.api.types.get(ty.id) else {
            return quote! {};
        };
        let named = self.names.types.contains_key(&ty.id);
        let nominal = matches!(
            def.kind,
            TypeKind::Struct(_) | TypeKind::Union(_) | TypeKind::Never
        ) || matches!(&def.kind, TypeKind::Enum(enumeration) if enumeration.repr == ScalarRepr::String && !self.names.consts.contains_key(&ty.id));
        if named && nominal {
            // Named models normalize their own members as they decode.
            return quote! {};
        }
        match &def.kind {
            TypeKind::Primitive(Prim::I32 | Prim::I64) => {
                quote! { super::support::integral(#value); }
            }
            TypeKind::Enum(enumeration) if enumeration.repr == ScalarRepr::Int => {
                quote! { super::support::integral(#value); }
            }
            TypeKind::Array(item) => {
                let binding = format_ident!("v{}", depth);
                let inner = self.normalizer(**item, quote! { #binding }, depth + 1);
                if inner.is_empty() {
                    return quote! {};
                }
                quote! {
                    if let serde_json::Value::Array(items) = &mut *#value {
                        for #binding in items.iter_mut() {
                            #inner
                        }
                    }
                }
            }
            TypeKind::Tuple(items) => {
                let parts: Vec<TokenStream> = items
                    .iter()
                    .enumerate()
                    .filter_map(|(position, item)| {
                        let binding = format_ident!("v{}", depth);
                        let inner = self.normalizer(*item, quote! { #binding }, depth + 1);
                        (!inner.is_empty()).then(|| {
                            quote! {
                                if let Some(#binding) = items.get_mut(#position) {
                                    #inner
                                }
                            }
                        })
                    })
                    .collect();
                if parts.is_empty() {
                    return quote! {};
                }
                quote! {
                    if let serde_json::Value::Array(items) = &mut *#value {
                        #(#parts)*
                    }
                }
            }
            TypeKind::Struct(object) => match &object.additional {
                AdditionalProps::Typed(item) => {
                    let binding = format_ident!("v{}", depth);
                    let inner = self.normalizer(**item, quote! { #binding }, depth + 1);
                    if inner.is_empty() {
                        return quote! {};
                    }
                    quote! {
                        if let serde_json::Value::Object(members) = &mut *#value {
                            for #binding in members.values_mut() {
                                #inner
                            }
                        }
                    }
                }
                _ => quote! {},
            },
            _ => quote! {},
        }
    }
}
