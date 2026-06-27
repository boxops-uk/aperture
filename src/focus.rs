// use std::{fmt, marker::PhantomData, mem::ManuallyDrop};

// use byteview::ByteView;
// use lasso::{Rodeo, RodeoReader, Spur};
// use std::{ops::Range, sync::Arc};
// use thiserror::Error;
// use tinyvec::TinyVec;

pub mod emit;
pub mod error;
pub mod exec;
pub mod plan;
pub mod schema;
pub mod transport;

// #[derive(Debug, Clone, Copy, PartialEq, Eq)]
// pub struct PredicateId(u32);

// #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
// pub struct FieldIdx(u16);

// #[derive(Clone, Copy, PartialEq, Eq)]
// pub struct NodeId(u32);

// #[derive(Debug, Clone, Copy, PartialEq, Eq)]
// pub struct VarId(u32);

// #[derive(Clone, Copy, PartialEq, Eq)]
// pub struct TyVarId(u32);

// pub type Span = Range<u32>;

// #[derive(Debug, Clone, Copy)]
// pub enum Symbol {
//     Schema(Spur),
//     Local(Spur),
// }

// #[derive(Clone, Copy)]
// pub enum Literal {
//     Int(i64),
//     Str(Symbol),
// }

// #[derive(Clone, Copy)]
// pub enum ScalarTy {
//     Int,
//     Str,
// }

// #[derive(Debug)]
// pub enum FactSource {
//     Var(VarId),
//     Field(Box<FactSource>, FieldIdx),
// }

// pub enum Project {
//     Lit(Literal),
//     SlotField(VarId, FieldIdx, ScalarTy),
//     FactId(FactSource),
//     FactField(FactSource, FieldIdx, ScalarTy),
//     Value(FactSource),
//     Record(Box<[Project]>),
// }

// #[derive(Debug)]
// pub enum Access {
//     Scan(PredicateId, SeekKey),
//     Fetch(PredicateId, FactSource),
// }

// #[derive(Debug)]
// pub enum SeekKey {
//     Prefix(Box<[u8]>),
//     Composite(Box<[SeekKeyPart]>),
// }

// #[derive(Debug)]
// pub enum SeekKeyPart {
//     Bytes(Box<[u8]>),
//     SlotField(VarId, FieldIdx),
// }

// #[derive(Debug)]
// pub enum ResidualOp {
//     EqConst(Box<[u8]>),
//     EqSlotField(VarId, FieldIdx),
//     Prefix(Box<[u8]>),
// }

// #[derive(Debug)]
// pub struct Binding(FieldIdx, VarId);

// #[derive(Debug)]
// pub struct Residual(FieldIdx, ResidualOp);

// #[derive(Debug)]
// pub struct PlanStmt {
//     out: Option<VarId>,
//     access: Access,
//     bindings: Box<[Binding]>,
//     residuals: Box<[Residual]>,
//     max_field: FieldIdx,
// }

// pub struct Plan {
//     nvars: u32,
//     body: Box<[PlanStmt]>,
//     head: Project,
//     output_ty: OutputTy,
// }

// #[derive(Debug, Clone)]
// pub enum OutputTy {
//     Int,
//     String,
//     FactId,
//     Bytes,
//     Record(Box<[(Symbol, OutputTy)]>),
// }

// pub struct Is<A, B>(PhantomData<(A, B)>);

// impl<A, B> Is<A, B> {
//     fn refl() -> Self {
//         Is(PhantomData)
//     }

//     fn symm(self) -> Is<B, A> {
//         Is(PhantomData)
//     }

//     fn trans<C>(self, _: Is<B, C>) -> Is<A, C> {
//         Is(PhantomData)
//     }

//     fn convert(self, a: A) -> B {
//         let src = ManuallyDrop::new(a);
//         unsafe { std::ptr::read((&*src as *const A).cast::<B>()) }
//     }
// }

// impl OutputTy {
//     pub fn into_display<'a>(&'a self, interner: &'a LocalInterner) -> DisplayOutputTy<'a> {
//         DisplayOutputTy {
//             ty: &self,
//             interner,
//         }
//     }
// }

// pub struct DisplayOutputTy<'a> {
//     ty: &'a OutputTy,
//     interner: &'a LocalInterner,
// }

// impl fmt::Display for DisplayOutputTy<'_> {
//     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
//         match self.ty {
//             OutputTy::Int => f.write_str("Int"),
//             OutputTy::String => f.write_str("String"),
//             OutputTy::FactId => f.write_str("FactId"),
//             OutputTy::Bytes => f.write_str("Bytes"),

//             OutputTy::Record(fields) => {
//                 let mut record = f.debug_struct("Record");

//                 for (symbol, ty) in fields.iter() {
//                     let name = self.interner.try_resolve(*symbol).unwrap_or("<unresolved>");

//                     record.field(
//                         name,
//                         &DebugOutputTy {
//                             ty,
//                             interner: self.interner,
//                         },
//                     );
//                 }

