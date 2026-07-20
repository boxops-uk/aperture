import { useEffect, useRef, type ReactNode } from "react";
import type { Registry, Algebra, BranchKeys } from "./operators";
import type { QueryState, Qid, QNode } from "./query";
import {
  createSearchStore,
  createInitialState,
  type SearchStore,
  type Action,
} from "./store";
import { wireSearch, type SearchMeta } from "./searchRunner";
import {
  SearchContext,
  useStore as useStoreG,
  useNode as useNodeG,
  useChildIds,
  useRootId,
  useDispatch as useDispatchG,
  useQueryFold as useQueryFoldG,
  useNodeFold as useNodeFoldG,
} from "./SearchContext";

/**
 * Binds a registry once and returns a typed kit for app ergonomics. It no longer
 * owns a private context: SearchRoot publishes {store, registry} into the shared
 * SearchContext, so registry-agnostic consumers (the Lexical classes) read the
 * same runtime. The store handle is a stable ref, so context propagation causes
 * zero re-renders; narrowing is via per-qid selectors in the generic hooks.
 */
export function createSearchKit<R extends Registry>(registry: R) {
  interface SearchRootProps<Res> {
    searchAlgebra: Algebra<R, Res>;
    rootEmpty: Res;
    rootOp: BranchKeys<R>;
    initialState?: QueryState<R>;
    debounceMs?: number;
    onDebouncedChange?: (
      r: Res,
      meta: SearchMeta,
      signal: AbortSignal,
    ) => void | Promise<void>;
    onQueryChange?: (serialized: unknown) => void;
    children: ReactNode;
  }

  function SearchRoot<Res>(props: SearchRootProps<Res>) {
    const {
      searchAlgebra,
      rootEmpty,
      rootOp,
      initialState,
      debounceMs,
      onDebouncedChange,
      onQueryChange,
      children,
    } = props;

    // Store created exactly once; stable across re-renders and per-instance, so
    // multiple <SearchRoot>s (with the same OR different registries) stay independent.
    const ref = useRef<SearchStore<R> | null>(null);
    if (ref.current === null) {
      ref.current = createSearchStore(
        initialState ?? createInitialState(registry, rootOp),
      );
    }
    const store = ref.current;

    useEffect(
      () =>
        wireSearch(store, {
          registry,
          searchAlgebra,
          rootEmpty,
          debounceMs,
          onSearch: onDebouncedChange,
          onQueryChange,
        }),
      [
        store,
        searchAlgebra,
        rootEmpty,
        debounceMs,
        onDebouncedChange,
        onQueryChange,
      ],
    );

    // one shared context; the value is this bar's runtime
    return (
      <SearchContext.Provider value={{ store, registry }}>
        {children}
      </SearchContext.Provider>
    );
  }

  // Typed aliases over the generic hooks (re-narrow the erased reads to R).
  return {
    SearchRoot,
    useStore: () => useStoreG<R>(),
    useNode: (qid: Qid) => useNodeG(qid) as QNode<R> | undefined,
    useChildIds,
    useRootId,
    useDispatch: () => useDispatchG() as (a: Action<R>) => void,
    useQueryFold: <Res,>(alg: Algebra<R, Res>, rootEmpty: Res) =>
      useQueryFoldG<R, Res>(alg, rootEmpty),
    useNodeFold: <Res,>(qid: Qid, alg: Algebra<R, Res>, rootEmpty: Res) =>
      useNodeFoldG<R, Res>(qid, alg, rootEmpty),
  };
}
