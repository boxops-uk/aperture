//! **`fjord.db.List` and `fjord.db.Interning` — the server, answered as facts.**
//!
//! [Operations §5](../../../docs/fjord-cli-design.md) asks for enumeration to ride
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
//! resume cursor gains no case, [`enumerate`](fjord_engine::iter) is not touched, and
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
//! — reads *bytes*. So the listing is encoded through [`fjord_store::fact::encode`],
//! the same function a hand-written deriver writes a fact with, and each row is
//! `predicate_id ++ key`, byte for byte what a scan of a real keyspace produces. Sorted
//! by those bytes, which is the order a keyspace would have held them in, because the
//! codec is order-preserving ([I1](../../../docs/invariants.md#i1)). Nothing above this
//! module can tell the difference, which is the point: a virtual predicate that needed
//! special handling anywhere else would not be worth having.
//!
//! # Two predicates, one wrapper
//!
//! `fjord.db.List` is what the store root holds; `fjord.db.Interning` is what the
//! write path has done to it since this server opened it (`bench/FINDINGS.md` §15 is why
//! the second exists — it priced our interning and could not say whether the cache was
//! hitting). They are different questions with the same *answer shape*: a tuple this
//! module knows how to encode, in key order, that nothing downstream can tell from a
//! keyspace. So the wrapper holds a [`Table`] per declared predicate and picks by the id
//! leading a scan's `lo`, and adding a third would be a row type and one line here.
//!
//! A schema declaring neither has no catalogue at all, which is every schema but a
//! server's — a client's, a test's, and the copy embedded in a database.
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

use byteview::ByteView;
use fjord_encoding::tuple::Value;
use fjord_schema::{
    id::FactId,
    schema::{PREDICATE_ID_SIZE, PredicateId, Schema},
};
use fjord_store::{
    catalog::Listing,
    error::StoreError,
    fact::{self, Fact, ToValue, record},
    fact_store::{Entity, FactStore},
};

use crate::stats;

/// **The catalogue, as text** — the virtual predicates this server answers, declared in
/// the same language as everything else.
///
/// It lives here, in the crate that answers them, because a virtual predicate belongs to
/// whoever answers it: it travels with the *process* rather than with an artifact, is
/// absent from the handshake fingerprint, absent from the copy a database embeds at
/// create, and owns no keyspace. Until 0.0.1 this text was a `const` in the CLI and was
/// handed *down* to the server, which had it the wrong way round — the tool that hosts a
/// server should not be the thing that decides what the server can answer.
///
/// [`Schemas`](crate::registry::Schemas) appends it to every database's own schema.
/// Reserved names sort last ([`RESERVED_NAMESPACE`](fjord_schema::syntax::lower::RESERVED_NAMESPACE)),
/// so appending moves no stored id.
pub const SOURCE: &str = include_str!("../schemas/catalogue.sigla");

/// The predicate this module answers, by name.
///
/// Resolved through the schema rather than hardcoded as an id, because the id is a
/// position and the schema is what decides positions. A deployment whose schema does not
/// declare it simply has no catalogue, and [`materialise`] says so by answering `None`.
pub const PREDICATE: &str = "fjord.db.List";

/// The write path's counters, by name — see [`stats::InterningCounters`].
pub const INTERNING: &str = "fjord.db.Interning";

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

/// One database's interning counters, as the row a query sees.
///
/// The counters are `u64` where the schema says `int`, which is an `i64`. Saturating
/// rather than wrapping, and stated rather than cast: these are monotonic counts that
/// would need 9.2 quintillion interns to reach the ceiling, so the arm is unreachable —
/// but a silent wrap would report a busy server as one that had done nothing, which is
/// the one wrong answer a gauge must not give.
struct Interning {
    name: String,
    instance: String,
    hits: i64,
    misses: i64,
    keys: i64,
    entities: i64,
}