//                 record.finish()
//             }
//         }
//     }
// }

// struct DebugOutputTy<'a> {
//     ty: &'a OutputTy,
//     interner: &'a LocalInterner,
// }

// impl fmt::Debug for DebugOutputTy<'_> {
//     fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
//         fmt::Display::fmt(
//             &DisplayOutputTy {
//                 ty: self.ty,
//                 interner: self.interner,
//             },
//             f,
//         )
//     }
// }

// #[derive(Debug, Clone)]
// enum OutputTySpec {
//     Int,
//     String,
//     FactId,
//     Bytes,
//     Record(Vec<(String, OutputTySpec)>),
// }

// pub fn arbitrary_output_ty() -> proptest::strategy::BoxedStrategy<(OutputTy, LocalInterner)> {
//     use proptest::prelude::*;

//     arbitrary_output_ty_spec()
//         .prop_map(|spec| {
//             let schema_reader = std::sync::Arc::new(SchemaReader::new(Rodeo::new().into_reader()));

//             let mut interner = LocalInterner::new(schema_reader);

//             let ty = lower_output_ty_spec(&spec, &mut interner);

//             (ty, interner)
//         })
//         .boxed()
// }

// fn arbitrary_output_ty_spec() -> proptest::strategy::BoxedStrategy<OutputTySpec> {
//     use proptest::prelude::*;

//     let leaf = proptest::sample::select(vec![
//         OutputTySpec::Int,
//         OutputTySpec::String,
//         OutputTySpec::FactId,
//         OutputTySpec::Bytes,
//     ]);

//     let record_strategy = leaf
//         .clone()
//         .prop_recursive(3, 6, 2, |inner| {
//             arbitrary_distinct_uids(1..=5).prop_flat_map(move |names| {
//                 let n = names.len();

//                 proptest::collection::vec(inner.clone(), n..=n).prop_map(move |tys| {
//                     let fields = names.clone().into_iter().zip(tys).collect::<Vec<_>>();

//                     OutputTySpec::Record(fields)
//                 })
//             })
//         })
//         .boxed();

//     prop_oneof![4 => leaf.boxed(), 1 => record_strategy].boxed()
// }

// fn lower_output_ty_spec(spec: &OutputTySpec, interner: &mut LocalInterner) -> OutputTy {
//     match spec {
//         OutputTySpec::Int => OutputTy::Int,
//         OutputTySpec::String => OutputTy::String,
//         OutputTySpec::FactId => OutputTy::FactId,
//         OutputTySpec::Bytes => OutputTy::Bytes,

//         OutputTySpec::Record(fields) => {
//             let fields = fields
//                 .iter()
//                 .map(|(name, ty)| {
//                     let symbol = interner.get_or_intern(name);
//                     let ty = lower_output_ty_spec(ty, interner);

//                     (symbol, ty)
//                 })
//                 .collect();

//             OutputTy::Record(fields)
//         }
//     }
// }

// pub enum Ty {
//     Int,
//     String,
//     Fact(PredicateId),
//     Record(Box<[(Symbol, Ty)]>),
//     Var(TyVarId),
//     Bytes,
//     Never,
//     Error,
// }

// pub enum GroundKind<T> {
//     Lit(Literal),
//     Var(VarId),
//     Wildcard,
//     Never,
//     Prefix(Symbol),
//     Record(Box<[(FieldIdx, T)]>),
// }

// pub enum FlatAccess {
//     Scan(PredicateId, NodeId),
//     Fetch(PredicateId, FactSource),
// }

// pub struct FlatStmt {
//     out: Option<VarId>,
//     access: FlatAccess,
// }

// pub struct FlatPlan {
//     nvars: u32,
//     body: Box<[FlatStmt]>,
//     head: Project,
//     store: SyntaxTree<GroundKind<NodeId>>,
// }

// #[derive(Clone, Copy)]
// pub enum FieldRef {
//     Key(Symbol),
//     Value,
// }

// pub enum ExprKind<T> {
//     Lit(Literal),
//     Var(Symbol),
//     Wildcard,
//     Never,
//     Prefix(Symbol),
//     Record(Box<[(Symbol, T)]>),
//     Access(FieldRef, T),
//     Fact(PredicateId, T),
//     Subquery(Box<Query<T>>),
//     Error,
// }

// pub enum QueryStmt<T> {
//     Bind(T, T),
//     Implicit(T),
// }

// pub struct Query<T> {
//     body: Box<[QueryStmt<T>]>,
//     head: T,
// }

// pub struct Ast {
//     query: Query<NodeId>,
//     store: SyntaxTree<ExprKind<NodeId>>,
// }

// pub struct SyntaxTree<K: Recursive> {
//     kinds: Vec<K>,
//     spans: Vec<Span>,
// }

