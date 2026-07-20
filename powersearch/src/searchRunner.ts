// Framework-agnostic search + persistence wiring. React's SearchRoot just calls
// this from a useEffect. Keeping it out of React makes it unit-testable and keeps
// the "debounce the side-effect, not the model" rule in one place.
import type { Registry, Algebra } from "./operators";
import type { SearchStore } from "./store";
import { serialize } from "./store";
import { foldQuery } from "./fold";

export interface SearchMeta {
  isEmpty: boolean;
  isPartial: boolean;
}

export interface WireSearchOptions<R extends Registry, Res> {
  registry: R;
  searchAlgebra: Algebra<R, Res>;
  rootEmpty: Res;
  debounceMs?: number;
  /** Debounced: folds the search algebra and fires with a fresh AbortSignal. */
  onSearch?: (
    r: Res,
    meta: SearchMeta,
    signal: AbortSignal,
  ) => void | Promise<void>;
  /** Synchronous per-commit: the serialized store for persistence (throttle in-consumer if desired). */
  onQueryChange?: (serialized: unknown) => void;
  /** Fire an initial search on wire-up so consumers can seed results. */
  runOnStart?: boolean;
}

export function wireSearch<R extends Registry, Res>(
  store: SearchStore<R>,
  opts: WireSearchOptions<R, Res>,
): () => void {
  const {
    registry,
    searchAlgebra,
    rootEmpty,
    debounceMs = 250,
    onSearch,
    onQueryChange,
    runOnStart = true,
  } = opts;

  let timer: ReturnType<typeof setTimeout> | undefined;
  let controller: AbortController | null = null;

  const runSearch = () => {
    if (!onSearch) return;
    const { result, isEmpty, isPartial } = foldQuery(
      registry,
      searchAlgebra,
      store.getState(),
      rootEmpty,
    );
    controller?.abort(); // supersede any in-flight search -> latest wins
    controller = new AbortController();
    // fire-and-forget; a failed search must never break the editor
    Promise.resolve(
      onSearch(result, { isEmpty, isPartial }, controller.signal),
    ).catch(() => {});
  };

  const onCommit = () => {
    onQueryChange?.(serialize(store.getState())); // persistence: synchronous (model is never stale)
    if (onSearch) {
      clearTimeout(timer);
      timer = setTimeout(runSearch, debounceMs);
    } // search: debounced
  };

  const unsub = store.subscribe(onCommit);
  if (runOnStart) runSearch();

  return () => {
    clearTimeout(timer);
    controller?.abort();
    unsub();
  };
}
