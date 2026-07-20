import { t } from "@traversable/schema";
import type { Registry, LeafKeys, BranchKeys, LeafPred } from "./operators";
import type { QNode, QueryState, Qid } from "./query";
import { newQid } from "./query";

const isProd = (globalThis as any)?.process?.env?.NODE_ENV === "production";

// ---- node factories (read defaults off the registry) ----
export function createLeaf<R extends Registry, K extends LeafKeys<R>>(
  reg: R,
  op: K,
): { id: Qid; kind: "leaf"; op: K; predicate: LeafPred<R, K>; value: unknown } {
  const def = reg[op] as any;
  return {
    id: newQid(),
    kind: "leaf",
    op,
    predicate: def.defaultPredicate,
    value: def.defaultValue,
  };
}

export function createBranch<R extends Registry, K extends BranchKeys<R>>(
  _reg: R,
  op: K,
  childIds: Qid[] = [],
): { id: Qid; kind: "branch"; op: K; childIds: Qid[] } {
  return { id: newQid(), kind: "branch", op, childIds };
}

export function createInitialState<R extends Registry, K extends BranchKeys<R>>(
  reg: R,
  rootOp: K,
): QueryState<R> {
  const root = createBranch(reg, rootOp);
  return {
    version: 1,
    rootId: root.id,
    nodes: { [root.id]: root } as Record<Qid, QNode<R>>,
  };
}

// ---- actions ----
export type Action<R extends Registry> =
  | { type: "setValue"; qid: Qid; value: unknown } // the per-keystroke path
  | { type: "setPredicate"; qid: Qid; predicate: string }
  | { type: "insertChild"; parentQid: Qid; node: QNode<R>; index?: number }
  | { type: "removeSubtree"; qid: Qid };

// ---- reducer (pure; structural sharing) ----
export function reducer<R extends Registry>(
  state: QueryState<R>,
  action: Action<R>,
): QueryState<R> {
  switch (action.type) {
    case "setValue": {
      const n = state.nodes[action.qid];
      if (!n || n.kind !== "leaf") return state;
      return {
        ...state,
        nodes: { ...state.nodes, [action.qid]: { ...n, value: action.value } },
      };
    }
    case "setPredicate": {
      const n = state.nodes[action.qid];
      if (!n || n.kind !== "leaf") return state;
      return {
        ...state,
        nodes: {
          ...state.nodes,
          [action.qid]: { ...n, predicate: action.predicate as any },
        },
      };
    }
    case "insertChild": {
      const parent = state.nodes[action.parentQid];
      if (!parent || parent.kind !== "branch") return state;
      const index = action.index ?? parent.childIds.length;
      const childIds = parent.childIds.slice();
      childIds.splice(index, 0, action.node.id);
      return {
        ...state,
        nodes: {
          ...state.nodes,
          [action.node.id]: action.node,
          [action.parentQid]: { ...parent, childIds },
        },
      };
    }
    case "removeSubtree": {
      const doomed = collectSubtree(state, action.qid);
      if (doomed.size === 0) return state;
      const nodes: Record<Qid, QNode<R>> = {};
      for (const [id, n] of Object.entries(state.nodes)) {
        if (doomed.has(id)) continue;
        nodes[id] =
          n.kind === "branch" && n.childIds.some((c) => doomed.has(c))
            ? { ...n, childIds: n.childIds.filter((c) => !doomed.has(c)) }
            : n;
      }
      return { ...state, nodes };
    }
  }
}

function collectSubtree<R extends Registry>(
  state: QueryState<R>,
  root: Qid,
): Set<Qid> {
  const out = new Set<Qid>();
  const stack: Qid[] = [root];
  while (stack.length) {
    const id = stack.pop() as Qid;
    const n = state.nodes[id];
    if (!n || out.has(id)) continue;
    out.add(id);
    if (n.kind === "branch") for (const c of n.childIds) stack.push(c);
  }
  return out;
}

