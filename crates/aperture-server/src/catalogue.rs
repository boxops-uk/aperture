//! **`aperture.db.List` — the store root, answered as facts.**
//!
//! [Operations §5](../../../docs/aperture-cli-design.md) asks for enumeration to ride
//! the query machinery rather than a control message, and this is that: `\l` is a query,
//! with a plan, a seek, residuals, a profile and a resume cursor, exactly as a query over
//! `src.File` is. What it buys is not tidiness — it is that filtering, joining and paging
//! all work the first time somebody wants them, instead of being three features a bespoke
//! `LIST` frame would have to grow one at a time.
//!
//! # Where this sits, and why it is not a new kind of `Source`
//!
//! The obvious place to put a virtual predicate is the executor: a `Source::Virtual`
//! beside `Seek` and `Fetch`. It is the wrong place, and the reason is the shape of the
//! seam that already exists. [`FactStore`] is *the* answer to "where do rows come from" —
//! two methods, `scan` and `point` — and the executor is generic over it. Answering a
//! predicate from memory is a different answer to that same question, not a different
//! question.
//!
//! Putting it here rather than in the machine means the plan IR gains no variant, the
//! resume cursor gains no case, [`enumerate`](aperture_engine::iter) is not touched, and
//! [I4](../../../docs/invariants.md#i4) needs no re-proving — the resume battery is
//! already written over an arbitrary `FactStore`, so a store that happens to hold its
//! rows in a `Vec` is a store it already covers. Against that, the IR does not *name*
//! virtual sources, so `:plan` shows a scan of predicate 22 and says nothing about where
//! its rows live. That is the trade, and it is the one the house rules ask for: do not
//! reshape the machine for an additive feature.
//!
//! # What makes the rows indistinguishable from stored ones
//!
//! Everything downstream — registers, residuals, field offsets, projections, the cursor
//! — reads *bytes*. So the listing is encoded through [`aperture_store::fact::encode`],
//! the same function a hand-written deriver writes a fact with, and each row is
//! `predicate_id ++ key`, byte for byte what a scan of a real keyspace produces. Sorted
//! by those bytes, which is the order a keyspace would have held them in, because the
//! codec is order-preserving ([I1](../../../docs/invariants.md#i1)). Nothing above this
//! module can tell the difference, which is the point: a virtual predicate that needed
//! special handling anywhere else would not be worth having.
//!
//! # One listing per query, and that is [I8](../../../docs/invariants.md#i8)'s shape
//!
//! The rows are materialised once, when the query is prepared, and the same `Arc` is
//! shared by every chunk of that query — so a `create` between two pages of `\more` is
//! invisible to the result in flight, exactly as a write to a keyspace is invisible to a
//! snapshot taken before it. Resume then means what it always means: the same data, read
//! from where the cursor says. A *new* query sees a fresh listing, which is the same
//! promise a new snapshot gives.

use std::sync::Arc;

use aperture_encoding::tuple::Value;
use aperture_schema::{
    id::FactId,
    schema::{PREDICATE_ID_SIZE, PredicateId, Schema},
};
use aperture_store::{
    catalog::Listing,
    error::StoreError,
    fact::{self, Fact, ToValue, record},
    fact_store::{Entity, FactStore},
};
use byteview::ByteView;

/// The predicate this module answers, by name.
///
/// Resolved through the schema rather than hardcoded as an id, because the id is a
/// position and the schema is what decides positions. A deployment whose schema does not
/// declare it simply has no catalogue, and [`materialise`] says so by answering `None`.
pub const PREDICATE: &str = "aperture.db.List";

/// One database, as the row a query sees.
///
/// **The field names are stated here and in the schema, independently**, which is the
/// same arrangement the .NET client has with the built-in schema and exists for the same
/// reason: [`fact::encode`] resolves each name against the schema and fails loudly on a
/// mismatch, so the two cannot drift into silently encoding a different tuple.
struct Row {
    name: String,
    instance: String,
    status: String,
    facts: i64,
    bytes: i64,
    created: String,
}

impl Fact for Row {
    const PREDICATE: &'static str = PREDICATE;

    fn key(&self) -> Value {
        record([
            ("name", self.name.to_value()),
            ("instance", self.instance.to_value()),
            ("status", self.status.to_value()),
            ("facts", self.facts.to_value()),
            ("bytes", self.bytes.to_value()),
            ("created", self.created.to_value()),
        ])
    }

    fn value(&self) -> Option<Value> {
        None
    }
}

/// The listing, encoded and in key order.
pub struct Catalogue {
    predicate: PredicateId,
    /// `(predicate_id ++ key, id)` — a scan's rows, sorted as a keyspace holds them.
    rows: Arc<[(ByteView, FactId)]>,
}

