import {
  createContext,
  useContext,
  useMemo,
  useSyncExternalStore,
} from "react";
import type { Registry, Algebra } from "./operators";
import type { AnyQNode, Qid } from "./query";
import type { SearchStore, Action } from "./store";
import { foldQuery, type FoldResult } from "./fold";

const EMPTY: readonly Qid[] = [];

/**
 * The per-search-bar runtime: the store handle AND the registry it is over. This
 * is what makes the Lexical classes registry-agnostic — they read {store,
 * registry} from here rather than importing a specific kit. Each <SearchRoot>
 * supplies its own value into this ONE shared context, so distinct search bars
 * can run distinct registries; the nearest provider wins. Typed as `any` at this
 * seam (the erasure boundary); the typed kit re-establishes types via casts.
 */
export interface SearchRuntime<R extends Registry = Registry> {
  readonly store: SearchStore<R>;
  readonly registry: R;
}

export const SearchContext = createContext<SearchRuntime<any> | null>(null);

export function useSearchRuntime<
  R extends Registry = Registry,
>(): SearchRuntime<R> {
  const rt = useContext(SearchContext);
  if (!rt) throw new Error("search hooks must be used within <SearchRoot>");
  return rt as SearchRuntime<R>;
}

export function useStore<R extends Registry = Registry>(): SearchStore<R> {
  return useSearchRuntime<R>().store;
}
export function useRegistry<R extends Registry = Registry>(): R {
  return useSearchRuntime<R>().registry;
}

// ---- Narrow per-qid subscriptions -----------------------------------------
// getSnapshot returns the STORED ref (structural sharing keeps untouched nodes'
// refs stable -> no re-render). Never transform in the snapshot (a fresh
// array/object each call would loop useSyncExternalStore). Reads are through the
// registry-agnostic AnyQNode shape.

export function useNode(qid: Qid): AnyQNode | undefined {
  const store = useSearchRuntime().store as SearchStore<any>;
  const snap = () => store.getState().nodes[qid] as AnyQNode | undefined;
  return useSyncExternalStore(store.subscribe, snap, snap);
}

export function useChildIds(qid: Qid): readonly Qid[] {
  const store = useSearchRuntime().store as SearchStore<any>;
  const snap = () => {
    const n = store.getState().nodes[qid] as AnyQNode | undefined;
    return n && n.kind === "branch" ? n.childIds : EMPTY;
  };
  return useSyncExternalStore(store.subscribe, snap, snap);
}

export function useRootId(): Qid {
  const store = useSearchRuntime().store as SearchStore<any>;
  const snap = () => store.getState().rootId;
  return useSyncExternalStore(store.subscribe, snap, snap);
}

export function useDispatch(): (a: Action<any>) => void {
  return (useSearchRuntime().store as SearchStore<any>).dispatch;
}

// ---- Live folds (NOT debounced). Fold in useMemo, NEVER in getSnapshot. -----
// registry comes from context too, so folds are registry-agnostic as well.

export function useQueryFold<R extends Registry, Res>(
  alg: Algebra<R, Res>,
  rootEmpty: Res,
): FoldResult<Res> {
  const { store, registry } = useSearchRuntime<R>();
  const state = useSyncExternalStore(
    store.subscribe,
    store.getState,
    store.getState,
  );
  return useMemo(
    () => foldQuery(registry, alg, state, rootEmpty),
    [state, alg, rootEmpty, registry],
  );
}

export function useNodeFold<R extends Registry, Res>(
  qid: Qid,
  alg: Algebra<R, Res>,
  rootEmpty: Res,
): FoldResult<Res> {
  const { store, registry } = useSearchRuntime<R>();
  const state = useSyncExternalStore(
    store.subscribe,
    store.getState,
    store.getState,
  );
  return useMemo(
    () => foldQuery(registry, alg, { ...state, rootId: qid }, rootEmpty),
    [state, qid, alg, rootEmpty, registry],
  );
}