// impl<K: Recursive> SyntaxTree<K> {
//     pub fn reduce<R, F>(&self, id: NodeId, f: &mut F) -> R
//     where
//         K: Recursive<Base<R> = R>,
//         F: FnMut(NodeId, K::Base<R>) -> R,
//     {
//         let acc = self.kinds[id.0 as usize].map(|child_id| self.reduce(child_id, f));
//         f(id, acc)
//     }
// }

// pub trait Recursive {
//     type Base<R>;
//     fn map<R, F: FnMut(NodeId) -> R>(&self, f: F) -> Self::Base<R>;
// }

// impl Recursive for GroundKind<NodeId> {
//     type Base<R> = GroundKind<R>;

//     fn map<R, F: FnMut(NodeId) -> R>(&self, mut f: F) -> Self::Base<R> {
//         match self {
//             GroundKind::Lit(lit) => GroundKind::Lit(*lit),
//             GroundKind::Var(var) => GroundKind::Var(*var),
//             GroundKind::Wildcard => GroundKind::Wildcard,
//             GroundKind::Never => GroundKind::Never,
//             GroundKind::Prefix(symbol) => GroundKind::Prefix(*symbol),
//             GroundKind::Record(fields) => {
//                 let new_fields = fields
//                     .iter()
//                     .map(|(idx, node_id)| (*idx, f(*node_id)))
//                     .collect();
//                 GroundKind::Record(new_fields)
//             }
//         }
//     }
// }

// impl Recursive for ExprKind<NodeId> {
//     type Base<R> = ExprKind<R>;

//     fn map<R, F: FnMut(NodeId) -> R>(&self, mut f: F) -> Self::Base<R> {
//         match self {
//             ExprKind::Lit(lit) => ExprKind::Lit(*lit),
//             ExprKind::Var(symbol) => ExprKind::Var(*symbol),
//             ExprKind::Wildcard => ExprKind::Wildcard,
//             ExprKind::Never => ExprKind::Never,
//             ExprKind::Prefix(symbol) => ExprKind::Prefix(*symbol),
//             ExprKind::Record(fields) => ExprKind::Record(
//                 fields
//                     .iter()
//                     .map(|(symbol, node_id)| (*symbol, f(*node_id)))
//                     .collect(),
//             ),
//             ExprKind::Access(field_ref, node_id) => ExprKind::Access(*field_ref, f(*node_id)),
//             ExprKind::Fact(pred_id, node_id) => ExprKind::Fact(*pred_id, f(*node_id)),
//             ExprKind::Subquery(query) => {
//                 ExprKind::Subquery(Box::new(query.map(|node_id| f(node_id))))
//             }
//             ExprKind::Error => ExprKind::Error,
//         }
//     }
// }

// impl Recursive for QueryStmt<NodeId> {
//     type Base<R> = QueryStmt<R>;

//     fn map<R, F: FnMut(NodeId) -> R>(&self, mut f: F) -> Self::Base<R> {
//         match self {
//             QueryStmt::Bind(lhs, rhs) => QueryStmt::Bind(f(*lhs), f(*rhs)),
//             QueryStmt::Implicit(node_id) => QueryStmt::Implicit(f(*node_id)),
//         }
//     }
// }

// impl Recursive for Query<NodeId> {
//     type Base<R> = Query<R>;

//     fn map<R, F: FnMut(NodeId) -> R>(&self, mut f: F) -> Self::Base<R> {
//         Query {
//             body: self
//                 .body
//                 .iter()
//                 .map(|stmt| stmt.map(|node_id| f(node_id)))
//                 .collect(),
//             head: f(self.head),
//         }
//     }
// }

// pub enum KeyLayout {
//     Scalar(ScalarTy),
//     Record(Box<[FieldDef]>),
// }

// pub struct FieldDef {
//     name: Spur,
//     ty: Ty,
// }

// pub struct PredicateDef {
//     name: Spur,
//     key_layout: KeyLayout,
//     value_ty: Ty,
// }

// #[derive(Debug)]
// pub struct SchemaReader {
//     reader: RodeoReader,
// }

// impl SchemaReader {
//     pub fn new(reader: RodeoReader) -> Self {
//         SchemaReader { reader }
//     }

//     pub fn get(&self, s: &str) -> Option<Symbol> {
//         self.reader.get(s).map(Symbol::Schema)
//     }

//     pub fn try_resolve(&self, symbol: Symbol) -> Option<&str> {
//         match symbol {
//             Symbol::Schema(spur) => self.reader.try_resolve(&spur),
//             Symbol::Local(_) => None,
//         }
//     }
// }

// #[derive(Debug)]
// pub struct LocalInterner {
//     schema_reader: Arc<SchemaReader>,
//     rodeo: Rodeo,
// }

// impl LocalInterner {
//     pub fn new(schema_reader: Arc<SchemaReader>) -> LocalInterner {
//         LocalInterner {
//             schema_reader,
//             rodeo: Rodeo::new(),
//         }
//     }

//     pub fn get(&self, s: &str) -> Option<Symbol> {
//         if let Some(symbol) = self.schema_reader.get(s) {
//             return Some(symbol);
//         }

