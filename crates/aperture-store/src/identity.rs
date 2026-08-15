//! **Content identity** — `hash(canonical schema, base facts)`, computed at `finish`.
//!
//! [`ops-I4`](../../../docs/aperture-cli-design.md) has asserted since before it was
//! computable that *"a DB built twice from identical inputs is identical"*, and that
//! identity is **always** the content hash. This is that hash.
//!
//! # Why it is computable at all
//!
//! It was not, until references stopped being ids on the way in. The clause reads
//! "hash the canonical schema and the base facts", and a base fact used to contain a
//! **physical `FactId`** — a number two reproducible builds can legitimately disagree
//! about, since it depends on the order writes happened to arrive in. Hashing it would
//! have made identity depend on nothing semantic.
//!
//! With [a reference sent as the target fact](../../../docs/open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline),
//! a database has a canonical **logical** form: expand every reference to the key of
//! the fact it names, recursively, and no physical id appears anywhere. That is what
//! is hashed.
//!
//! ```text
//!   stored     src.Decl { line = 12, module = #1:3, name = "key_of" }
//!   logical    src.Decl { line = 12,
//!                         module = src.Module { file = src.File "keys.py",
//!                                               name = "keys" },
//!                         name = "key_of" }
//! ```
//!
//! # Order-independent by construction, not by argument
//!
//! Each fact is hashed on its own and the results are **summed**. Nothing depends on
//! the order facts are visited in, which matters more than it first appears: the
//! obvious alternative — walk each predicate's `keys` tree in order and hash
//! sequentially — is *not* order-independent, because that tree is sorted by the
//! **physical** key, and a key holding a reference sorts by the target's id. Two
//! databases with identical logical content and different id assignment would walk
//! their facts in different logical orders and hash differently.
//!
//! Summing sidesteps that entirely. It makes the hash a **multiset** hash: it sees
//! which facts exist and how many, and nothing about sequence. That is exactly the
//! property `ops-I4` wants, and it is a property of the construction rather than of an
//! argument about tree ordering that a future change could invalidate.
//!
//! # What this is not
//!
//! FNV-1a and a wrapping sum: a check that two builds produced the same content, not a
//! security boundary. Anyone who can write facts can obviously choose what the hash
//! is. Accidental collision is what it defends against, and a 64-bit digest is
//! proportionate to that.

use aperture_encoding::tuple::{Value, decode_key};
use aperture_schema::{
    id::FactId,
    schema::{LocalInterner, PredicateTy, Schema},
};

use crate::{error::StoreError, fact_store::FactStore, store::FjallDb};

/// How deep a chain of references may be expanded before it is called a fault.
///
/// A reference in a *key* cannot be part of a cycle — the target must be fully
/// identified before the referring key has any bytes at all — so a well-formed
/// database cannot reach this. It exists because a **corrupt** one could, and a data
/// path answers with an error rather than a stack overflow.
pub const MAX_REFERENCE_DEPTH: usize = 64;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;

/// Tags, so that structurally different values cannot fold to the same bytes — an int
/// and a one-field record of that int, say.
const TAG_INT: u8 = 1;
const TAG_STR: u8 = 2;
const TAG_RECORD: u8 = 3;
const TAG_REFERENCE: u8 = 4;
const TAG_NULL: u8 = 5;
const TAG_VALUE_SIDE: u8 = 6;
const TAG_NO_VALUE_SIDE: u8 = 7;

/// What a database's content came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    /// `hash(canonical schema, base facts)`.
    pub fingerprint: u64,
    /// Facts counted on the way — free, since the walk visits every one.
    pub facts: u64,
}

/// Compute a database's content identity.
///
/// # Errors
///
/// [`StoreError`] if the store cannot be read, a key cannot be decoded, or a reference
/// chain runs past [`MAX_REFERENCE_DEPTH`] — all of which mean a database that is not
/// what it says it is, and none of which should be sealed over.
pub fn compute(
    db: &FjallDb,
    schema: &Schema,
    schema_fingerprint: u64,
) -> Result<Identity, StoreError> {
    let reader = db.reader();
    let interner = LocalInterner::new(schema.interner().clone());

    let mut sum: u64 = 0;
    let mut facts: u64 = 0;

    for predicate in db.predicate_ids() {
        let Some(declared) = schema.get(predicate) else {
            // A predicate with trees but no declaration: the schema and the data
            // disagree, and sealing that would record an identity for content this
            // build cannot describe.
            return Err(StoreError::Meta {
                path: std::path::PathBuf::from(format!("predicate {}", predicate.0)),
                detail: "the database holds a predicate the schema does not declare".to_owned(),
            });
        };

        let key_ty = declared.predicate().key.clone();
        let value_ty = declared.predicate().value.clone();

        for row in reader.scan(&predicate.0.to_be_bytes(), None)? {
            let (_row, id) = row?;

            let entity = reader.point(id)?.ok_or(StoreError::DanglingFactId(id))?;

            let mut hash = FNV_OFFSET;

            // The predicate is part of a fact's identity: the same key under two
            // predicates is two facts.
            feed(&mut hash, &predicate.0.to_le_bytes());

            let key = decode_key(&interner, &entity.key, &key_ty)?;
            feed_value(&mut hash, &reader, schema, &interner, &key, 0)?;

            match &value_ty {
                Some(ty) => {
                    feed(&mut hash, &[TAG_VALUE_SIDE]);
                    let value =
                        aperture_encoding::tuple::decode_typed(&interner, &entity.value, ty)?;
                    feed_value(&mut hash, &reader, schema, &interner, &value, 0)?;
                }
                None => feed(&mut hash, &[TAG_NO_VALUE_SIDE]),
            }

            // Summed rather than chained — see the module docs. Wrapping is the
            // point: the combination has to be associative *and* commutative.
            sum = sum.wrapping_add(hash);
            facts += 1;
        }
    }

    // The schema and the fact count go in last. The count is not redundant: it
    // separates two different multisets whose per-fact hashes happen to sum alike,
    // which a sum alone cannot.
    let mut fingerprint = FNV_OFFSET;
    feed(&mut fingerprint, &schema_fingerprint.to_le_bytes());
    feed(&mut fingerprint, &sum.to_le_bytes());
    feed(&mut fingerprint, &facts.to_le_bytes());

    Ok(Identity { fingerprint, facts })
}