impl Catalogue {
    /// Encode a listing against `schema`.
    ///
    /// Answers `None` when the schema declares no catalogue, which is every schema but
    /// a server's — a client's, a test's, and the copy embedded in a database.
    ///
    /// # Errors
    ///
    /// [`StoreError::Meta`] if the schema declares the predicate with a shape this
    /// module does not write — a field renamed on one side only. Reported rather than
    /// papered over, because the alternative is a listing that encodes to bytes no query
    /// can read.
    pub fn materialise(
        schema: &Schema,
        listing: &Listing,
    ) -> Result<Option<Catalogue>, StoreError> {
        let Some((predicate, _)) = schema.find_position(PREDICATE) else {
            return Ok(None);
        };

        let mut rows: Vec<(ByteView, FactId)> = Vec::with_capacity(listing.entries.len());

        for (sequence, entry) in listing.entries.iter().enumerate() {
            let meta = &entry.meta;

            let row = Row {
                name: meta.name.clone(),
                instance: meta.instance.clone(),
                status: meta.status.to_string(),
                // Absent until `finish` counts them, and absent is **-1** rather than
                // 0: a writable database with no facts and one whose facts have not
                // been counted are different situations, and a query that cannot tell
                // them apart would report the second as the first.
                facts: meta.facts.map_or(-1, |facts| facts as i64),
                bytes: meta.bytes.map_or(-1, |bytes| bytes as i64),
                created: meta.created_at_ms.to_string(),
            };

            let (id, key, _value) =
                fact::encode(schema, &row).map_err(|source| StoreError::Meta {
                    path: std::path::PathBuf::from(PREDICATE),
                    detail: format!("the catalogue does not match its declaration: {source}"),
                })?;

            debug_assert_eq!(id, predicate, "find_position and encode agree on the id");

            let mut bytes = Vec::with_capacity(PREDICATE_ID_SIZE + key.len());
            bytes.extend_from_slice(&predicate.0.to_be_bytes());
            bytes.extend_from_slice(&key);

            // Sequences from 1, as a real allocator hands them out, so nothing
            // downstream meets a fact id shaped differently from every other.
            let fact_id =
                FactId::new(predicate, sequence as u64 + 1).map_err(|source| StoreError::Meta {
                    path: std::path::PathBuf::from(PREDICATE),
                    detail: format!("the catalogue cannot be given fact ids: {source}"),
                })?;

            rows.push((ByteView::from(bytes), fact_id));
        }

        // Key order, because that is the order a keyspace would have held them in and
        // every seek downstream assumes it. The codec is order-preserving, so sorting
        // the encoded bytes *is* sorting by the tuple ([I1]).
        rows.sort_by(|(a, _), (b, _)| a.cmp(b));

        Ok(Some(Catalogue {
            predicate,
            rows: Arc::from(rows),
        }))
    }

    /// How many databases the listing held. Used by tests, and by nothing else.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn names(&self, lo: &[u8]) -> bool {
        lo.len() >= PREDICATE_ID_SIZE && lo[..PREDICATE_ID_SIZE] == self.predicate.0.to_be_bytes()
    }
}

/// A store that answers the catalogue from memory and everything else from `inner`.
pub struct Catalogued<S> {
    inner: S,
    catalogue: Arc<Catalogue>,
}

impl<S: FactStore> Catalogued<S> {
    pub fn new(inner: S, catalogue: Arc<Catalogue>) -> Catalogued<S> {
        Catalogued { inner, catalogue }
    }
}

/// One scan type for both, so the executor's loop stays one loop.
pub enum Scan<I> {
    Stored(I),
    Listed(std::vec::IntoIter<(ByteView, FactId)>),
}

impl<I: Iterator<Item = Result<(ByteView, FactId), StoreError>>> Iterator for Scan<I> {
    type Item = Result<(ByteView, FactId), StoreError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Scan::Stored(scan) => scan.next(),
            Scan::Listed(rows) => rows.next().map(Ok),
        }
    }
}

impl<S: FactStore> FactStore for Catalogued<S> {
    type Scan = Scan<S::Scan>;

    fn scan(&self, lo: &[u8], hi: Option<&[u8]>) -> Result<Self::Scan, StoreError> {
        if !self.catalogue.names(lo) {
            return self.inner.scan(lo, hi).map(Scan::Stored);
        }

        // The same half-open range fjall is given, over the same bytes: `lo` inclusive,
        // `hi` exclusive, and no row from another predicate because the predicate id
        // leads every key.
        let rows: Vec<(ByteView, FactId)> = self
            .catalogue
            .rows
            .iter()
            .filter(|(key, _)| key.as_ref() >= lo && hi.is_none_or(|hi| key.as_ref() < hi))
            .cloned()
            .collect();

        Ok(Scan::Listed(rows.into_iter()))
    }

    fn point(&self, id: FactId) -> Result<Option<Entity>, StoreError> {
        if id.predicate() != self.catalogue.predicate {
            return self.inner.point(id);
        }

        // The key **without** its predicate prefix, which is what `entities` holds and
        // what a fetch splices a prefix back onto. Getting this the other way round
        // would read four bytes of predicate id as the first field of the key and
        // answer, silently, with nothing.
        Ok(self
            .catalogue
            .rows
            .iter()
            .find(|(_, row_id)| *row_id == id)
            .map(|(key, _)| Entity {
                key: ByteView::from(&key[PREDICATE_ID_SIZE..]),
                value: ByteView::from(&[][..]),
            }))
    }
}

/// Whether this plan reads `predicate` anywhere — a level's alternatives, or a
/// negation's.
///
/// **Asked so the listing is materialised only when it is wanted.** Building it walks
/// the store root and reads a sidecar per database, which is `ops-I7` working exactly as
/// designed and still far too much to do on every query about `src.File`. A plan names
/// its predicates, so the cheap question is answerable before the expensive work.
#[must_use]
pub fn reads(plan: &aperture_engine::plan::Plan, predicate: PredicateId) -> bool {
    use aperture_engine::plan::{Step, Test};

    plan.body.iter().any(|step| {
        let sources = match step {
            Step::Level(level) => &level.sources,
            Step::Test(Test::Absent(sources)) => sources,
            Step::Derive(_) => return false,
        };

        sources
            .iter()
            .any(|source| source.predicate_id() == predicate)
    })
}