//         self.rodeo.get(s).map(Symbol::Local)
//     }

//     pub fn try_resolve(&self, symbol: Symbol) -> Option<&str> {
//         match symbol {
//             Symbol::Schema(spur) => self.schema_reader.try_resolve(Symbol::Schema(spur)),
//             Symbol::Local(spur) => self.rodeo.try_resolve(&spur),
//         }
//     }

//     pub fn get_or_intern(&mut self, s: &str) -> Symbol {
//         if let Some(symbol) = self.schema_reader.get(s) {
//             return symbol;
//         }

//         Symbol::Local(self.rodeo.get_or_intern(s))
//     }
// }

// #[derive(Debug, Clone, Copy, PartialEq, Eq)]
// pub enum SymbolSpec {
//     Schema,
//     Local,
// }

// pub fn arbitrary_symbols<S>(
//     spec_strategy: S,
// ) -> proptest::strategy::BoxedStrategy<(Vec<Symbol>, LocalInterner)>
// where
//     S: proptest::strategy::Strategy<Value = Vec<SymbolSpec>> + 'static,
// {
//     use proptest::prelude::*;

//     spec_strategy
//         .prop_flat_map(|spec| {
//             let n = spec.len();

//             arbitrary_distinct_uids(n..=n)
//                 .prop_map(move |strings| build_symbols_from_spec(&spec, strings))
//         })
//         .boxed()
// }

// pub fn arbitrary_symbol_spec() -> proptest::strategy::BoxedStrategy<SymbolSpec> {
//     use proptest::prelude::*;

//     proptest::sample::select(vec![SymbolSpec::Schema, SymbolSpec::Local]).boxed()
// }

// fn build_symbols_from_spec(
//     spec: &[SymbolSpec],
//     strings: Vec<String>,
// ) -> (Vec<Symbol>, LocalInterner) {
//     assert_eq!(spec.len(), strings.len());

//     let mut schema_rodeo = Rodeo::new();
//     let mut local_rodeo = Rodeo::new();

//     let mut symbols = Vec::with_capacity(spec.len());

//     for (ctor, s) in spec.iter().copied().zip(strings.iter()) {
//         let symbol = match ctor {
//             SymbolSpec::Schema => Symbol::Schema(schema_rodeo.get_or_intern(s)),
//             SymbolSpec::Local => Symbol::Local(local_rodeo.get_or_intern(s)),
//         };

//         symbols.push(symbol);
//     }
//     let schema_reader = Arc::new(SchemaReader::new(schema_rodeo.into_reader()));

//     let local_interner = LocalInterner {
//         schema_reader: schema_reader.clone(),
//         rodeo: local_rodeo,
//     };

//     (symbols, local_interner)
// }

// fn arbitrary_distinct_uids(
//     size: std::ops::RangeInclusive<usize>,
// ) -> impl proptest::strategy::Strategy<Value = Vec<String>> {
//     use proptest::strategy::Strategy;

//     proptest::collection::btree_set(arbitrary_uid(), size).prop_map(|set| set.into_iter().collect())
// }

// fn arbitrary_uid() -> impl proptest::strategy::Strategy<Value = String> {
//     "[a-z][a-zA-Z0-9_]{0,9}"
// }

// pub struct Schema {
//     reader: SchemaReader,
//     predicates: Arc<[PredicateDef]>,
// }

// pub type Scratch = Vec<u8>;

// #[inline]
// pub fn put_i64(scratch: &mut Scratch, value: i64) {
//     scratch.extend_from_slice(&value.to_be_bytes());
// }

// #[inline]
// pub fn put_u64(scratch: &mut Scratch, value: u64) {
//     scratch.extend_from_slice(&value.to_be_bytes());
// }

// #[inline]
// pub fn put_bytes(scratch: &mut Scratch, bytes: &[u8]) {
//     scratch.extend_from_slice(bytes);
// }

// #[must_use = "every opened slot must be closed with a corresponding `close_len`"]
// pub struct LenSlot {
//     at: u32,
// }

// const LEN_SLOT_SIZE: usize = size_of::<u32>();

// #[inline]
// pub fn open_len(scratch: &mut Scratch) -> LenSlot {
//     let at = scratch.len();
//     scratch.extend_from_slice(&[0u8; LEN_SLOT_SIZE]);
//     LenSlot { at: at as u32 }
// }

// #[inline]
// pub fn close_len(scratch: &mut Scratch, slot: LenSlot) -> Result<(), TransportCodecError> {
//     let len = scratch.len() - slot.at as usize - LEN_SLOT_SIZE;
//     if len > u32::MAX as usize {
//         return Err(TransportCodecError::FieldTooLarge(len));
//     }
//     let len_bytes = (len as u32).to_be_bytes();
//     scratch[slot.at as usize..slot.at as usize + LEN_SLOT_SIZE].copy_from_slice(&len_bytes);
//     Ok(())
// }

// #[must_use = "every opened slot must be closed with a corresponding `close_row`"]
// pub struct RowSlot {
//     at: u32,
// }

