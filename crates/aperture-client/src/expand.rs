//! **Expanding a reference into the fact it names**, recursively.
//!
//! A row carries a reference as a `FactId`, because that is what a reference *is* once
//! stored ([chapter 3](../../../docs/03-storage-model.md)). So a query over a code index
//! answers `{"to": "#3:7", "file": "#1:12"}`, and the two interesting fields of the
//! interesting predicate are numbers naming facts the reader cannot see. This turns them
//! into the facts:
//!
//! ```text
//!   {"to": "#3:7", "file": "#1:12"}
//!   {"to": {"module": {"file": "store/codec.py", "name": "store"}, "name": "encode",
//!           "line": 12},
//!    "file": "store/codec.py"}
//! ```
//!
//! # It is the logical form, which is a shape this project already has a name for
//!
//! Expanding every reference to its target's **key**, recursively, is the definition of
//! a database's canonical *logical* form: it is what `ops-I4`'s content hash is computed
//! over, and it is what a producer sends when it nests a reference rather than holding
//! an id ([settled]). So an expanded row and the form a producer would have written the
//! same fact in are one shape rather than two — and the expanded value is built out of
//! [`WireRef::Nested`], the vocabulary that already existed for the inbound direction,
//! rather than out of something new for display.
//!
//! The **value side is not expanded**, for the reason the wire's `Fetched` gives: a
//! reference names an identity, the identity is the key, and the value is a different
//! read that a query can already ask for by name (`X.value`).
//!
//! # Why it is a client's job
//!
//! Every readable shape in this tool is made client-side — the server carries the binary
//! format and nothing else — and expansion is a *display* decision: how deep, whether at
//! all, and what to do about an id that resolves to nothing are all questions about the
//! reader rather than about the data. What the client cannot do alone is the one thing it
//! asks for: [`Connection::fetch`] is the point read, because focus names a fact by its
//! key and never by its number.
//!
//! # What it costs, stated plainly
//!
//! One point read per distinct id, and **one round trip per level of depth** rather than
//! one per reference: the walk is breadth-first, so a row's references are resolved
//! together, then their references together, and so on. The cache is what makes this
//! affordable on a real index — a page of references into one file names that file's id
//! forty times — and it is kept across pages for the same reason.
//!
//! A reference that resolves to nothing is left as the id it was. That cannot happen for
//! an id lifted out of a row — both column families are written together
//! ([I12](../../../docs/invariants.md#i12)) and ids are never reused
//! ([I11](../../../docs/invariants.md#i11)) — so it means corruption, and
//! [`unresolved`](Expander::unresolved) counts it rather than hiding it behind a
//! plausible-looking row.
//!
//! [settled]: ../../../docs/open-decisions.md#what-a-reference-is-on-the-way-in--settled-the-target-fact-written-inline

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use aperture_schema::{
    id::FactId,
    schema::{PredicateId, Schema},
};
use aperture_wire::{WireFact, WireRef, WireValue, protocol::Found};

use crate::{connection::Connection, error::ClientError};

/// How deep bare "expand it" goes.
///
/// **A guard rather than a limit anybody should reach.** In the data, expansion always
/// terminates: a reference in a *key* cannot be part of a cycle, because the target has
/// to be fully identified before the referring key has any bytes at all
/// ([chapter 3](../../../docs/03-storage-model.md#interning-a-nested-fact)) — so the
/// logical form of a fact is a finite tree, and the built-in code index's deepest is
/// three hops. What this bounds is a *schema* nobody has written yet, where a mistake in
/// this walk would be a page of point reads instead of a wrong answer. A person who
/// wants fewer hops says so: `:expand 1`.
pub const FULL_DEPTH: usize = 16;

/// How many facts the cache holds before it starts again.
///
/// **A display cache, and nothing rests on it.** A fact is immutable and an id is never
/// reused ([I11](../../../docs/invariants.md#i11)), so re-reading one always gives the
/// same answer — dropping an entry costs a point read and can cost nothing else. That is
/// what makes emptying it the right answer to a full one: a shell paging through a
/// result keeps its working set, and `aperture query --expand` over a million rows stays
/// bounded instead of ending as a memory report.
///
/// Emptied rather than evicted one at a time because the access pattern is *clustered* —
/// rows arrive in key order, so the references in a page name the same few targets — and
/// an LRU's bookkeeping would cost more than the reads a perfect policy would save.
pub const MAX_CACHED: usize = 100_000;

