import type { Registry, Algebra } from "./operators";
import type { QueryState, Qid } from "./query";

// Internal Option — emptiness propagates through the tree; the single monoid
// identity (rootEmpty) is applied exactly once, at the root.
type Option<A> = { some: true; value: A } | { some: false };
const NONE: Option<never> = { some: false };
const some = <A>(value: A): Option<A> => ({ some: true, value });

export interface FoldResult<Res> {
  result: Res;
  isEmpty: boolean;
  isPartial: boolean;
}

export function foldQuery<R extends Registry, Res>(
  reg: R,
  alg: Algebra<R, Res>,
  state: QueryState<R>,
  rootEmpty: Res,
): FoldResult<Res> {
  let isPartial = false;
  const A = alg as Record<string, unknown>;

  const go = (qid: Qid): Option<Res> => {
    const node = state.nodes[qid];
    if (!node) {
      isPartial = true;
      return NONE;
    } // dangling ref -> hole (total)

    if (node.kind === "leaf") {
      const op = reg[node.op] as { schema: (u: unknown) => boolean };
      if (!op.schema(node.value)) {
        isPartial = true;
        return NONE;
      } // partial leaf -> None
      // THE one witnessed cast: the keyword indexes both the schema that
      // validated `value` and the algebra clause that consumes it.
      const clause = A[node.op as string] as (p: string, v: unknown) => Res;
      return some(clause(node.predicate as string, node.value));
    }

    const kids: Res[] = [];
    for (const cid of node.childIds) {
      const r = go(cid);
      if (r.some) kids.push(r.value); // drop absent children
    }
    if (kids.length === 0) return NONE; // empty branch -> None
    const clause = A[node.op as string] as (children: Res[]) => Res;
    return some(clause(kids));
  };

  const top = go(state.rootId);
  return {
    result: top.some ? top.value : rootEmpty,
    isEmpty: !top.some,
    isPartial,
  };
}