// const ROW_SLOT_SIZE: usize = size_of::<u32>();

// #[inline]
// pub fn open_row(scratch: &mut Scratch) -> RowSlot {
//     let at = scratch.len();
//     scratch.extend_from_slice(&[0u8; ROW_SLOT_SIZE]);
//     RowSlot { at: at as u32 }
// }

// #[inline]
// pub fn close_row(scratch: &mut Scratch, slot: RowSlot) -> Result<(), TransportCodecError> {
//     let len = scratch.len() - slot.at as usize - ROW_SLOT_SIZE;
//     if len > u32::MAX as usize {
//         return Err(TransportCodecError::RowTooLarge(len));
//     }
//     let len_bytes = (len as u32).to_be_bytes();
//     scratch[slot.at as usize..slot.at as usize + ROW_SLOT_SIZE].copy_from_slice(&len_bytes);
//     Ok(())
// }

// #[must_use = "every opened slot must be closed with a corresponding `close_record`"]
// pub struct RecordSlot {
//     at: u16,
// }

// const RECORD_SLOT_SIZE: usize = size_of::<u16>();

// #[inline]
// pub fn open_record(scratch: &mut Scratch) -> RecordSlot {
//     let at = scratch.len();
//     scratch.extend_from_slice(&[0u8; RECORD_SLOT_SIZE]);
//     RecordSlot { at: at as u16 }
// }

// #[inline]
// pub fn close_record(scratch: &mut Scratch, slot: RecordSlot) -> Result<(), TransportCodecError> {
//     let len = scratch.len() - slot.at as usize - RECORD_SLOT_SIZE;
//     if len > u16::MAX as usize {
//         return Err(TransportCodecError::RecordTooLarge(len));
//     }
//     let len_bytes = (len as u16).to_be_bytes();
//     scratch[slot.at as usize..slot.at as usize + RECORD_SLOT_SIZE].copy_from_slice(&len_bytes);
//     Ok(())
// }

// pub const OT_INT: u8 = 0x01;
// pub const OT_STRING: u8 = 0x02;
// pub const OT_FACT_ID: u8 = 0x03;
// pub const OT_BYTES: u8 = 0x04;
// pub const OT_RECORD: u8 = 0x05;

// #[derive(Error, Debug)]
// pub enum TransportCodecError {
//     #[error("unknown symbol: {0:?}")]
//     UnknownSymbol(Symbol),

//     #[error("record too large: expected no more than u16::MAX bytes, got {0} bytes")]
//     RecordTooLarge(usize),

//     #[error("row too large: expected no more than u32::MAX bytes, got {0} bytes")]
//     RowTooLarge(usize),

//     #[error("field too large: expected no more than u32::MAX bytes, got {0} bytes")]
//     FieldTooLarge(usize),
// }

// pub fn put_output_ty(
//     scratch: &mut Scratch,
//     local_interner: &LocalInterner,
//     ty: &OutputTy,
// ) -> Result<(), TransportCodecError> {
//     match ty {
//         OutputTy::Int => {
//             scratch.push(OT_INT);
//         }
//         OutputTy::String => {
//             scratch.push(OT_STRING);
//         }
//         OutputTy::FactId => {
//             scratch.push(OT_FACT_ID);
//         }
//         OutputTy::Bytes => {
//             scratch.push(OT_BYTES);
//         }
//         OutputTy::Record(fields) => {
//             scratch.push(OT_RECORD);
//             scratch.extend_from_slice(&(fields.len() as u16).to_be_bytes());
//             for (symbol, field_ty) in fields.iter() {
//                 let name_bytes = local_interner
//                     .try_resolve(*symbol)
//                     .map(|s| s.as_bytes())
//                     .ok_or_else(|| TransportCodecError::UnknownSymbol(*symbol))?;

//                 scratch.extend_from_slice(&(name_bytes.len() as u16).to_be_bytes());
//                 scratch.extend_from_slice(name_bytes);
//                 put_output_ty(scratch, local_interner, field_ty)?
//             }
//         }
//     }

//     Ok(())
// }

// #[derive(Debug, Error)]
// pub enum ApertureError {
//     #[error("field index out of bounds: {0:?}")]
//     FieldIndexOutOfBounds(FieldIdx),

//     #[error("malformed key")]
//     MalformedKey,

//     #[error("unexpected marker: {0}")]
//     UnexpectedMarker(u8),
// }

// pub type FactId = u64;

// #[derive(Debug, Clone)]
// pub struct Slot {
//     fact_id: FactId,
//     key: ByteView,
// }

// pub struct Env {
//     slots: Box<[Option<Slot>]>,
// }

// pub struct Row<'e> {
//     env: &'e Env,
// }

// trait Store {
//     fn fetch_entity(&self, id: FactId) -> Result<Entity, ApertureError>;
// }

// struct Entity {
//     key: ByteView,
//     value: ByteView,
// }