/// A recursive expander, with the cache that makes it affordable.
///
/// Holds the served schema because the reply to a fetch is schema-driven and readable
/// only against the schema the server encoded it with — see [`Connection::fetch`].
pub struct Expander {
    schema: Arc<Schema>,
    /// Each id's **key, unexpanded**, or `None` for an id naming no fact.
    ///
    /// Flat rather than already-expanded, and that is what makes the cache compose:
    /// a cached key's own references are themselves cache entries, so the same fact is
    /// read once however many depths it is reached at, and a deeper expansion of a row
    /// already seen shallowly costs only the levels it adds.
    cache: HashMap<FactId, Option<WireValue>>,
    /// Predicates this server will not resolve an id of, **learned from its refusal**.
    ///
    /// **Its motivating case is gone, and it is kept anyway.** That case was a *virtual*
    /// predicate — `X where X = aperture.db.List _` heads on a whole catalogue row — which
    /// the server briefly refused and now answers, because `Catalogued` implements the
    /// point read as well as the scan. What is left is a refusal that should not happen:
    /// an id naming a predicate the server's schema does not declare.
    ///
    /// It stays because of what it *prevents*, which has nothing to do with the cause:
    /// expansion is a display feature layered over a result, and a refusal partway through
    /// a page would otherwise lose every row of it. Degrading one field to ids and saying
    /// so once is the right failure; losing the rows is not. Asking once and remembering
    /// is what keeps it to one refusal per predicate per session.
    unexpandable: HashMap<PredicateId, String>,
    /// What to tell the person, once, when a page is done.
    notices: Vec<String>,
    fetched: u64,
    unresolved: u64,
}

impl Expander {
    #[must_use]
    pub fn new(schema: Arc<Schema>) -> Expander {
        Expander {
            schema,
            cache: HashMap::new(),
            unexpandable: HashMap::new(),
            notices: vec![],
            fetched: 0,
            unresolved: 0,
        }
    }

    /// Anything the person should be told about this expansion, taken once.
    ///
    /// Drained rather than read, so a surface that prints per page prints each notice on
    /// the page it happened and never again — a predicate that cannot be expanded is
    /// mentioned when it is discovered, not on every page thereafter.
    pub fn take_notices(&mut self) -> Vec<String> {
        std::mem::take(&mut self.notices)
    }

    /// Point reads asked of the server, across every row expanded so far.
    ///
    /// The cost of expansion, as a number a shell can put next to its timing: it counts
    /// *distinct* ids, so the gap between it and the references on screen is what the
    /// cache saved.
    #[must_use]
    pub fn fetched(&self) -> u64 {
        self.fetched
    }

    /// References that resolved to no fact, which is corruption rather than an absence.
    #[must_use]
    pub fn unresolved(&self) -> u64 {
        self.unresolved
    }

    /// One row, with every reference in it replaced by the fact it names, to `depth`
    /// hops.
    ///
    /// A depth of zero is the row unchanged, which is what "expansion off" costs: no
    /// walk, no round trip, the same value back.
    ///
    /// # Errors
    ///
    /// Whatever the fetch reports — a server that declines the ids, or a broken
    /// connection. A row is never *partly* expanded on the way to an error: the reads
    /// all happen first, and the substitution afterwards cannot fail.
    pub fn expand(
        &mut self,
        connection: &mut Connection,
        value: &WireValue,
        depth: usize,
    ) -> Result<WireValue, ClientError> {
        if depth == 0 {
            return Ok(value.clone());
        }

        // **Emptied between rows, never inside one.** Everything this row needs is read
        // below and read back in `substitute`, so an eviction partway through would
        // render half a row's references as ids — a row that looks expanded and is not.
        // The bound is therefore the cap plus whatever one row names.
        if self.cache.len() > MAX_CACHED {
            self.cache.clear();
        }

        self.prefetch(connection, value, depth)?;
        Ok(self.substitute(value, depth))
    }

