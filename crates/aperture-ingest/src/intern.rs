//! Interning: a [`WireFact`] in, a [`FactId`] out, and the target facts its
//! references named written on the way.

use aperture_encoding::tuple::{Value, encode_key, encode_typed};
use aperture_schema::{
    id::FactId,
    schema::{PredicateId, PredicateTy, Schema},
};
use aperture_wire::{WireFact, WireRef, WireValue, block};

use crate::{error::IngestError, sink::FactSink};

/// What one ingest did.
///
/// The counts are over **every** fact the call touched, nested targets included, and
/// that is the number a write stream reports back: a producer sending a thousand
/// declarations that all name one file has written a thousand and one facts, not two
/// thousand, and the difference is what tells it interning is working.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ingested {
    /// Facts this call wrote.
    pub created: usize,
    /// Facts already present, so not written again — `ops-I5`'s silent dedup.
    pub deduped: usize,
    /// The id of each **top-level** fact, in the order they were given. Nested
    /// targets are not here: they are referenced, not ingested in their own right.
    pub ids: Vec<FactId>,
}

impl Ingested {
    /// Facts touched, written or not.
    #[must_use]
    pub fn seen(&self) -> usize {
        self.created + self.deduped
    }
}

/// Ingest one fact, interning every reference it carries.
///
/// # Errors
///
/// [`IngestError::TypeMismatch`] if the fact does not fit the schema,
/// [`IngestError::Conflict`] if it or one of its nested targets disagrees with a
/// fact already stored under that key, and [`IngestError::Store`] if the store
/// itself could not answer.
pub fn intern_fact<S: FactSink>(
    sink: &S,
    schema: &Schema,
    fact: &WireFact,
) -> Result<Ingested, IngestError> {
    let mut counts = Ingested::default();
    let id = intern_one(sink, schema, fact, &mut counts)?;
    counts.ids.push(id);
    Ok(counts)
}

/// Ingest a whole [block](aperture_wire::block) of facts.
///
/// The write stream's unit: a `CopyData` frame's payload is exactly this.
///
/// # Errors
///
/// [`IngestError::Wire`] if the block does not decode, and whatever
/// [`intern_fact`] reports for any fact in it.
pub fn intern_block<S: FactSink>(
    sink: &S,
    schema: &Schema,
    bytes: &[u8],
) -> Result<Ingested, IngestError> {
    let (facts, _) = block::decode_block(bytes, schema)?;

    let mut counts = Ingested::default();
    for fact in &facts {
        let id = intern_one(sink, schema, fact, &mut counts)?;
        counts.ids.push(id);
    }

    Ok(counts)
}

/// One fact: resolve its pieces, encode, resolve-or-create.
///
/// **The order in the body is the whole algorithm.** The key is resolved first, and
/// resolving it is what interns any nested target inside it — so by the time
/// `encode_key` runs, every reference in the key is an id and the key has bytes. A
/// parent cannot be written before its children because until they exist it has no
/// identity to be written under.
fn intern_one<S: FactSink>(
    sink: &S,
    schema: &Schema,
    fact: &WireFact,
    counts: &mut Ingested,
) -> Result<FactId, IngestError> {
    let declared = schema
        .get(fact.predicate)
        .ok_or(IngestError::UnknownPredicate(fact.predicate.0))?
        .predicate()
        .clone();

    let key = resolve(sink, schema, &declared.key, &fact.key, counts)?;
    let key_bytes = encode_key(&declared.key, &key).map_err(|_| IngestError::TypeMismatch {
        what: "a key",
        detail: "could not be encoded against its declared type",
    })?;

    // An absent value side is *no bytes*, matching what `fact::encode` writes for a
    // predicate without one — so a fact written by hand and the same fact ingested
    // dedup against each other rather than being two rows.
    let value_bytes = match (&declared.value, &fact.value) {
        (None, None) => Vec::new(),
        (Some(value_ty), Some(value)) => {
            let value = resolve(sink, schema, value_ty, value, counts)?;
            encode_typed(value_ty, &value).map_err(|_| IngestError::TypeMismatch {
                what: "a value side",
                detail: "could not be encoded against its declared type",
            })?
        }
        (Some(_), None) => {
            return Err(IngestError::TypeMismatch {
                what: "a fact",
                detail: "the predicate declares a value side and this fact has none",
            });
        }
        (None, Some(_)) => {
            return Err(IngestError::TypeMismatch {
                what: "a fact",
                detail: "the predicate declares no value side and this fact has one",
            });
        }
    };

    let interned = sink.resolve_or_create(fact.predicate, &key_bytes, &value_bytes)?;

    if interned.created {
        counts.created += 1;
    } else {
        counts.deduped += 1;
    }

    Ok(interned.id)
}