// struct Executor<'s> {
//     keys: &'s fjall::Keyspace,
//     entities: &'s fjall::Keyspace,
//     schema: &'s Schema,
// }

// pub struct FieldOffsets {
//     ranges: TinyVec<[std::ops::Range<usize>; 8]>,
// }

// impl FieldOffsets {
//     pub fn scan(key: &ByteView, max: FieldIdx) -> Result<Self, ApertureError> {
//         let mut ranges = TinyVec::new();
//         let mut i = 0;
//         while ranges.len() <= max.0 as usize {
//             if i >= key.len() {
//                 break;
//             }
//             let start = i;
//             let end = key_codec::skip_one(key, i, false)?;
//             ranges.push(start..end);
//             i = end;
//         }
//         Ok(FieldOffsets { ranges })
//     }

//     fn field(&self, field_idx: FieldIdx, key: ByteView) -> Result<ByteView, ApertureError> {
//         let range = self
//             .ranges
//             .get(field_idx.0 as usize)
//             .ok_or(ApertureError::FieldIndexOutOfBounds(field_idx))?;

//         Ok(key.slice(range.clone()))
//     }
// }

// impl<'s> Executor<'s> {
//     fn exec(&self, plan: &Plan) -> Result<u64, ApertureError> {
//         use itertools::Itertools;

//         let Ok(stmt) = plan.body.iter().exactly_one() else {
//             unimplemented!()
//         };

//         let (pred_id, seek_key) = match &stmt.access {
//             Access::Scan(pred_id, seek_key) => (pred_id, seek_key),
//             Access::Fetch(_, _) => unimplemented!(),
//         };

//         let mut lo = pred_id.0.to_be_bytes().to_vec();
//         match seek_key {
//             SeekKey::Prefix(prefix) => {
//                 lo.extend_from_slice(prefix);
//             }
//             SeekKey::Composite(_) => unimplemented!(),
//         }

//         let mut env = Env {
//             slots: vec![None; plan.nvars as usize].into_boxed_slice(),
//         };

//         let mut row_count = 0u64;

//         for guard in self.keys.prefix(&lo) {
//             let (key, value) = guard.into_inner().unwrap();

//             let key_bytes: ByteView = key.into();

//             let fact_key = key_bytes.slice(size_of::<u32>()..key_bytes.len());

//             let fact_id = u64::from_be_bytes(
//                 value[..size_of::<u32>()]
//                     .try_into()
//                     .map_err(|_| ApertureError::MalformedKey)?,
//             );

//             let field_offsets = FieldOffsets::scan(&fact_key, stmt.max_field)?;

//             if !self.check_residuals(stmt.residuals.as_ref(), &fact_key, &field_offsets)? {
//                 continue;
//             }

//             if let Some(out) = stmt.out {
//                 env.slots[out.0 as usize] = Some(Slot {
//                     fact_id,
//                     key: fact_key.clone(),
//                 });
//             }

//             for binding in stmt.bindings.iter() {
//                 env.slots[binding.1.0 as usize] = Some(Slot {
//                     fact_id,
//                     key: fact_key.clone(),
//                 })
//             }

//             let row = Row { env: &env };
//             self.emit(&row);
//             row_count += 1;
//         }

//         Ok(row_count)
//     }

//     pub fn check_residuals(
//         &self,
//         _plan: &[Residual],
//         _key: &ByteView,
//         _offsets: &FieldOffsets,
//     ) -> Result<bool, ApertureError> {
//         unimplemented!()
//     }

//     pub fn emit(&self, _: &Row) {
//         unimplemented!()
//     }
// }

// mod key_codec {
//     use super::*;

//     const M_NULL: u8 = 0x00;
//     const M_BYTES: u8 = 0x01;
//     const M_STRING: u8 = 0x02;
//     const M_RECORD: u8 = 0x05;

//     const M_INT_NEG_MIN: u8 = 0x0C;
//     const M_INT_ZERO: u8 = 0x14;
//     const M_INT_POS_MAX: u8 = 0x1C;

//     #[allow(dead_code)]
//     mod reserved {
//         pub const M_DOUBLE: u8 = 0x21;
//         pub const M_UUID: u8 = 0x30;
//         pub const M_ENUM: u8 = 0x40;
//         pub const M_TIMESTAMP: u8 = 0x48;
//     }

//     const TERM: u8 = 0x00;
//     const ESC: u8 = 0xFF;

//     #[inline]
//     pub fn int_width(mag: u64) -> usize {
//         8 - (mag.leading_zeros() as usize / 8)
//     }

//     pub fn put_int(out: &mut Scratch, v: i64) {
//         if v == 0 {
//             out.push(M_INT_ZERO);
//             return;
//         }

//         let mag = v.unsigned_abs();
//         let width = int_width(mag);

//         let marker = if v > 0 {
//             M_INT_ZERO + width as u8
//         } else {
//             M_INT_ZERO - width as u8
//         };

//         let bytes = if v > 0 {
//             mag.to_be_bytes()
//         } else {
//             (!mag).to_be_bytes()
//         };

