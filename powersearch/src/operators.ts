import type { AnySchema, Schema, TypeOf } from "./schema";

// The Control is UI, which we're ignoring this milestone. We keep only its TYPE
// linkage to T (so schema <-> control can't drift) without importing React.
export type LeafControlProps<T> = {
  value: T | undefined;
  onChange: (value: T) => void;
};
export type LeafControl<T> = (props: LeafControlProps<T>) => unknown;

// ---- Operator definitions (the "functor" — R-free by construction) ----

export interface LeafDef<T, P extends string> {
  readonly kind: "leaf";
  readonly schema: Schema<T>; // canonical declaration of T
  readonly predicates: readonly P[]; // per-operator predicate choices
  readonly defaultPredicate: P;
  readonly defaultValue?: T;
  readonly Control: LeafControl<T>;
}

export interface BranchDef {
  readonly kind: "branch"; // P0: variadic, no params. Additive later.
}

/**
 * Smart constructor that SEALS the existential: T is known here (inferred from
 * `schema`), checked against Control/defaultValue, then erased from the public
 * surface. `const P` captures the predicate literal union.
 */
export function leaf<S extends AnySchema, const P extends string>(cfg: {
  schema: S;
  predicates: readonly [P, ...P[]]; // non-empty at the type level
  Control: LeafControl<TypeOf<S>>;
  defaultPredicate?: NoInfer<P>;
  defaultValue?: TypeOf<S>;
}): LeafDef<TypeOf<S>, P> {
  // A provided default is the leaf's initial value, so it must be a valid
  // instance of the schema — otherwise a freshly added token reads as partial
  // until first edited, and validity flips on touch. (Omitting defaultValue is
  // fine: `undefined` fails the guard by design, so the token starts partial.)
  if (cfg.defaultValue !== undefined && !cfg.schema(cfg.defaultValue)) {
    throw new Error("leaf(): defaultValue does not satisfy schema");
  }
  return {
    kind: "leaf",
    schema: cfg.schema as unknown as Schema<TypeOf<S>>,
    predicates: cfg.predicates,
    defaultPredicate: cfg.defaultPredicate ?? cfg.predicates[0],
    defaultValue: cfg.defaultValue,
    Control: cfg.Control,
  };
}

export function branch(_cfg: Record<never, never> = {}): BranchDef {
  return { kind: "branch" };
}

// ---- Registry ----

export type Registry = Record<string, LeafDef<any, any> | BranchDef>;

/** Identity + freeze. Keyword == object key, so uniqueness is automatic. */
export function defineOperators<const R extends Registry>(reg: R): Readonly<R> {
  return Object.freeze(reg);
}

// ---- Type-level projections over a registry ----

export type LeafKeys<R> = {
  [K in keyof R]: R[K] extends LeafDef<any, any> ? K : never;
}[keyof R];
export type BranchKeys<R> = {
  [K in keyof R]: R[K] extends BranchDef ? K : never;
}[keyof R];

export type LeafValue<R, K extends keyof R> =
  R[K] extends LeafDef<infer T, any> ? T : never;
export type LeafPred<R, K extends keyof R> =
  R[K] extends LeafDef<any, infer P> ? P : never;

/**
 * An interpretation. R is a FREE parameter here — not a member of the registry —
 * so you can define arbitrarily many algebras over the same operator set.
 * Each leaf clause receives THIS operator's predicate union and value type.
 * Branch clauses are pure semigroups over a guaranteed-non-empty child list.
 */
export type Algebra<R extends Registry, Res> = {
  [K in LeafKeys<R>]: (
    predicate: LeafPred<R, K>,
    value: LeafValue<R, K>,
  ) => Res;
} & { [K in BranchKeys<R>]: (children: Res[]) => Res };

/** Runtime list of the leaf-operator keywords in a registry. */
export function leafKeywordsOf<R extends Registry>(reg: R): LeafKeys<R>[] {
  return (Object.entries(reg) as [keyof R, LeafDef<any, any> | BranchDef][])
    .filter(([, d]) => d.kind === "leaf")
    .map(([k]) => k) as LeafKeys<R>[];
}

export function branchKeywordsOf<R extends Registry>(reg: R): BranchKeys<R>[] {
  return (Object.entries(reg) as [keyof R, LeafDef<any, any> | BranchDef][])
    .filter(([, d]) => d.kind === "branch")
    .map(([k]) => k) as BranchKeys<R>[];
}