impl Interning {
    fn of(counters: &stats::InterningCounters) -> Interning {
        let saturating = |count: u64| i64::try_from(count).unwrap_or(i64::MAX);

        Interning {
            name: counters.name.clone(),
            instance: counters.instance.clone(),
            hits: saturating(counters.hits),
            misses: saturating(counters.misses),
            keys: saturating(counters.keys),
            entities: saturating(counters.entities),
        }
    }
}

impl Fact for Interning {
    const PREDICATE: &'static str = INTERNING;

    fn key(&self) -> Value {
        record([
            ("name", self.name.to_value()),
            ("instance", self.instance.to_value()),
            ("hits", self.hits.to_value()),
            ("misses", self.misses.to_value()),
            ("keys", self.keys.to_value()),
            ("entities", self.entities.to_value()),
        ])
    }

    fn value(&self) -> Option<Value> {
        None
    }
}

/// One virtual predicate's rows, encoded and in key order.
struct Table {
    predicate: PredicateId,
    /// `(predicate_id ++ key, id)` — a scan's rows, sorted as a keyspace holds them.
    rows: Arc<[(ByteView, FactId)]>,
}

/// Every virtual predicate this server answers, encoded and in key order.
pub struct Catalogue {
    tables: Box<[Table]>,
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
        interning: &[stats::InterningCounters],
    ) -> Result<Option<Catalogue>, StoreError> {
        let mut tables = Vec::with_capacity(2);

        if let Some(table) = Table::of(schema, Self::listing_rows(listing))? {
            tables.push(table);
        }

        if let Some(table) = Table::of(schema, interning.iter().map(Interning::of))? {
            tables.push(table);
        }

        if tables.is_empty() {
            return Ok(None);
        }

        Ok(Some(Catalogue {
            tables: tables.into_boxed_slice(),
        }))
    }

    /// The store root's entries as rows, ready to encode.
    fn listing_rows(listing: &Listing) -> impl Iterator<Item = Row> + '_ {
        listing.entries.iter().map(|entry| {
            let meta = &entry.meta;

            Row {
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
            }
        })
    }

    /// How many rows this catalogue holds, over every predicate. Tests, and nothing else.
    #[must_use]
    pub fn len(&self) -> usize {
        self.tables.iter().map(|table| table.rows.len()).sum()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The table a scan starting at `lo` reads, if any — decided by the predicate id
    /// leading every key.
    fn table_for(&self, lo: &[u8]) -> Option<&Table> {
        if lo.len() < PREDICATE_ID_SIZE {
            return None;
        }

        self.tables
            .iter()
            .find(|table| lo[..PREDICATE_ID_SIZE] == table.predicate.0.to_be_bytes())
    }

    /// The table holding `id`, if any.
    fn table_of(&self, id: FactId) -> Option<&Table> {
        self.tables
            .iter()
            .find(|table| table.predicate == id.predicate())
    }
}

impl Table {
    /// Encode `rows` against `schema`, or `None` if it does not declare their predicate.
    ///
    /// # Errors
    ///
    /// [`StoreError::Meta`] if the schema declares the predicate with a shape this module
    /// does not write — a field renamed on one side only. Reported rather than papered
    /// over, because the alternative is rows encoded to bytes no query can read.
    fn of<F: Fact>(
        schema: &Schema,
        rows: impl IntoIterator<Item = F>,
    ) -> Result<Option<Table>, StoreError> {
        let Some((predicate, _)) = schema.find_position(F::PREDICATE) else {
            return Ok(None);
        };

        let mut encoded: Vec<(ByteView, FactId)> = Vec::new();

        for (sequence, row) in rows.into_iter().enumerate() {
            let (id, key, _value) =
                fact::encode(schema, &row).map_err(|source| StoreError::Meta {
                    path: std::path::PathBuf::from(F::PREDICATE),
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
                    path: std::path::PathBuf::from(F::PREDICATE),
                    detail: format!("the catalogue cannot be given fact ids: {source}"),
                })?;

            encoded.push((ByteView::from(bytes), fact_id));
        }

        // Key order, because that is the order a keyspace would have held them in and
        // every seek downstream assumes it. The codec is order-preserving, so sorting
        // the encoded bytes *is* sorting by the tuple ([I1]).
        encoded.sort_by(|(a, _), (b, _)| a.cmp(b));

        Ok(Some(Table {
            predicate,
            rows: Arc::from(encoded),
        }))
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
        let Some(table) = self.catalogue.table_for(lo) else {
            return self.inner.scan(lo, hi).map(Scan::Stored);
        };

        // The same half-open range fjall is given, over the same bytes: `lo` inclusive,
        // `hi` exclusive, and no row from another predicate because the predicate id
        // leads every key.
        let rows: Vec<(ByteView, FactId)> = table
            .rows
            .iter()
            .filter(|(key, _)| key.as_ref() >= lo && hi.is_none_or(|hi| key.as_ref() < hi))
            .cloned()
            .collect();

        Ok(Scan::Listed(rows.into_iter()))
    }

