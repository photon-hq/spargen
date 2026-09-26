use std::collections::{HashMap, HashSet};

use crate::diag::JsonPointer;

use super::{Ident, IdentRole};

/// A naming scope that allocates unique identifiers and resolves collisions deterministically.
///
/// On a clash, a stable disambiguator derived from the item's JSON Pointer is applied — being
/// order-independent, it stays deterministic under spec reordering. Injectivity within a
/// scope is a property-tested invariant.
///
/// A [`strict`](Scope::strict) scope still disambiguates (so allocation stays total), but records
/// every clash so the caller can refuse to emit a hash-suffixed public name.
#[derive(Debug, Default)]
pub struct Scope {
    used: HashSet<String>,
    /// The position that first claimed each allocated spelling, for collision reports.
    owners: HashMap<String, JsonPointer>,
    strict: bool,
    collisions: Vec<Collision>,
}

/// Two or more positions whose natural identifiers are the same spelling in one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    /// The contested identifier.
    pub ident: String,
    /// The position that kept the natural spelling (`None` when it was reserved rather than
    /// allocated).
    pub first: Option<JsonPointer>,
    /// The position that would have received a hash-suffixed spelling.
    pub other: JsonPointer,
}

impl Scope {
    /// A scope that records collisions instead of silently disambiguating them.
    pub fn strict(strict: bool) -> Self {
        Self {
            strict,
            ..Self::default()
        }
    }

    /// The collisions recorded by a strict scope, in allocation order.
    pub fn take_collisions(&mut self) -> Vec<Collision> {
        std::mem::take(&mut self.collisions)
    }

    /// Mark the escaped spelling of `hint` as occupied without disambiguating it.
    ///
    /// This is used when a binding is already part of an externally-derived surface and later
    /// generator-owned bindings must yield to it.
    pub fn reserve(&mut self, hint: &str, role: IdentRole) {
        let ident = super::escape(hint, role);
        self.used.insert(ident.as_str().to_owned());
    }

    /// Allocate a unique identifier for `hint` in `role`. If the cased/escaped name is already
    /// taken in this scope, `provenance` seeds a stable disambiguator.
    pub fn alloc(&mut self, hint: &str, role: IdentRole, provenance: &JsonPointer) -> Ident {
        self.alloc_at(hint, role, provenance, provenance)
    }

    /// [`alloc`](Self::alloc), reporting a collision at `position` rather than at `provenance`.
    /// `provenance` alone seeds the disambiguator, so a more precise report position never changes
    /// a generated spelling.
    pub fn alloc_at(
        &mut self,
        hint: &str,
        role: IdentRole,
        provenance: &JsonPointer,
        position: &JsonPointer,
    ) -> Ident {
        let base = super::escape(hint, role);
        if self.used.insert(base.as_str().to_owned()) {
            self.owners
                .insert(base.as_str().to_owned(), position.clone());
            return base;
        }

        if self.strict {
            self.collisions.push(Collision {
                ident: base.as_str().trim_start_matches("r#").to_owned(),
                first: self.owners.get(base.as_str()).cloned(),
                other: position.clone(),
            });
        }
        let raw_base = base.as_str().trim_start_matches("r#");
        let suffix = stable_suffix(provenance.as_str());
        let mut candidate = super::escape(&format!("{raw_base}_{suffix}"), role);
        let mut counter = 2usize;
        while !self.used.insert(candidate.as_str().to_owned()) {
            candidate = super::escape(&format!("{raw_base}_{suffix}_{counter}"), role);
            counter += 1;
        }
        candidate
    }
}

fn stable_suffix(input: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", hash as u32)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use crate::diag::JsonPointer;

    use super::{IdentRole, Scope};

    #[test]
    fn allocation_yields_to_reserved_identifier() {
        let pointer = JsonPointer::from("/paths/~1files/get");
        let mut scope = Scope::default();
        scope.reserve("path", IdentRole::Param);

        let allocated = scope.alloc("path", IdentRole::Param, &pointer);

        assert_ne!(allocated.as_str(), "path");
        assert!(allocated.as_str().starts_with("path_"));
    }

    #[test]
    fn strict_scope_records_collisions_and_stays_injective() {
        let mut scope = Scope::strict(true);
        let first = JsonPointer::from("/components/schemas/a_b");
        let second = JsonPointer::from("/components/schemas/a-b");
        let kept = scope.alloc("a_b", IdentRole::Type, &first);
        let other = scope.alloc("a-b", IdentRole::Type, &second);
        assert_eq!(kept.as_str(), "AB");
        assert_ne!(other.as_str(), "AB");
        assert_eq!(
            scope.take_collisions(),
            vec![super::Collision {
                ident: "AB".to_owned(),
                first: Some(first),
                other: second,
            }]
        );
    }

    #[test]
    fn lenient_scope_records_nothing() {
        let mut scope = Scope::default();
        let pointer = JsonPointer::from("/x");
        scope.alloc("a", IdentRole::Type, &pointer);
        scope.alloc("a", IdentRole::Type, &pointer);
        assert!(scope.take_collisions().is_empty());
    }

    proptest! {
        #[test]
        fn allocations_are_injective(hints in proptest::collection::vec("[A-Za-z0-9_ -]{0,24}", 1..64)) {
            let mut scope = Scope::default();
            let mut seen = std::collections::HashSet::new();
            for (index, hint) in hints.iter().enumerate() {
                let pointer = JsonPointer::root().push(&index.to_string());
                let ident = scope.alloc(hint, IdentRole::Field, &pointer);
                prop_assert!(seen.insert(ident.as_str().to_owned()));
            }
        }
    }
}