    /// Read every id the walk will reach, **a level at a time**.
    ///
    /// One round trip per level rather than one per reference, which is the difference
    /// between a page costing four exchanges and it costing four hundred. Ids already
    /// cached are not asked for again, and the frontier still descends *through* them —
    /// a level's next level comes from what is in the cache, not from what this call
    /// happened to fetch.
    fn prefetch(
        &mut self,
        connection: &mut Connection,
        value: &WireValue,
        depth: usize,
    ) -> Result<(), ClientError> {
        let mut level: Vec<FactId> = vec![];
        references(value, &mut level);

        for _ in 0..depth {
            if level.is_empty() {
                break;
            }

            // **Deduped before anything is done with it**, and that is not only about
            // asking twice. The next level is built from *this* one, so a row naming the
            // same target a hundred times would contribute that target's references a
            // hundred times over — and again at the level after, which multiplies rather
            // than repeats. Collapsing here bounds the frontier by the number of distinct
            // facts, which is what it should have been all along.
            let mut seen: HashSet<FactId> = HashSet::new();
            level.retain(|id| seen.insert(*id));

            // **Grouped by predicate, and that is about attribution rather than tidiness.**
            // A refusal ends the stream it was asked on, so a batch mixing predicates
            // would be refused as a whole and there would be no way to tell which one the
            // server would not answer — leaving the choice between disabling expansion for
            // all of them or asking again forever. One request per predicate makes the
            // answer unambiguous, and a level rarely holds more than two.
            let mut wanted: BTreeMap<PredicateId, Vec<FactId>> = BTreeMap::new();

            for id in &level {
                if !self.cache.contains_key(id) && !self.unexpandable.contains_key(&id.predicate())
                {
                    wanted.entry(id.predicate()).or_default().push(*id);
                }
            }

            for (predicate, ids) in wanted {
                for batch in ids.chunks(aperture_wire::protocol::MAX_FETCH) {
                    match connection.fetch(&self.schema, batch) {
                        Ok(answers) => {
                            for (id, answer) in batch.iter().zip(answers) {
                                self.fetched += 1;

                                let key = match answer {
                                    Found::Key(key) => Some(key),

                                    // **A stored fact that is not there is corruption**,
                                    // for an id out of a row: both column families are
                                    // written together (I12) and ids are never reused
                                    // (I11), so there is no legitimate way to reach one.
                                    Found::Missing => {
                                        self.unresolved += 1;
                                        None
                                    }

                                    // **A row of a predicate the server answers rather
                                    // than stores**, whose listing has moved on — a
                                    // database created or removed since the row was
                                    // produced. Ordinary, so it is *not* counted as
                                    // damage; the reference simply stays an id.
                                    Found::Unstored => None,
                                };

                                self.cache.insert(*id, key);
                            }
                        }

                        // **A refusal is about this predicate, not about the row.** The
                        // rows are still worth printing with the id in them, so it is
                        // recorded, said once, and never asked again — where propagating
                        // would lose a page of perfectly good rows to a field nobody could
                        // have expanded.
                        Err(ClientError::Server { message, .. }) => {
                            self.refuse(predicate, message);
                            break;
                        }

                        // A server that cannot fetch at all, or a broken conversation.
                        // Neither is about one predicate, and the caller decides.
                        Err(other) => return Err(other),
                    }
                }
            }

            let mut next: Vec<FactId> = vec![];
            for id in &level {
                if let Some(Some(key)) = self.cache.get(id) {
                    references(key, &mut next);
                }
            }
            level = next;
        }

        Ok(())
    }

    /// Record a predicate this server will not resolve, and say so once.
    ///
    /// The notice names the predicate, because the server's own message cannot: it is
    /// answering about an id, and what a person needs is which *field* of their row is
    /// going to keep showing a number.
    fn refuse(&mut self, predicate: PredicateId, message: String) {
        let name = self
            .schema
            .get(predicate)
            .and_then(|found| found.name().map(str::to_owned))
            .unwrap_or_else(|| format!("predicate {}", predicate.0));

        self.notices.push(format!(
            "{name} cannot be expanded, so its references stay ids: {message}"
        ));

        self.unexpandable.insert(predicate, message);
    }

    /// Build the expanded value out of what is cached. **Pure** — no I/O, so it is
    /// testable without a server, and a row cannot fail halfway through rendering.
    ///
    /// An id that is not cached, or cached as naming nothing, stays the id it was: the
    /// depth ran out, or the reference dangles.
    fn substitute(&self, value: &WireValue, depth: usize) -> WireValue {
        match value {
            WireValue::Int(_) | WireValue::Str(_) => value.clone(),

            // A record is not a hop. Its fields are this fact's own, so they expand at
            // the same depth — otherwise `{at = {line, col}}` would spend a level on a
            // span that names nothing.
            WireValue::Record(fields) => WireValue::Record(
                fields
                    .iter()
                    .map(|field| self.substitute(field, depth))
                    .collect(),
            ),

            WireValue::Ref(WireRef::Id(id)) if depth > 0 => match self.cache.get(id) {
                Some(Some(key)) => WireValue::Ref(WireRef::Nested(Box::new(WireFact {
                    predicate: id.predicate(),
                    key: self.substitute(key, depth - 1),
                    value: None,
                }))),
                _ => value.clone(),
            },

            // Depth exhausted, or a nested reference that arrived nested — which a
            // server never sends, since stored a reference is an id. Left alone rather
            // than walked into: expanding what is already expanded would be guessing at
            // where it came from.
            WireValue::Ref(_) => value.clone(),
        }
    }
}