/// A wire value against its declared type, with every reference resolved to an id.
///
/// The recursion that makes the walk bottom-up: a `Fact`-typed position holding a
/// nested fact interns it *here*, before this value has been built, which is
/// necessarily before the fact holding this value can be encoded.
fn resolve<S: FactSink>(
    sink: &S,
    schema: &Schema,
    ty: &PredicateTy,
    value: &WireValue,
    counts: &mut Ingested,
) -> Result<Value, IngestError> {
    match (ty, value) {
        (PredicateTy::Int, WireValue::Int(n)) => Ok(Value::Int(*n)),
        (PredicateTy::Str, WireValue::Str(s)) => Ok(Value::Str(s.clone())),

        (PredicateTy::Fact(target), WireValue::Ref(reference)) => match reference {
            // A producer that already holds the id. Checked against the field's
            // declared target even though the wire decoder checks it too: a
            // `WireFact` can be built by hand — a deriver does — and the id's own
            // tag is what makes this free ([I11]).
            WireRef::Id(id) => {
                if id.predicate() != *target {
                    return Err(IngestError::TypeMismatch {
                        what: "a reference",
                        detail: "names a different predicate than the field declares",
                    });
                }
                Ok(Value::FactRef(*id))
            }

            WireRef::Nested(nested) => {
                if nested.predicate != *target {
                    return Err(IngestError::TypeMismatch {
                        what: "a nested fact",
                        detail: "is of a different predicate than the field declares",
                    });
                }
                Ok(Value::FactRef(intern_one(sink, schema, nested, counts)?))
            }
        },

        (PredicateTy::Record(field_tys), WireValue::Record(fields)) => {
            if field_tys.len() != fields.len() {
                return Err(IngestError::TypeMismatch {
                    what: "a record",
                    detail: "has a different number of fields than the schema declares",
                });
            }

            // The wire value is positional and a stored one is named, so the names
            // come from the schema here — the one place they are added back, and the
            // reason a wire record cannot be built with them in the wrong order.
            let mut out = Vec::with_capacity(fields.len());
            for ((name, field_ty), field) in field_tys.iter().zip(fields.iter()) {
                let name = schema
                    .interner()
                    .resolve(*name)
                    .ok_or(IngestError::TypeMismatch {
                        what: "a record field",
                        detail: "is named by a symbol this schema cannot resolve",
                    })?
                    .to_owned();

                out.push((name, resolve(sink, schema, field_ty, field, counts)?));
            }

            Ok(Value::Record(out.into()))
        }

        _ => Err(IngestError::TypeMismatch {
            what: "a value",
            detail: "does not fit the type the schema declares for it",
        }),
    }
}