/// Fold a value into `hash`, expanding every reference to the fact it names.
fn feed_value<S: FactStore>(
    hash: &mut u64,
    store: &S,
    schema: &Schema,
    interner: &LocalInterner,
    value: &Value,
    depth: usize,
) -> Result<(), StoreError> {
    match value {
        Value::Int(n) => {
            feed(hash, &[TAG_INT]);
            feed(hash, &n.to_le_bytes());
        }

        Value::Str(text) => {
            feed(hash, &[TAG_STR]);
            // The length is fed too, so `"ab" ++ "c"` and `"a" ++ "bc"` cannot fold
            // to the same bytes in a record.
            feed(hash, &(text.len() as u64).to_le_bytes());
            feed(hash, text.as_bytes());
        }

        Value::Null => feed(hash, &[TAG_NULL]),

        Value::Record(fields) => {
            feed(hash, &[TAG_RECORD]);
            feed(hash, &(fields.len() as u64).to_le_bytes());

            // Positional: the field *names* are pinned by the schema fingerprint,
            // which is folded in separately, so hashing them here would be paying
            // twice for one guarantee.
            for (_name, field) in fields.iter() {
                feed_value(hash, store, schema, interner, field, depth)?;
            }
        }

        // **The expansion.** A reference contributes the *logical* key of the fact it
        // names, never its id — which is the whole reason this hash means anything
        // across two independent builds.
        Value::FactRef(id) => {
            feed(hash, &[TAG_REFERENCE]);
            feed_reference(hash, store, schema, interner, *id, depth + 1)?;
        }
    }

    Ok(())
}

fn feed_reference<S: FactStore>(
    hash: &mut u64,
    store: &S,
    schema: &Schema,
    interner: &LocalInterner,
    id: FactId,
    depth: usize,
) -> Result<(), StoreError> {
    if depth > MAX_REFERENCE_DEPTH {
        return Err(StoreError::Meta {
            path: std::path::PathBuf::from(format!("fact {id:?}")),
            detail: format!(
                "a reference chain ran past {MAX_REFERENCE_DEPTH} hops, which a \
                 well-formed database cannot do"
            ),
        });
    }

    let predicate = id.predicate();

    let declared = schema.get(predicate).ok_or_else(|| StoreError::Meta {
        path: std::path::PathBuf::from(format!("predicate {}", predicate.0)),
        detail: "a reference names a predicate the schema does not declare".to_owned(),
    })?;
    let key_ty: PredicateTy = declared.predicate().key.clone();

    let entity = store.point(id)?.ok_or(StoreError::DanglingFactId(id))?;

    // The target's predicate is part of what the reference *means*: two facts with
    // the same key under different predicates are different targets.
    feed(hash, &predicate.0.to_le_bytes());

    let key = decode_key(interner, &entity.key, &key_ty)?;
    feed_value(hash, store, schema, interner, &key, depth)
}

#[inline]
fn feed(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash ^= u64::from(byte);
        *hash = hash.wrapping_mul(FNV_PRIME);
    }
}

/// Bytes on disk under `path`, recursively.
///
/// Descriptive (`ops-I4`) and so best-effort: a file that vanishes between the listing
/// and the stat is skipped rather than failing a seal over a number nobody computes
/// anything from.
#[must_use]
pub fn directory_size(path: &std::path::Path) -> u64 {
    let Ok(listing) = std::fs::read_dir(path) else {
        return 0;
    };

    listing
        .filter_map(Result::ok)
        .map(|entry| match entry.file_type() {
            Ok(kind) if kind.is_dir() => directory_size(&entry.path()),
            Ok(_) => entry.metadata().map(|meta| meta.len()).unwrap_or(0),
            Err(_) => 0,
        })
        .sum()
}
