---
title: sigla query language
description: Every construct sigla has — patterns, statements, binds, constraints, denials, comparisons, arithmetic, negation, disjunction, subqueries and references — with the rows each one returns.
---

sigla is a typed, Datalog-flavoured query language. A query is a **head pattern**, the word
`where`, and a list of **statements**:

```sigla
{file = F, line = L} where src.Ref {to = src.Decl {name = "encode"}, file = F, at = {line = L}}
```

The head says what a row looks like. The statements say which rows there are. There is no
`select`, no `from`, no `join` keyword: a join is two statements sharing a variable, and the
compiler decides the order.

## Grammar

```text
query      ::= pattern 'where' stmt (';' stmt)* [';']

stmt       ::= '!' pattern                     negation — no such row exists
             | pattern '=' pattern             bind, constraint, alias or fold
             | pattern '!=' pattern            denial
             | pattern ('<'|'<='|'>'|'>=') pattern    comparison
             | pattern                         a generator (implicit bind)

pattern    ::= sum ('|' sum)*                  disjunction, flat and n-ary
sum        ::= branch (('+'|'-') branch)*       arithmetic, flat and left to right
branch     ::= QualifiedName branch            a fact pattern
             | primary ('.' field ['?'])*       field access
primary    ::= '_'                             wildcard
             | Variable                        an upper-case name
             | 42 | -42 | 1_000                integers
             | "text"                          a string
             | "text"..                        a string prefix — a range, not a value
             | '{' field '=' pattern, … '}'     a record
             | 'never'                         the empty relation
             | '(' pattern ')'                 a group
             | '(' pattern 'where' stmt… ')'    a subquery
```

**Names carry meaning.** A variable is `UpperCase`, a field is `lowerCase`, and a predicate
is `namespace.UpperCase`. Nothing needs a sigil, and the lexer can tell all three apart.

**Precedence, loosest to tightest:** `|` → `+`/`-` → application (`test.Foo X`) → `.`
access. So `test.Foo A | test.Bar B` is a disjunction of two fact patterns, and
`test.Name Y.name` is `test.Name (Y.name)`. A disjunction *inside* a key is parenthesised:
`test.Foo (A | B)`.

## The examples database

Every "returns" below is what the query actually answers against the fixture database the
test corpus uses — so each row is checked by a test rather than written down by hand.

```schema
predicate test.Foo    : { id : int, name : string } -> string
predicate test.Bar    : { id : int }
predicate test.Edge   : { from : int, to : int }
predicate test.Node   : { id : int }
predicate test.Nested : { outer : { inner : int } }
predicate test.Name   : string
predicate test.Count  : int
predicate test.Wide   : { outer : { extra : int, inner : int } }
predicate test.Ref    : { of : test.Foo }
predicate test.Link   : { at : int, of : test.Foo }
predicate test.Deep   : { via : test.Ref }
predicate test.Boxed  : { id : int } -> { lo : int, hi : int }

# One union, used twice: leading the key in Tagged, behind the key in Label.
# The tags are deliberately not positions — `num` is 3 and `text` is 0.
type test.What = { num : int = 3 | text : string = 0 }
predicate test.Tagged : { what : test.What, id : int }
predicate test.Label  : { id : int, what : test.What }
```

| Predicate | Facts |
|---|---|
| `test.Foo` | `{1, ann} -> one`, `{2, bob} -> two`, `{3, ann} -> three` |
| `test.Bar` | `{1}`, `{2}` |
| `test.Edge` | `{1,2}`, `{1,3}`, `{2,3}` |
| `test.Node` | `{2}`, `{3}` |
| `test.Name` | `abc`, `ann`, `anna`, `bob` |
| `test.Count` | `i64::MIN`, `-42`, `7`, `1000` |
| `test.Nested` | `{inner = 1}`, `{inner = 7}` |
| `test.Ref` | `→ Foo#1`, `→ Foo#2` |
| `test.Link` | `{10, Foo#1}`, `{11, Foo#2}`, `{12, Foo#2}` |
| `test.Deep` | `→ Ref#1`, `→ Ref#2` |
| `test.Boxed` | `{1} -> {10,20}`, `{2} -> {30,40}` |
| `test.Tagged` | `{num 1, 10}`, `{text "a", 20}`, `{num 2, 30}`, `{text "b", 40}` |
| `test.Label` | `{10, num 1}`, `{20, text "a"}`, `{30, num 2}`, `{40, text "b"}` |

