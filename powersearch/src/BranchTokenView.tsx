import { useMemo } from "react";
import { useLexicalComposerContext } from "@lexical/react/LexicalComposerContext";
import { LexicalExtensionComposer } from "@lexical/react/LexicalExtensionComposer";
import type { Qid } from "./query";
import { useSearchRuntime, useNode, useDispatch } from "./SearchContext";
import { leafKeywordsOf, branchKeywordsOf } from "./operators";
import { buildBranchExtension } from "./EditorExtension";

// A branch token = group chrome + a NESTED editor for this branch's own children.
// The nested editor is linked to the parent via $getParentEditor (from the
// surrounding composer context). Because the parent's reconcile keeps this
// token's NodeKey stable, this editor is built once and won't remount on sibling
// edits (verified headless).
export function BranchTokenView({ qid }: { qid: Qid }) {
  const [parentEditor] = useLexicalComposerContext();
  const { store, registry } = useSearchRuntime();
  const node = useNode(qid);
  const dispatch = useDispatch();

  const extension = useMemo(
    () =>
      buildBranchExtension(
        store,
        registry,
        qid,
        leafKeywordsOf(registry),
        branchKeywordsOf(registry),
        { $getParentEditor: () => parentEditor },
      ),
    [store, registry, qid, parentEditor],
  );

  if (!node || node.kind !== "branch") return null;

  return (
    <span className="bt-token" data-qid={qid}>
      <span className={`lt-chip bt-op bt-op--${node.op}`}>{node.op}</span>
      <span className="bt-editor">
        <LexicalExtensionComposer extension={extension} />
      </span>
      <button
        className="lt-chip lt-x"
        title="remove group"
        onClick={() => dispatch({ type: "removeSubtree", qid })}
      >
        ×
      </button>
    </span>
  );
}
