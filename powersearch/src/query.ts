import { nanoid } from "nanoid";
import type { Registry, LeafKeys, BranchKeys, LeafPred } from "./operators";

export type Qid = string;

export const newQid = () => nanoid();

// Node types are DERIVED from the registry: `op` is a literal keyword and
// `predicate` is narrowed to that operator's predicate union. `value` is stored
// as `unknown` because the store holds the DRAFT — it may be schema-invalid
// mid-type; the fold decides partiality.
export type LeafNode<R extends Registry> = {
  [K in LeafKeys<R>]: {
    id: Qid;
    kind: "leaf";
    op: K;
    predicate: LeafPred<R, K>;
    value: unknown;
  };
}[LeafKeys<R>];

export type BranchNode<R extends Registry> = {
  [K in BranchKeys<R>]: { id: Qid; kind: "branch"; op: K; childIds: Qid[] };
}[BranchKeys<R>];

export type QNode<R extends Registry> = LeafNode<R> | BranchNode<R>;

// Registry-agnostic projections of the node shapes. Every QNode<R> is assignable
// to these (op/predicate widen to string, childIds to readonly), so the generic
// hooks and Lexical classes can read nodes without knowing the registry, while
// the typed kit re-narrows to QNode<R> at its edges.
export type AnyLeafNode = {
  id: Qid;
  kind: "leaf";
  op: string;
  predicate: string;
  value: unknown;
};
export type AnyBranchNode = {
  id: Qid;
  kind: "branch";
  op: string;
  childIds: readonly Qid[];
};
export type AnyQNode = AnyLeafNode | AnyBranchNode;

export interface QueryState<R extends Registry> {
  version: number; // migration seam
  rootId: Qid;
  nodes: Record<Qid, QNode<R>>; // normalized flat map: O(1) per-keystroke writes
}