## Statements

### A generator

A fact pattern on its own is a **generator**: a loop over the rows of one predicate.

```sigla
X where test.Foo {name = X}          → ann; bob; ann
```

The key is **mandatory**, so a whole-predicate scan is written with a wildcard:

```sigla
X where X = test.Foo _               → test.Foo#1; test.Foo#2; test.Foo#3
```

A scalar key is one field, so a variable may stand for the whole of it:

```sigla
Y where test.Count Y                 → -9223372036854775808; -42; 7; 1000
Y where test.Foo Y                   → {id = 1, name = ann}; {id = 2, name = bob}; {id = 3, name = ann}
```

Fields not mentioned are simply not constrained. Naming one binds it; giving it a constant
narrows or filters.

### A join is a shared variable

```sigla
X where test.Edge {from = X, to = Y}; test.Node {id = Y}     → 1; 1; 2
```

Two levels, and the inner one's seek key is built from the outer row's bytes — that is what
a join *is* here. Which statement runs first is `reorder`'s decision, not the order you
typed:

```sigla
P where test.Ref {of = P}; P = test.Foo {id = 1}             → test.Foo#1
P where P = test.Foo {id = 1}; test.Ref {of = P}             → test.Foo#1
```

Both spellings are the **same plan**. `reorder` emits the *runnable frontier*: a statement
that reads a variable the next statement binds is moved rather than refused.

### A bind means one of four things

`=` is not assignment. flatten decides which of four things a bind is, and **only the first
takes a register**:

| Written | It is | Cost |
|---|---|---|
| `X = test.Foo _` | a **row bind** — a loop level | one register, one level |
| `X = 42`, `X = {inner = 1}` | a **constant fold** — substituted at every use | nothing |
| `Y = X.name` | an **alias** — a name for a place already in a register | nothing |
| `X = "a"..`, `X = Y` (both bound) | a **constraint** — what the value has to look like | a seek, or a residual |

```sigla
X where X = 42                               → 42
Y where X = test.Foo _; Y = X.name           → ann; bob; ann
Z where test.Bar {id = Z}; Z = 1             → 1
X where test.Foo {id = X}; test.Bar {id = Y}; X = Y   → 1; 2
```

A folded constant reaches a key field as the literal written in place would, so
`Z = 1; test.Bar {id = Z}` seeks the bytes `test.Bar {id = 1}` seeks. A query whose every
bind folded has **no levels at all** and means exactly one row.

### A constraint narrows the level that captures the variable

A string prefix is a **range**, not a value, so there is nothing for a variable to *be* — it
says what the value has to look like:

```sigla
X where test.Name X; X = "a"..               → abc; ann; anna
X where X = "a"..; test.Name X               → abc; ann; anna
```

Both are the *same range seek* `test.Name "a"..` is. Constraints are collected from the whole
body before an order is chosen and applied by whichever level captures the variable — which
is why writing one first changes nothing. Behind an already-open field there is no seek left
to narrow, and the same constraint filters instead:

```sigla
X where test.Foo {name = X}; X = "a"..       → ann; ann
```

:::note This is not a residual you can move
Applying a constraint afterwards as a filter would answer the same rows and read the whole
predicate to find them. The whole point of collecting it before ordering is that the level
that captures the variable can seek with it.
:::

### A denial: `!=`

The negative of a constraint, and never a seek — "does not start with `a`" is the two ranges
either side of one, and a seek walks one range.

```sigla
X where test.Name X; X != "a"..              → bob
X where test.Name X; X != "abc"              → ann; anna; bob
X where test.Count X; X != 7                 → -9223372036854775808; -42; 1000
X where test.Name X; X = "a"..; X != "an"..  → abc
```

