import { useMemo } from "react";
import { LexicalExtensionComposer } from "@lexical/react/LexicalExtensionComposer";
import { useSearchRuntime, useRootId } from "./SearchContext";
import { leafKeywordsOf, branchKeywordsOf } from "./operators";
import { buildBranchExtension } from "./EditorExtension";

// Root editor: buildBranchExtension with no parent. Everything else recurses.
export function SearchBar() {
  const { store, registry } = useSearchRuntime();
  const rootId = useRootId();
  const extension = useMemo(
    () =>
      buildBranchExtension(
        store,
        registry,
        rootId,
        leafKeywordsOf(registry),
        branchKeywordsOf(registry),
      ),
    [store, registry, rootId],
  );
  return <LexicalExtensionComposer extension={extension} />;
}
