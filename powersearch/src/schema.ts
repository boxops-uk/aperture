// A @traversable/schema is, at its core, a type-narrowing predicate.
// We depend ONLY on that structural fact — not on any nominal library type —
// which is what lets any schema (a t.* combinator OR a hand-written refinement)
// slot in, and lets us recover T structurally from the predicate signature.

/** Loose constraint: any predicate is a valid schema (t.foo is assignable to this). */
export type AnySchema = (u: unknown) => boolean;

/** A schema known to narrow to T. */
export type Schema<T = unknown> = (u: unknown) => u is T;

/** Recover the narrowed type from any predicate-shaped schema. */
export type TypeOf<S> = S extends (u: any) => u is infer T ? T : unknown;