The last one is the pair worth reading in a `:plan`: the constraint narrows the level's seek
to a range, and the denial filters the rows inside it.

`!` and `!=` are different questions and stay different syntax. `!` says *no such row
exists* and costs a test step; `!=` says *this row's field does not look like that* and costs
a residual.

### Comparisons: `<` `<=` `>` `>=`

They are **statements**, not expressions — a comparison binds nothing, and reading `X < 3` as
an expression would need a boolean type the model does not have.

```sigla
X where test.Count X; X < 7                  → -9223372036854775808; -42
X where test.Count X; X <= 7                 → -9223372036854775808; -42; 7
X where test.Count X; 7 <= X                 → 7; 1000
N where test.Name N; N > "ann"               → anna; bob
{a = X, b = Y} where test.Edge {from = X, to = Y}; X < Y   → {a = 1, b = 2}; {a = 1, b = 3}; {a = 2, b = 3}
```

Strings compare for the same reason integers do: the encoding is order-preserving, so
`"ann" < "anna"` falls out of the bytes. The constant may be on either side — the field
carries the residual whichever way it was written, and the relation is flipped rather than a
second code path added. Comparisons **filter**; none of them is a seek yet.

### Arithmetic: `+` `-`

A derived bind: one value per row, computed from the row, in a register of its own. It is not
a loop level — the cursor stores nothing for it, because it is recomputed on resume.

```sigla
Y where test.Count X; Y = X + 1              → -9223372036854775807; -41; 8; 1001
Y where test.Count X; Y = X - 1              → 9223372036854775807; -43; 6; 999
Y where test.Edge {from = A, to = B}; Y = A + B        → 3; 4; 5
Y where test.Edge {from = A, to = B}; Y = A + B - 1    → 2; 3; 4
Z where test.Count X; Y = X + 1; Z = Y + 1   → -9223372036854775806; -40; 9; 1002
X where test.Count X; Y = X + 1; Y > 8       → 1000
```

Flat and left to right — three operands and two operators are one step. Subtraction
**wraps**: `i64::MIN - 1` is `i64::MAX`, because every `i64` is a legal value and the type
model has no arithmetic error for a query to receive. A chain reads the previous register
rather than re-deriving it. A computed value compared against a constant is a `Test` step,
because neither side is a row and there is no level to hang a residual on.

### Negation: `!`

A statement prefix. It binds nothing, takes no cursor entry, and each source is drained only
to its **first** row — the question is whether a witness exists, not how many.

```sigla
X where test.Foo {id = X}; !test.Bar {id = X}                        → 3
X where !test.Bar {id = X}; test.Foo {id = X}                        → 3
X where test.Foo {id = X}; !(test.Bar {id = X} | test.Edge {from = X, to = _})   → 3
X where test.Bar {id = X}; !never                                    → 1; 2
```

The second line is the placement rule made visible: `X` is a *read* of the negation, so the
frontier cannot run it before the statement that binds `X`. An unbound variable therefore
never acts as a wildcard — and a variable occurring **only** inside a negation is refused
(`reject/unbound-variable`), because `!(Q _)` already spells "no `Q` at all" and the two
readings of `!(Q X)` are indistinguishable at a glance.

### Disjunction: `|`

One level with an alternative per branch, tried in order, and the rows are the branches'
concatenated — not merged and not deduplicated.

```sigla
X where test.Foo {id = X} | test.Bar {id = X}     → 1; 2; 3; 1; 2
```

It is flat and n-ary, never a right-leaning tree, and it is **never DNF-expanded** across
sibling conjuncts.

### `never`

The empty relation: a level with no alternative to open, exhausted the moment it is entered.

```sigla
X where X = never                            → (no rows)
```

`never`, an ordinary scan and a disjunction are one node counted at 0, 1 and N — which is
why `never` needed no special case in the machine.

### Subqueries

A subquery in a generating position **inlines**: its statements become the enclosing query's,
and its head is the value the bind names.

```sigla
X where X = (Y where test.Foo {id = Y})              → 1; 2; 3
X where X = (Y where test.Name Y; Y != "a"..)        → bob
```