    fn point(&self, id: FactId) -> Result<Option<Entity>, StoreError> {
        let Some(table) = self.catalogue.table_of(id) else {
            return self.inner.point(id);
        };

        // The key **without** its predicate prefix, which is what `entities` holds and
        // what a fetch splices a prefix back onto. Getting this the other way round
        // would read four bytes of predicate id as the first field of the key and
        // answer, silently, with nothing.
        Ok(table
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
pub fn reads(plan: &fjord_engine::plan::Plan, predicate: PredicateId) -> bool {
    use fjord_engine::plan::{Step, Test};

    plan.body.iter().any(|step| {
        let sources = match step {
            Step::Level(level) => &level.sources,
            Step::Test(Test::Absent(sources)) => sources,
            // Neither reads a predicate: a derive computes from registers, and a
            // computed comparison reads none at all.
            Step::Derive(_) | Step::Test(Test::Compare { .. }) => return false,
        };

        sources
            .iter()
            .any(|source| source.predicate_id() == predicate)
    })
}

#[cfg(test)]
mod tests {
    use fjord_schema::schema::{Predicate, PredicateTy};
    use fjord_store::{catalog::Entry, meta::Meta};
    use lasso::Rodeo;

    use super::*;

    /// A schema declaring the catalogue and nothing else.
    ///
    /// Stated here rather than imported from the CLI: this crate cannot see
    /// `code_index`, and that is the right way round — the server answers whatever
    /// schema it is given, and a test that borrowed the real one would be checking that
    /// two files agree rather than that this one works.
    pub(super) fn catalogue_schema() -> Schema {
        let mut rodeo = Rodeo::new();
        let mut sym = |name: &str| rodeo.get_or_intern(name);

        let listing = Predicate {
            name: sym(PREDICATE),
            key: PredicateTy::Record(Arc::from([
                (sym("name"), PredicateTy::Str),
                (sym("instance"), PredicateTy::Str),
                (sym("status"), PredicateTy::Str),
                (sym("facts"), PredicateTy::Int),
                (sym("bytes"), PredicateTy::Int),
                (sym("created"), PredicateTy::Str),
            ])),
            value: None,
        };

        // Both, because the wrapper picking the wrong table is the failure the second
        // predicate introduced, and a schema declaring one cannot exhibit it.
        let interning = Predicate {
            name: sym(INTERNING),
            key: PredicateTy::Record(Arc::from([
                (sym("name"), PredicateTy::Str),
                (sym("instance"), PredicateTy::Str),
                (sym("hits"), PredicateTy::Int),
                (sym("misses"), PredicateTy::Int),
                (sym("keys"), PredicateTy::Int),
                (sym("entities"), PredicateTy::Int),
            ])),
            value: None,
        };

        Schema::new(rodeo.into_reader(), Arc::from(vec![listing, interning]))
    }

    pub(super) fn counters_of(name: &str) -> stats::InterningCounters {
        stats::InterningCounters {
            name: name.to_owned(),
            instance: "01ABC".to_owned(),
            hits: 7,
            misses: 3,
            keys: 3,
            entities: 1,
        }
    }

    pub(super) fn listing_of(names: &[&str]) -> Listing {
        Listing {
            entries: names
                .iter()
                .map(|name| Entry {
                    meta: Meta::new(*name, "01ABC", 0),
                    path: std::path::PathBuf::from(name),
                })
                .collect(),
            problems: vec![],
        }
    }

    /// **`point` answers the key *without* its predicate prefix**, which is what
    /// `entities` holds and what a fetch splices a prefix back onto.
    ///
    /// Unit-tested because no query can reach it: nothing references the catalogue and
    /// it has no value side, so the arm exists for the day one of those changes. That
    /// makes it exactly the code most likely to be wrong when it is first needed — and
    /// the failure would not be an error but four bytes of predicate id read as the
    /// first field of a key, answering with nothing.
    #[test]
    fn a_point_read_answers_the_key_a_fetch_expects() {
        let schema = catalogue_schema();
        let catalogue = Catalogue::materialise(&schema, &listing_of(&["alpha", "beta"]), &[])
            .expect("it encodes")
            .expect("the schema declares it");

        assert_eq!(catalogue.len(), 2);

        let store = Catalogued::new(NoStore, Arc::new(catalogue));
        let rows: Vec<_> = store
            .scan(&PredicateId(0).0.to_be_bytes(), None)
            .expect("a scan")
            .collect::<Result<_, _>>()
            .expect("rows");

        assert_eq!(rows.len(), 2, "both databases");

        for (row, id) in rows {
            let entity = store
                .point(id)
                .expect("a point read")
                .expect("the id is one this listing handed out");

            assert_eq!(
                entity.key.as_ref(),
                &row[PREDICATE_ID_SIZE..],
                "the prefix belongs to the scan's row, never to the entity"
            );
            assert!(entity.value.is_empty(), "the catalogue has no value side");
        }
    }

    /// **Each predicate answers out of its own table**, which is what the second one put
    /// at risk: one wrapper, two ids, and a lookup by the bytes leading a scan.
    ///
    /// The failure this catches is not an error — it is a query about the databases
    /// answering with counters, or the other way round, because both encode to a tuple of
    /// two strings followed by integers and nothing downstream would object.
    #[test]
    fn a_scan_reads_the_table_its_predicate_names() {
        let schema = catalogue_schema();
        let catalogue = Catalogue::materialise(
            &schema,
            &listing_of(&["alpha", "beta", "gamma"]),
            &[counters_of("alpha"), counters_of("beta")],
        )
        .expect("it encodes")
        .expect("the schema declares both");

        assert_eq!(
            catalogue.len(),
            5,
            "three databases and two sets of counters"
        );

        let store = Catalogued::new(NoStore, Arc::new(catalogue));
        let scan = |predicate: u32| -> usize {
            store
                .scan(&PredicateId(predicate).0.to_be_bytes(), None)
                .expect("a scan")
                .count()
        };

        assert_eq!(scan(0), 3, "fjord.db.List answers the listing");
        assert_eq!(scan(1), 2, "fjord.db.Interning answers the counters");
    }

    /// A counter is a `u64` and the schema says `int`; the ceiling saturates rather than
    /// wrapping, because a wrapped count reports a busy server as an idle one.
    #[test]
    fn a_counter_past_the_signed_ceiling_saturates() {
        let row = Interning::of(&stats::InterningCounters {
            name: "alpha".to_owned(),
            instance: "01ABC".to_owned(),
            hits: u64::MAX,
            misses: 0,
            keys: 0,
            entities: 0,
        });

        assert_eq!(row.hits, i64::MAX);
    }

    /// A listing sorts by its encoded key, which is what every seek downstream assumes.
    #[test]
    fn rows_come_back_in_key_order() {
        let schema = catalogue_schema();
        let catalogue =
            Catalogue::materialise(&schema, &listing_of(&["zulu", "alpha", "mike"]), &[])
                .expect("it encodes")
                .expect("the schema declares it");

        let store = Catalogued::new(NoStore, Arc::new(catalogue));
        let rows: Vec<_> = store
            .scan(&PredicateId(0).0.to_be_bytes(), None)
            .expect("a scan")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows");

        let keys: Vec<&[u8]> = rows.iter().map(|(key, _)| key.as_ref()).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();

        assert_eq!(
            keys, sorted,
            "the listing is in key order, not listing order"
        );
    }

    /// A schema that does not declare the catalogue simply has none.
    #[test]
    fn a_schema_without_the_predicate_has_no_catalogue() {
        let mut rodeo = Rodeo::new();
        let name = rodeo.get_or_intern("src.File");
        let bare = Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![Predicate {
                name,
                key: PredicateTy::Str,
                value: None,
            }]),
        );

        assert!(
            Catalogue::materialise(&bare, &listing_of(&["alpha"]), &[])
                .expect("it does not fail")
                .is_none()
        );
    }

    /// A store that holds nothing, so a test can prove the wrapper answered rather than
    /// delegated.
    pub(super) struct NoStore;

    impl FactStore for NoStore {
        type Scan = std::vec::IntoIter<Result<(ByteView, FactId), StoreError>>;

        fn scan(&self, _lo: &[u8], _hi: Option<&[u8]>) -> Result<Self::Scan, StoreError> {
            panic!("the catalogue's own predicate must never reach the inner store")
        }

        fn point(&self, _id: FactId) -> Result<Option<Entity>, StoreError> {
            panic!("the catalogue's own ids must never reach the inner store")
        }
    }
}

#[cfg(test)]
mod ranges {
    use fjord_schema::schema::PredicateTy;