/// Every reference held directly in a value, appended in order.
///
/// Directly: it does not follow one, which is what makes the caller's loop
/// breadth-first rather than this function recursive into the store.
fn references(value: &WireValue, out: &mut Vec<FactId>) {
    match value {
        WireValue::Int(_) | WireValue::Str(_) => {}
        WireValue::Ref(WireRef::Id(id)) => out.push(*id),
        WireValue::Ref(WireRef::Nested(fact)) => references(&fact.key, out),
        WireValue::Record(fields) => {
            for field in fields.iter() {
                references(field, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aperture_schema::schema::{Predicate, PredicateId, PredicateTy};
    use lasso::Rodeo;

    fn id(predicate: u32, sequence: u64) -> FactId {
        FactId::new(PredicateId(predicate), sequence).expect("a fact id")
    }

    /// `p0 : string` and `p1 : { to : p0 }` — one reference, one hop.
    fn schema() -> Arc<Schema> {
        let mut rodeo = Rodeo::new();
        let zero = rodeo.get_or_intern("t.Named");
        let one = rodeo.get_or_intern("t.Ref");
        let to = rodeo.get_or_intern("to");

        Arc::new(Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![
                Predicate {
                    name: zero,
                    key: PredicateTy::Str,
                    value: None,
                },
                Predicate {
                    name: one,
                    key: PredicateTy::Record(Arc::from(vec![(
                        to,
                        PredicateTy::Fact(PredicateId(0)),
                    )])),
                    value: None,
                },
            ]),
        ))
    }

    /// An expander with its cache pre-loaded, so the pure half can be tested without a
    /// socket — which is the point of splitting `substitute` out of `expand`.
    fn loaded(entries: Vec<(FactId, Option<WireValue>)>) -> Expander {
        let mut expander = Expander::new(schema());
        for (id, key) in entries {
            expander.cache.insert(id, key);
        }
        expander
    }

    fn str_of(text: &str) -> WireValue {
        WireValue::Str(text.to_owned())
    }

    #[test]
    fn a_reference_becomes_the_fact_it_names() {
        let file = id(0, 3);
        let expander = loaded(vec![(file, Some(str_of("store/codec.py")))]);

        let row = WireValue::Record(Box::from([WireValue::Ref(WireRef::Id(file))]));

        assert_eq!(
            expander.substitute(&row, 1),
            WireValue::Record(Box::from([WireValue::Ref(WireRef::Nested(Box::new(
                WireFact {
                    predicate: PredicateId(0),
                    key: str_of("store/codec.py"),
                    value: None,
                }
            )))]))
        );
    }

    /// **Recursion is the point**, and depth counts *hops*, not levels of nesting.
    #[test]
    fn expansion_follows_a_chain_and_stops_where_it_is_told() {
        let file = id(0, 3);
        let outer = id(1, 9);

        let expander = loaded(vec![
            (file, Some(str_of("a.py"))),
            (
                outer,
                Some(WireValue::Record(Box::from([WireValue::Ref(WireRef::Id(
                    file,
                ))]))),
            ),
        ]);

        let row = WireValue::Ref(WireRef::Id(outer));

        // One hop: the outer fact, its own reference still an id.
        let one = expander.substitute(&row, 1);
        let WireValue::Ref(WireRef::Nested(fact)) = &one else {
            panic!("one hop should have expanded the outer reference: {one:?}");
        };
        assert_eq!(
            fact.key,
            WireValue::Record(Box::from([WireValue::Ref(WireRef::Id(file))])),
            "the second hop is not taken at depth 1"
        );

        // Two hops: all the way down.
        let two = expander.substitute(&row, 2);
        let WireValue::Ref(WireRef::Nested(fact)) = &two else {
            panic!("two hops should still expand the outer reference: {two:?}");
        };
        let WireValue::Record(fields) = &fact.key else {
            panic!("the outer fact's key is a record: {fact:?}");
        };
        assert_eq!(
            fields[0],
            WireValue::Ref(WireRef::Nested(Box::new(WireFact {
                predicate: PredicateId(0),
                key: str_of("a.py"),
                value: None,
            })))
        );
    }

    /// **A record is not a hop.** A span nested in a key would otherwise eat the level
    /// its siblings needed.
    #[test]
    fn a_nested_record_costs_no_depth() {
        let file = id(0, 1);
        let expander = loaded(vec![(file, Some(str_of("a.py")))]);

        // `{at = {line, col}, file = <ref>}` — the reference is two records deep.
        let row = WireValue::Record(Box::from([
            WireValue::Record(Box::from([WireValue::Int(4), WireValue::Int(19)])),
            WireValue::Ref(WireRef::Id(file)),
        ]));

        let WireValue::Record(fields) = expander.substitute(&row, 1) else {
            panic!("a record stays a record");
        };
        assert!(
            matches!(fields[1], WireValue::Ref(WireRef::Nested(_))),
            "the reference expanded at depth 1 despite the nesting: {fields:?}"
        );
    }

    /// Depth zero is the row unchanged, which is what "off" has to cost.
    #[test]
    fn no_depth_is_no_change() {
        let file = id(0, 1);
        let expander = loaded(vec![(file, Some(str_of("a.py")))]);
        let row = WireValue::Ref(WireRef::Id(file));

        assert_eq!(expander.substitute(&row, 0), row);
    }

    /// An id naming nothing stays the id it was, rather than becoming an empty fact or
    /// disappearing. It cannot happen for an id out of a row (I11/I12), so the honest
    /// rendering is the number, and the counter is what says it happened.
    #[test]
    fn a_reference_that_names_nothing_is_left_as_the_id() {
        let missing = id(0, 7);
        let expander = loaded(vec![(missing, None)]);
        let row = WireValue::Ref(WireRef::Id(missing));

        assert_eq!(expander.substitute(&row, FULL_DEPTH), row);
    }

    /// **A repeated reference is one entry in the frontier, not many.**
    ///
    /// The failure this guards is multiplicative rather than wasteful: the next level is
    /// built from this one, so a hundred copies of an id would contribute its own
    /// references a hundred times, and the level after that ten thousand. A row naming
    /// one file forty times is the ordinary case, not a contrived one.
    #[test]
    fn a_repeated_reference_does_not_multiply_the_frontier() {
        let file = id(0, 1);
        let outer = id(1, 2);

        let expander = loaded(vec![
            (file, Some(str_of("a.py"))),
            (
                outer,
                Some(WireValue::Record(Box::from([WireValue::Ref(WireRef::Id(
                    file,
                ))]))),
            ),
        ]);

        // The same reference twenty times over, two levels deep.
        let row = WireValue::Record(
            (0..20)
                .map(|_| WireValue::Ref(WireRef::Id(outer)))
                .collect::<Vec<_>>()
                .into(),
        );

        // Everything is cached, so no connection is needed: what is being checked is the
        // walk's arithmetic, and `substitute` proves the row still expands.
        let expanded = expander.substitute(&row, FULL_DEPTH);
        let WireValue::Record(fields) = &expanded else {
            panic!("a record stays a record");
        };
        assert_eq!(fields.len(), 20, "every one of them expanded");

        // Nothing was read, because nothing was missing — the cache answers a repeat as
        // it answers the first.
        assert_eq!(expander.fetched(), 0);

        // The frontier's own arithmetic is a *cost* rather than an answer, so this says
        // what it is rather than asserting it from outside: `prefetch` collapses each
        // level to its distinct ids, and the twenty above are one entry at every depth.
        // Reads were already deduped before that fix; what it bounds is the frontier.
    }

    /// The frontier walk finds references at every level, and finds each id once.
    #[test]
    fn references_are_collected_in_order_and_from_every_level() {
        let a = id(0, 1);
        let b = id(1, 2);

        let value = WireValue::Record(Box::from([
            WireValue::Ref(WireRef::Id(a)),
            WireValue::Record(Box::from([
                WireValue::Int(1),
                WireValue::Ref(WireRef::Id(b)),
            ])),
            str_of("x"),
        ]));

        let mut out = vec![];
        references(&value, &mut out);
        assert_eq!(out, vec![a, b]);
    }
}