//         out.push(marker);
//         out.extend_from_slice(&bytes[8 - width..]);
//     }

//     #[inline]
//     pub fn put_u64(out: &mut Scratch, v: u64) {
//         if v == 0 {
//             out.push(M_INT_ZERO);
//             return;
//         }
//         let n = int_width(v);
//         let be = v.to_be_bytes();
//         out.push(M_INT_ZERO + n as u8);
//         out.extend_from_slice(&be[8 - n..]);
//     }

//     pub fn decode_int(field: &[u8]) -> Result<i64, ApertureError> {
//         let tag = *field.first().ok_or(ApertureError::MalformedKey)?;
//         if tag == M_INT_ZERO {
//             return Ok(0);
//         }
//         let mut buf = [0u8; 8];
//         if tag > M_INT_ZERO {
//             let n = (tag - M_INT_ZERO) as usize;
//             if field.len() != 1 + n {
//                 return Err(ApertureError::MalformedKey);
//             }
//             buf[8 - n..].copy_from_slice(&field[1..]);
//             Ok(u64::from_be_bytes(buf) as i64)
//         } else if tag >= M_INT_NEG_MIN {
//             let n = (M_INT_ZERO - tag) as usize;
//             if field.len() != 1 + n {
//                 return Err(ApertureError::MalformedKey);
//             }
//             for k in 0..n {
//                 buf[8 - n + k] = !field[1 + k];
//             }
//             Ok((u64::from_be_bytes(buf) as i64).wrapping_neg())
//         } else {
//             Err(ApertureError::UnexpectedMarker(tag))
//         }
//     }

//     pub fn put_escaped(out: &mut Scratch, marker: u8, content: &[u8]) {
//         out.push(marker);
//         for &b in content {
//             out.push(b);
//             if b == TERM {
//                 out.push(ESC);
//             }
//         }
//         out.push(TERM);
//     }

//     pub fn put_str(out: &mut Scratch, s: &str) {
//         put_escaped(out, M_STRING, s.as_bytes());
//     }

//     pub fn put_bytes(out: &mut Scratch, b: &[u8]) {
//         put_escaped(out, M_BYTES, b);
//     }

//     pub fn decode_escaped<'a>(
//         field: &'a [u8],
//     ) -> Result<std::borrow::Cow<'a, [u8]>, ApertureError> {
//         use std::borrow::Cow;
//         if field.len() < 2 {
//             return Err(ApertureError::MalformedKey);
//         }
//         let inner = &field[1..field.len() - 1];
//         if !inner.contains(&TERM) {
//             return Ok(Cow::Borrowed(inner));
//         }
//         let mut out = Vec::with_capacity(inner.len());
//         let mut i = 0;
//         while i < inner.len() {
//             out.push(inner[i]);
//             i += if inner[i] == TERM { 2 } else { 1 };
//         }
//         Ok(Cow::Owned(out))
//     }

//     pub fn skip_terminated(src: &[u8], mut i: usize) -> Result<usize, ApertureError> {
//         loop {
//             match src.get(i) {
//                 None => return Err(ApertureError::MalformedKey),
//                 Some(&TERM) if src.get(i + 1) == Some(&ESC) => i += 2,
//                 Some(&TERM) => return Ok(i + 1),
//                 Some(_) => i += 1,
//             }
//         }
//     }

//     pub fn skip_one(src: &[u8], i: usize, nested: bool) -> Result<usize, ApertureError> {
//         let tag = *src.get(i).ok_or(ApertureError::MalformedKey)?;
//         let after_tag = i + 1;
//         match tag {
//             M_NULL => {
//                 if nested {
//                     if src.get(after_tag) != Some(&ESC) {
//                         return Err(ApertureError::MalformedKey);
//                     }
//                     Ok(after_tag + 1)
//                 } else {
//                     Ok(after_tag)
//                 }
//             }
//             M_BYTES | M_STRING => skip_terminated(src, after_tag),
//             M_RECORD => {
//                 let mut j = after_tag;
//                 loop {
//                     match src.get(j) {
//                         None => return Err(ApertureError::MalformedKey),
//                         Some(&TERM) if src.get(j + 1) != Some(&ESC) => return Ok(j + 1),
//                         Some(_) => j = skip_one(src, j, true)?,
//                     }
//                 }
//             }
//             M_INT_NEG_MIN..=M_INT_POS_MAX => {
//                 let n = (tag as i16 - M_INT_ZERO as i16).unsigned_abs() as usize;
//                 let end = after_tag + n;
//                 if end > src.len() {
//                     return Err(ApertureError::MalformedKey);
//                 }
//                 Ok(end)
//             }
//             other => Err(ApertureError::UnexpectedMarker(other)),
//         }
//     }

//     pub fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
//         let mut out = prefix.to_vec();
//         while let Some(&last) = out.last() {
//             if last == 0xFF {
//                 out.pop();
//             } else {
//                 *out.last_mut().unwrap() += 1;
//                 return Some(out);
//             }
//         }
//         None
//     }