/// The predicate a block declares, without decoding its facts.
///
/// # Errors
///
/// Whatever [`block::decode_header`] reports.
pub fn block_predicate(bytes: &[u8]) -> Result<PredicateId, IngestError> {
    Ok(block::decode_header(bytes)?.predicate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aperture_store::store::Interned;
    use aperture_wire::value::proptest::arb_schema_and_fact;
    use proptest::prelude::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// A model store that **records the order writes happen in**.
    ///
    /// The differential-oracle pattern the store batteries already use, for the one
    /// claim a real database cannot answer: `FjallDb` can say what ended up on disk,
    /// and only this can say a child was written before its parent. It implements the
    /// same three rules — allocate on a new key, return the id on an identical fact,
    /// reject a same-key-different-value — so a divergence between it and the real
    /// store is a divergence in the rules rather than in the recording.
    /// `(predicate, key bytes)` — what a `keys` row is keyed by, modelled.
    type Slot = (u32, Vec<u8>);
    /// The id assigned to a slot, and the value side stored under it.
    type Row = (FactId, Vec<u8>);

    #[derive(Default)]
    struct Recorder {
        rows: RefCell<HashMap<Slot, Row>>,
        next: RefCell<HashMap<u32, u64>>,
        /// Every write, in order.
        writes: RefCell<Vec<FactId>>,
    }

    impl Recorder {
        fn written(&self) -> Vec<FactId> {
            self.writes.borrow().clone()
        }
    }

    impl FactSink for Recorder {
        fn resolve_or_create(
            &self,
            predicate: PredicateId,
            key_fields: &[u8],
            value: &[u8],
        ) -> Result<Interned, IngestError> {
            let slot = (predicate.0, key_fields.to_vec());

            if let Some((id, stored)) = self.rows.borrow().get(&slot) {
                return if stored == value {
                    Ok(Interned {
                        id: *id,
                        created: false,
                    })
                } else {
                    Err(IngestError::Conflict {
                        predicate,
                        existing: *id,
                    })
                };
            }

            let mut next = self.next.borrow_mut();
            let sequence = next.entry(predicate.0).or_insert(1);
            let id = FactId::new(predicate, *sequence).expect("a model id");
            *sequence += 1;

            self.rows.borrow_mut().insert(slot, (id, value.to_vec()));
            self.writes.borrow_mut().push(id);

            Ok(Interned { id, created: true })
        }
    }

    /// A chain: `C` references `B` references `A`, each reference in a **key**.
    fn chain_schema() -> Schema {
        use aperture_schema::schema::Predicate;
        use lasso::Rodeo;
        use std::sync::Arc;

        let mut rodeo = Rodeo::new();
        let names: Vec<_> = ["gen.A", "gen.B", "gen.C"]
            .iter()
            .map(|n| rodeo.get_or_intern(*n))
            .collect();

        Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![
                Predicate {
                    name: names[0],
                    key: PredicateTy::Str,
                    value: None,
                },
                Predicate {
                    name: names[1],
                    key: PredicateTy::Fact(PredicateId(0)),
                    value: None,
                },
                Predicate {
                    name: names[2],
                    key: PredicateTy::Fact(PredicateId(1)),
                    value: None,
                },
            ]),
        )
    }

    /// **The walk is bottom-up, and the order is forced rather than chosen.**
    ///
    /// `C { B { A "leaf" } }` must be written A, B, C — because a parent's key holds
    /// its child's *id*, so until the child exists the parent has no bytes and
    /// therefore no identity to be written under. Asserted as an order, not as a set:
    /// the set would be identical under any order and would say nothing.
    #[test]
    fn a_parent_is_never_written_before_the_child_its_key_names() {
        let schema = chain_schema();
        let sink = Recorder::default();

        let deep = WireFact {
            predicate: PredicateId(2),
            key: WireValue::Ref(WireRef::Nested(Box::new(WireFact {
                predicate: PredicateId(1),
                key: WireValue::Ref(WireRef::Nested(Box::new(WireFact {
                    predicate: PredicateId(0),
                    key: WireValue::Str("leaf".to_owned()),
                    value: None,
                }))),
                value: None,
            }))),
            value: None,
        };

        let out = intern_fact(&sink, &schema, &deep).expect("it ingests");

        assert_eq!(out.created, 3);
        assert_eq!(
            sink.written()
                .iter()
                .map(|id| id.predicate().0)
                .collect::<Vec<_>>(),
            vec![0, 1, 2],
            "writes must run leaf-first"
        );
        assert_eq!(out.ids, vec![FactId::new(PredicateId(2), 1).unwrap()]);
    }

    /// **Interning is idempotent**, which is what makes a failed stream retryable:
    /// re-sending everything writes nothing new and answers the same ids.
    #[test]
    fn ingesting_twice_writes_nothing_the_second_time() {
        let schema = chain_schema();
        let sink = Recorder::default();

        let fact = WireFact {
            predicate: PredicateId(1),
            key: WireValue::Ref(WireRef::Nested(Box::new(WireFact {
                predicate: PredicateId(0),
                key: WireValue::Str("leaf".to_owned()),
                value: None,
            }))),
            value: None,
        };

        let first = intern_fact(&sink, &schema, &fact).expect("it ingests");
        let second = intern_fact(&sink, &schema, &fact).expect("again");

        assert_eq!(first.created, 2);
        assert_eq!(second.created, 0);
        assert_eq!(second.deduped, 2);
        assert_eq!(first.ids, second.ids);
        assert_eq!(sink.written().len(), 2, "nothing was written twice");
    }

    /// **A fact can contradict itself, and that is a conflict like any other.**
    ///
    /// Found by the property below rather than reasoned out, and worth a case of its
    /// own because it is the one way a *single* well-typed fact can be refused: a
    /// nested fact both names and **defines** its target, so naming one target twice
    /// with two different value sides is a producer disagreeing with itself inside
    /// one message.
    ///
    /// Rejected rather than resolved, and the reason is `ops-I4`: picking either
    /// occurrence would be order-dependent, which is the thing reproducibility
    /// forbids. Both orders reject, so the answer does not depend on the walk.
    #[test]
    fn a_fact_that_contradicts_itself_is_a_conflict() {
        use aperture_schema::schema::Predicate;
        use lasso::Rodeo;
        use std::sync::Arc;

        let mut rodeo = Rodeo::new();
        let (target, pair) = (
            rodeo.get_or_intern("gen.Target"),
            rodeo.get_or_intern("gen.Pair"),
        );
        let (left, right) = (rodeo.get_or_intern("l"), rodeo.get_or_intern("r"));

        let schema = Schema::new(
            rodeo.into_reader(),
            Arc::from(vec![
                Predicate {
                    name: target,
                    key: PredicateTy::Str,
                    // A value side is what lets two facts share a key and differ.
                    value: Some(PredicateTy::Int),
                },
                Predicate {
                    name: pair,
                    key: PredicateTy::Record(
                        vec![
                            (left, PredicateTy::Fact(PredicateId(0))),
                            (right, PredicateTy::Fact(PredicateId(0))),
                        ]
                        .into(),
                    ),
                    value: None,
                },
            ]),
        );

        let target_fact = |contents: i64| {
            WireRef::Nested(Box::new(WireFact {
                predicate: PredicateId(0),
                key: WireValue::Str("same".to_owned()),
                value: Some(WireValue::Int(contents)),
            }))
        };

        let contradictory = WireFact {
            predicate: PredicateId(1),
            key: WireValue::Record(
                vec![
                    WireValue::Ref(target_fact(1)),
                    WireValue::Ref(target_fact(2)),
                ]
                .into(),
            ),
            value: None,
        };

        assert!(matches!(
            intern_fact(&Recorder::default(), &schema, &contradictory),
            Err(IngestError::Conflict { .. })
        ));

        // Agreeing with itself is fine, and interns to one row named twice.
        let agreeing = WireFact {
            predicate: PredicateId(1),
            key: WireValue::Record(
                vec![
                    WireValue::Ref(target_fact(1)),
                    WireValue::Ref(target_fact(1)),
                ]
                .into(),
            ),
            value: None,
        };

        let out = intern_fact(&Recorder::default(), &schema, &agreeing).expect("it ingests");
        assert_eq!(out.created, 2, "the target once, and the pair");
        assert_eq!(out.deduped, 1, "the second mention found the first");
    }

    /// **The census.** The properties below tolerate a self-contradictory draw, so
    /// they would pass vacuously if *every* draw were one. Both outcomes have to be
    /// reached for them to be saying anything.
    #[test]
    fn the_generator_reaches_both_outcomes() {
        use proptest::{
            strategy::{Strategy, ValueTree},
            test_runner::TestRunner,
        };

        const RUNS: usize = 400;

        let mut runner = TestRunner::deterministic();
        let (mut ingested, mut conflicted, mut nested) = (0, 0, 0);

        for _ in 0..RUNS {
            let spec = arb_schema_and_fact()
                .new_tree(&mut runner)
                .unwrap()
                .current();
            let schema = spec.schema();
            let fact = spec.fact(&schema);
            let sink = Recorder::default();

            match intern_fact(&sink, &schema, &fact) {
                Ok(out) => {
                    ingested += 1;
                    // More facts written than the one given means a nested target was
                    // interned, which is the case all of this exists for.
                    nested += usize::from(out.created > 1);
                }
                Err(IngestError::Conflict { .. }) => conflicted += 1,
                Err(other) => panic!("a well-typed fact failed for another reason: {other}"),
            }
        }

        assert!(ingested * 4 > RUNS, "only {ingested}/{RUNS} facts ingested");
        assert!(conflicted > 0, "no draw contradicted itself");
        assert!(
            nested * 8 > RUNS,
            "only {nested}/{RUNS} draws interned a nested target"
        );
    }

    proptest! {
        /// **A well-typed fact ingests, or contradicts itself — and nothing else.**
        ///
        /// [Phase 7a's acceptance criterion](../../PLAN.md) — "interning is bottom-up
        /// and total on any well-typed nested value: no order in which a parent is
        /// written before the child its key holds". Stated as *no other failure mode*,
        /// because the criterion is about the walk and a conflict is about the
        /// producer.
        ///
        /// The generator draws schemas whose references point strictly backwards,
        /// which is the real constraint rather than a convenience: a reference in a
        /// key cannot be part of a cycle.
        #[test]
        fn a_well_typed_fact_ingests_or_conflicts_and_nothing_else(spec in arb_schema_and_fact()) {
            let schema = spec.schema();
            let fact = spec.fact(&schema);
            let sink = Recorder::default();

            match intern_fact(&sink, &schema, &fact) {
                Ok(out) => {
                    prop_assert_eq!(out.created, sink.written().len());
                    prop_assert_eq!(out.ids.len(), 1);
                }
                Err(IngestError::Conflict { .. }) => {}
                Err(other) => prop_assert!(false, "unexpected failure: {}", other),
            }
        }

        /// The same fact twice: nothing new, same ids, and every fact the first pass
        /// wrote is found rather than rewritten.
        #[test]
        fn ingesting_any_fact_twice_is_idempotent(spec in arb_schema_and_fact()) {
            let schema = spec.schema();
            let fact = spec.fact(&schema);
            let sink = Recorder::default();

            let Ok(first) = intern_fact(&sink, &schema, &fact) else {
                return Ok(());
            };
            let after_first = sink.written();
            let second = intern_fact(&sink, &schema, &fact).expect("what ingested once ingests twice");

            prop_assert_eq!(second.created, 0);
            prop_assert_eq!(second.deduped, first.seen());
            prop_assert_eq!(&second.ids, &first.ids);
            prop_assert_eq!(sink.written(), after_first);
        }

        /// **The outermost fact is written last**, because everything it names had to
        /// exist before it had a key. The generic form of the chain test above.
        #[test]
        fn the_outermost_fact_is_written_last(spec in arb_schema_and_fact()) {
            let schema = spec.schema();
            let fact = spec.fact(&schema);
            let sink = Recorder::default();

            let Ok(out) = intern_fact(&sink, &schema, &fact) else {
                return Ok(());
            };
            let written = sink.written();

            if let (Some(top), Some(last)) = (out.ids.first(), written.last()) {
                prop_assert_eq!(top, last, "the outermost fact must be written last");
            }
            prop_assert_eq!(written.len(), out.created);
        }
    }
}