    use super::{tests::*, *};

    /// **The half-open range, isolated** — `lo` inclusive, `hi` exclusive.
    ///
    /// Unit-tested because a query cannot reach it: a string prefix compiles to a seek
    /// *and* a residual, so the residual re-checks whatever a broken range let through
    /// and the end-to-end answer stays right. Deleting the upper bound here is caught in
    /// one line; through a query it is caught nowhere.
    ///
    /// That makes this the one piece of `Catalogued` whose correctness rests entirely on
    /// a unit test, which is worth saying out loud rather than leaving to be discovered
    /// the first time something depends on the bound alone.
    #[test]
    fn a_scan_honours_both_ends_of_its_range() {
        let schema = catalogue_schema();
        let listing = listing_of(&["alpha", "code", "zulu"]);
        let catalogue = Catalogue::materialise(&schema, &listing, &[])
            .expect("it encodes")
            .expect("declared");

        let store = Catalogued::new(NoStore, Arc::new(catalogue));

        let key_of = |name: &str| {
            let mut bytes = PredicateId(0).0.to_be_bytes().to_vec();
            // The **leading key field alone**, encoded as the key holds it — which is
            // exactly what a seek prefix is, and why this is the right thing to bound a
            // range with.
            bytes.extend_from_slice(
                &fjord_encoding::tuple::encode_typed(&PredicateTy::Str, &Value::Str(name.into()))
                    .expect("a string encodes"),
            );
            bytes
        };

        let rows = |lo: Vec<u8>, hi: Option<Vec<u8>>| -> usize {
            store.scan(&lo, hi.as_deref()).expect("a scan").count()
        };

        assert_eq!(rows(PredicateId(0).0.to_be_bytes().to_vec(), None), 3);

        // `lo` is inclusive: the row it names is in.
        assert!(rows(key_of("code"), None) >= 1, "lo includes its own key");

        // `hi` is exclusive, and it is the end this exists to pin: `zulu` sorts after
        // `code` and must be left out.
        assert_eq!(
            rows(key_of("alpha"), Some(key_of("zulu"))),
            2,
            "alpha and code, and never zulu"
        );
        assert_eq!(
            rows(key_of("code"), Some(key_of("zulu"))),
            1,
            "code alone, bounded on both sides"
        );
        assert_eq!(
            rows(key_of("alpha"), Some(key_of("alpha"))),
            0,
            "an empty range is empty, rather than one row wide"
        );
    }
}