//     #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
//     pub enum Value {
//         Null,
//         Bytes(ByteView),
//         Str(String),
//         Record(Vec<Value>),
//         Int(i64),
//     }

//     pub fn encode_value(out: &mut Scratch, v: &Value, nested: bool) {
//         match v {
//             Value::Null => {
//                 out.push(M_NULL);
//                 if nested {
//                     out.push(ESC);
//                 }
//             }
//             Value::Bytes(b) => put_bytes(out, b),
//             Value::Str(s) => put_str(out, s),
//             Value::Record(elems) => {
//                 out.push(M_RECORD);
//                 for e in elems {
//                     encode_value(out, e, true);
//                 }
//                 out.push(TERM);
//             }
//             Value::Int(n) => put_int(out, *n),
//         }
//     }

//     pub fn encode_tuple(elems: &[Value]) -> Vec<u8> {
//         let mut out = Vec::new();
//         for e in elems {
//             encode_value(&mut out, e, false);
//         }
//         out
//     }

//     pub fn decode_value(src: &[u8], nested: bool) -> Result<(Value, &[u8]), ApertureError> {
//         let end = skip_one(src, 0, nested)?;
//         let (field, rest) = src.split_at(end);
//         let tag = field[0];
//         let v = match tag {
//             M_NULL => Value::Null,
//             M_BYTES => Value::Bytes(decode_escaped(field)?.into_owned().into()),
//             M_STRING => {
//                 let bytes = decode_escaped(field)?.into_owned();
//                 Value::Str(String::from_utf8(bytes).map_err(|_| ApertureError::MalformedKey)?)
//             }
//             M_RECORD => {
//                 let mut inner = &field[1..field.len() - 1];
//                 let mut elems = Vec::new();
//                 while !inner.is_empty() {
//                     let (v, rest) = decode_value(inner, true)?;
//                     elems.push(v);
//                     inner = rest;
//                 }
//                 Value::Record(elems)
//             }
//             M_INT_NEG_MIN..=M_INT_POS_MAX => Value::Int(decode_int(field)?),
//             other => return Err(ApertureError::UnexpectedMarker(other)),
//         };
//         Ok((v, rest))
//     }
// }

// #[cfg(test)]
// mod tests {
//     use super::key_codec::*;
//     use proptest::prelude::*;

//     fn value_strategy() -> impl Strategy<Value = Value> {
//         let leaf = prop_oneof![
//             Just(Value::Null),
//             any::<Vec<u8>>().prop_map(|b| Value::Bytes(b.into())),
//             any::<String>().prop_map(Value::Str),
//             any::<i64>().prop_map(Value::Int),
//         ];

//         leaf.prop_recursive(4, 64, 8, |inner| {
//             prop::collection::vec(inner, 0..8).prop_map(Value::Record)
//         })
//     }

//     proptest! {
//         #[test]
//         fn test_value_roundtrip(v in value_strategy(), nested in any::<bool>()) {
//             let mut out = Vec::new();

//             encode_value(&mut out, &v, nested);
//             let (decoded, rest) = decode_value(&out, nested).unwrap();

//             assert_eq!(v, decoded);
//             assert!(rest.is_empty(), "Decoder did not consume all bytes");
//         }

//         #[test]
//         fn test_tuple_roundtrip(tuple in prop::collection::vec(value_strategy(), 0..10)) {
//             let encoded = encode_tuple(&tuple);
//             let mut current = encoded.as_slice();
//             let mut decoded_tuple = Vec::new();

//             while !current.is_empty() {
//                 let (v, rest) = decode_value(current, false).unwrap();
//                 decoded_tuple.push(v);
//                 current = rest;
//             }

//             assert_eq!(tuple, decoded_tuple);
//         }

//         #[test]
//         fn test_value_ordering(v1 in value_strategy(), v2 in value_strategy(), nested in any::<bool>()) {
//             let mut enc1 = Vec::new();
//             encode_value(&mut enc1, &v1, nested);

//             let mut enc2 = Vec::new();
//             encode_value(&mut enc2, &v2, nested);

//             assert_eq!(
//                 v1.cmp(&v2),
//                 enc1.cmp(&enc2),
//                 "Ordering mismatch between Value and Encoded bytes:\nv1: {:?}\nv2: {:?}",
//                 v1, v2
//             );
//         }

//         #[test]
//         fn test_tuple_ordering(
//             t1 in prop::collection::vec(value_strategy(), 0..5),
//             t2 in prop::collection::vec(value_strategy(), 0..5)
//         ) {
//             let enc1 = encode_tuple(&t1);
//             let enc2 = encode_tuple(&t2);

//             assert_eq!(
//                 t1.cmp(&t2),
//                 enc1.cmp(&enc2),
//                 "Tuple ordering mismatch:\nt1: {:?}\nt2: {:?}",
//                 t1, t2
//             );
//         }
//     }
// }