// ---- referential integrity (schema can't see refs, so we must) ----
export function checkIntegrity<R extends Registry>(state: QueryState<R>): void {
  if (!state.nodes[state.rootId])
    throw new Error(`integrity: rootId '${state.rootId}' missing`);
  const refCount = new Map<Qid, number>();
  for (const [id, n] of Object.entries(state.nodes)) {
    if (n.id !== id)
      throw new Error(`integrity: node keyed '${id}' carries id '${n.id}'`);
    if (n.kind === "branch")
      for (const c of n.childIds) {
        if (!state.nodes[c])
          throw new Error(`integrity: '${id}' -> missing child '${c}'`);
        refCount.set(c, (refCount.get(c) ?? 0) + 1);
      }
  }
  for (const id of Object.keys(state.nodes)) {
    const rc = refCount.get(id) ?? 0;
    if (id === state.rootId) {
      if (rc !== 0) throw new Error(`integrity: root '${id}' is referenced`);
    } else if (rc !== 1)
      throw new Error(`integrity: '${id}' referenced ${rc}x (want 1)`);
  }
}

// ---- external store (useSyncExternalStore-shaped) ----
export interface SearchStore<R extends Registry> {
  getState(): QueryState<R>; // STABLE ref; never folds/derives
  subscribe(listener: () => void): () => void;
  dispatch(action: Action<R>): void;
}

export function createSearchStore<R extends Registry>(
  initial: QueryState<R>,
): SearchStore<R> {
  let state = initial;
  const listeners = new Set<() => void>();
  return {
    getState: () => state,
    subscribe(listener) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    dispatch(action) {
      const next = reducer(state, action);
      if (next === state) return; // no-op: skip notify
      if (!isProd) checkIntegrity(next); // fail loud at the mutation that broke it
      state = next;
      for (const l of listeners) l();
    },
  };
}

// ---- serialization boundary ----
// Canonical form is already plain JSON.
export function serialize<R extends Registry>(state: QueryState<R>): unknown {
  return { version: state.version, rootId: state.rootId, nodes: state.nodes };
}

// Derived structural gate: a traversable union built FROM the registry, so it
// cannot drift from the operator set. `value` is permissive (draft survives
// reload); per-field validity is the fold's job. Swap the boolean predicate for
// `.validate` here if you want structured per-node diagnostics.
export function buildStoreSchema<R extends Registry>(
  reg: R,
): (u: unknown) => boolean {
  const parts: any[] = [];
  for (const [op, def] of Object.entries(reg)) {
    parts.push(
      (def as any).kind === "leaf"
        ? t.object({
            id: t.string,
            kind: t.eq("leaf"),
            op: t.eq(op),
            predicate: (t as any).enum(...(def as any).predicates),
            value: t.unknown,
          })
        : t.object({
            id: t.string,
            kind: t.eq("branch"),
            op: t.eq(op),
            childIds: t.array(t.string),
          }),
    );
  }
  return (t as any).union(...parts) as (u: unknown) => boolean;
}

export function deserialize<R extends Registry>(
  reg: R,
  raw: unknown,
): QueryState<R> {
  if (typeof raw !== "object" || raw === null)
    throw new Error("deserialize: not an object");
  const obj = raw as any;
  if (
    typeof obj.rootId !== "string" ||
    typeof obj.nodes !== "object" ||
    obj.nodes === null
  )
    throw new Error("deserialize: missing rootId/nodes");
  const nodeSchema = buildStoreSchema(reg);
  for (const [id, n] of Object.entries(obj.nodes))
    if (!nodeSchema(n))
      throw new Error(`deserialize: node '${id}' failed structural schema`);
  const state: QueryState<R> = {
    version: obj.version ?? 1,
    rootId: obj.rootId,
    nodes: obj.nodes,
  };
  checkIntegrity(state);
  return state;
}