## Patterns

### Records

```sigla
{a = X, b = Y} where test.Foo {name = X, id = Y}     → {a = ann, b = 1}; {a = bob, b = 2}; {a = ann, b = 3}
X where test.Nested {outer = {inner = X}}            → 1; 7
X where P = test.Nested _; {inner = X} = P.outer     → 1; 7
X where P = test.Wide _; {extra = _, inner = X} = P.outer   → 2
X where {a = X} = {a = 1}                            → 1
```

A record on the left of a bind **destructures a place**: each piece names a piece of the row,
so it is the same plan as `X = P.outer.inner`. A wildcard piece binds nothing and cannot
fail. A record of constants folds, to any depth.

A query's record fields are sorted by name when it is lowered, so `{a = 1, b = 2}` and
`{b = 2, a = 1}` are one type and one set of bytes. (A *schema's* fields are never sorted —
that order is the key order.)

### Field access, and `.value`

```sigla
X.name where X = test.Foo _                  → ann; bob; ann
X.value where X = test.Foo _                 → one; two; three
X.value where X = test.Boxed _               → {lo = 10, hi = 20}; {lo = 30, hi = 40}
```

`.value` is the fact's value side. A record value projects whole — one point read, in the
shape the schema declares. A value can be **projected** but not **matched**
([I6](invariants.html#i6)), and a key field literally named `value` makes `.value` ambiguous
and is refused by name.

### Unions

A one-field record against a union-typed field is an **injection**: it names one
alternative and matches its payload. `.alt?` is the **select** — it checks the tag and binds
the payload, answering exactly what the injection does.

```sigla
X where test.Tagged {what = {num = X}, id = _}    → 1; 2
X where test.Tagged {what = {text = X}, id = _}   → a; b
X where test.Tagged {what = {num = _}, id = X}    → 10; 30
Y where test.Label X; Y = X.what.num?             → 1; 2
X where test.Label {id = X, what = {num = 2}}     → 30
```

Three things the examples pin:

- **A tag is not a position.** `num` is discriminant 3 and `text` is 0; a reader taking a
  tag for an index would answer the `text` rows for the `num` query.
- **Where the union sits in the key decides what the match costs.** `test.Tagged` leads with
  it, so naming an alternative is a *prefix* of the key order — a seek (the profile says 2
  rows examined of 4). In `test.Label` the tag lands after the seek prefix has closed and
  filters instead.
- **A wildcard payload is still a seek** — the tag alone is the shortest prefix an
  alternative has.

A select against the wrong alternative is an **error**, never another type's bytes: the
expected discriminant is checked before any read through the payload. And rows whose
alternative the query never mentions pass untouched — an unmentioned union field is a
wildcard.

### Literals

```sigla
X where X = test.Count -42                   → test.Count#2
X where X = test.Count -9223372036854775808  → test.Count#1
X where X = test.Count 1_000                 → test.Count#4
X where X = test.Name "abc"..                → test.Name#1
```

`i64::MIN` is reachable only through the unary minus, because the literal itself does not fit
`i64`. Underscores separate digits. Malformed literals are lexed permissively and rejected in
lowering with a code — `lit/int-leading-zero`, `lit/int-underscore`, `lit/int-range`,
`lit/string-escape` — rather than by a silent reinterpretation.

## References

A reference is where a fact database earns its keep, and there are **two different
operations** on one:

**Following** a reference is a compare against an id already in a register. It reads
nothing:

```sigla
P where P = test.Foo {id = 1}; test.Ref {of = P}                 → test.Foo#1
X where X = test.Ref {of = test.Foo {id = 1}}                    → test.Ref#1
X where X = test.Deep {via = test.Ref {of = test.Foo {id = 1}}}  → test.Deep#1
```

A fact pattern inside another is a generator, hoisted into a loop level of its own and
matched by id — recursively, innermost first. That is the idiomatic spelling of the join.

**Reading through** a reference needs the target fact fetched, because its fields are in
another fact's key:

```sigla
X.name where test.Ref {of = X}                       → ann; bob
X.value where test.Ref {of = X}                      → one; two
N where test.Deep {via = R}; N = R.of.name           → ann; bob
{a = X.id, b = X.name} where test.Ref {of = X}       → {a = 1, b = ann}; {a = 2, b = bob}
P.id where test.Ref {of = P}; test.Bar {id = P.id}   → 1; 2
```

That compiles to a **fetch level** — one point read per row of the level above it — binding
the fetched fact as an ordinary register from there on. Two reads of one reference are *one*
fetch. A chain of references is a chain of fetches. And because the fetch is an outer level,
its register can splice into the seek of a level below it.

:::note Why the two stay distinct in the plan
A register holds its own row's key bytes *and* the ids inside them. Splicing key bytes where
an id belongs compares a key against an id and quietly matches nothing — the characteristic
bug of this design. The IR keeps `Fetch` and an id compare as different things so that cannot
be written by accident.
:::

A row on the way out carries a reference as a `FactId` (`#4:1`). To see the fact it names, ask
the client to expand it — `query --expand`, `:expand` — which is a protocol question, not a
query one. See [A query, step by step](query-lifecycle.html#expanding-a-reference).

## The head

The head is a pattern, projected once per row:

```sigla
X where …                    a variable — the whole row, or the whole value it names
X.name where …               a field of a bound row
{a = X, b = Y.name} where …  a record built out of pieces
test.Bar {id = 1} where …    a fact pattern — hoisted into the last level, projected as the fact
```

A wildcard head projects nothing (`reject/wildcard-in-head`), and a string prefix is a
pattern rather than a value, so it cannot be a head (`reject/not-projectable`).

## How a query becomes fast: seek, splice, filter

Per key field, in declaration order, `flatten` decides one of three things — and the decision
is **order-dependent**, which is why `reorder` runs first:

| Decision | When | Cost |
|---|---|---|
| **seek** | The field is a constant (or a constrained range) and every field before it is too | Narrows the scan to a prefix or a range |
| **splice** | The field's value is in a register an *earlier* level bound | Narrows the inner scan to rows matching the outer row — this is the join |
| **filter** (residual) | Anything after the seek prefix has closed: a capture, or a constant behind one | Read the row, then test it |

The moment a key field is *captured* (bound to a variable), the seek prefix closes and
everything after it filters:

```sigla
X where X = test.Foo {id = 1}                → test.Foo#1        (a seek)
X where test.Foo {id = X, name = "ann"}      → 1; 3              (a scan, then a filter)
```

`:plan` shows exactly which happened, and `--profile` shows what it cost:

```plan
  r0 <- src.Decl scan
       where name == "symbol_0000000_000"
  r1 <- src.Ref seek[to = r0#, file = _, at = _]
  head {f = r1.file, l = r1.at.line}
```

```text
STEP      EXAMINED
src.Decl  1000      full scan
src.Ref   1
1001 examined, 1 produced
```

If a question you ask often reads far more rows than it produces, the answer is usually the
**schema**, not the query: declare the same data keyed for that question. See
[Schema language](schema-language.html#field-order-is-the-index-design).

## Errors and refusals

Diagnostics carry a **code**, and the code is what tests assert on, so wording can improve
without breaking anything. There are three families:

| Prefix | Means |
|---|---|
| `reject/…` | Meaningless, and rejected for good |
| `nyi/…` | Parses, has a meaning, not implemented yet |
| `lit/…` | A malformed literal |

```text
error[reject/unknown-predicate]: `src.Nope` is not a predicate in this schema
  ┌─ <input>:1:9
  │
1 │ X where src.Nope X
  │         ^^^^^^^^^^
```

### Rejections you might hit

| Code | Example | Why |
|---|---|---|
| `reject/unbound-variable` | `X where test.Foo _` | Range restriction: nothing captures `X`, so there are no values for it to range over |
| `reject/unknown-field` | `X where test.Foo {nosuch = X}` | Not a field of the predicate's key |
| `reject/unknown-predicate` | `X where X = nosuch.Pred _` | Not in the schema |
| `reject/type-mismatch` | `N where test.Name N; N < 3` | The two sides of a comparison unify |
| `reject/type-mismatch` | `X where X = test.Foo _; X < 3` | A whole **row** has no order — an id is an allocation sequence, and exposing it as an order would be a trap |
| `reject/no-value` | `X.value where X = test.Bar _` | The predicate is key-only |
| `reject/value-shadowed` | `X.value where X = test.Shadow _` | The key has a field named `value`, so `.value` is ambiguous |
| `reject/duplicate-field` | `X where test.Foo {name = X, name = Y}` | Record fields are a set; a duplicate is an error, not last-one-wins |
| `reject/not-a-generator` | `X where X = test.Foo _; 42` | A statement that is neither generates nor constrains anything |
| `reject/bind-lhs` | `X where 42 = test.Foo _` | A literal cannot be a bind target |
| `reject/wildcard-in-head` | `_ where test.Foo _` | A wildcard head projects nothing |
| `reject/not-a-union` | `X.alt? where X = test.Foo _` | A select on something that is not a union at all |
| `reject/unknown-alternative` | `X where test.Tagged {what = {nosuch = X}, id = _}` | A name the union does not declare — the same class of mistake as an unknown field |
| `reject/union-arity` | `X where test.Tagged {what = {num = X, text = _}, id = _}` | Two alternatives at once is what a *record* of two fields means, and a union cannot |

### Not implemented yet

| Code | Example | What is missing |
|---|---|---|
| `nyi/repeated-variable` | `X where test.Edge {from = X, to = X}` | An intra-row repeat needs a same-row residual operator; rejected for now rather than adding one nothing else uses |
| `nyi/value-match` | `Y where test.Foo {id = X}; test.Bar {id = Y + 1}` | Matching a key field against a **computed** value — a seek compares bytes known at compile time |
| `nyi/value-match` | `X where X = test.Foo _; X.value < "b"` | A value has no residual: its bytes are in the identity map, which I6 keeps out of the scan loop |
| `nyi/value-bind` | `X where test.Nested {outer = {inner = Y}}; X = {inner = Y}` | A record mentioning a captured variable is in no register and would have to be **built** |
| `nyi/value-field` | `X.value.lo where X = test.Boxed _` | A field *inside* a value: a value is fetched whole, so there is no register to walk a path in |
| `nyi/bind-unification` | `X where X = test.Foo _; X.name != "a"..` | An access chain on the left is pattern-pushing. Write `Y = X.name; Y != "a"..` |
| `nyi/negation` | `X where test.Foo {id = X}; !(Y where test.Bar {id = Y})` | A negated **subquery** — a level inside a test |
| `nyi/negation` | `P where …; !test.Ref {of = test.Foo {id = 2}}` | A generator inside a negation's key: hoisting it out would change the answer when it matches nothing |
| `nyi/fact-field` | reading through a reference held in a fact's **value** | A fetch follows a reference in a key |
| `nyi/whole-key` | matching a whole key against a record field | A stored key is flat and a record field is wrapped, so the two are not the same bytes |

Most of these have a one-line workaround, and the diagnostic says it. The pattern to
remember: **name the intermediate value in a statement of its own.** An alias costs nothing —
no register, no step — so `Y = X.name; Y != "a"..` compiles to exactly the plan the refused
spelling would have wanted.

## What sigla does not have

Not as a gap list — as a design boundary:

- **No recursion.** Every negation is therefore evaluated against a relation that is already
  total, which is what makes stratification a non-problem here.
- **No aggregation.** No `count`, no `all`. `query --count` and `--format count` ask a
  question *about* an answer without adding one to the language, and aggregation is the one
  construct that cannot be made suspend-free — it materialises.
- **No `LIMIT`.** Bounding a result is the client's job: `--limit` and `:more` cancel and
  resume in band, and the query is unchanged.
- **No way to name a fact by its id.** An id is physical; putting one in the language would
  put a storage detail in a query. Expansion happens on the protocol instead.
- **No if-then-else.** `(C; T) | (!C; E)` is the desugaring, and it needs no machinery.
