import {
  $getRoot,
  $createParagraphNode,
  $isParagraphNode,
  $getSelection,
  $isNodeSelection,
  KEY_BACKSPACE_COMMAND,
  KEY_DELETE_COMMAND,
  COMMAND_PRIORITY_LOW,
  defineExtension,
  type LexicalEditor,
  type LexicalNode,
  type ParagraphNode,
} from "lexical";
import type { Registry } from "./operators";
import type { SearchStore } from "./store";
import type { Qid } from "./query";
import {
  LeafTokenNode,
  $createLeafTokenNode,
  $isLeafTokenNode,
} from "./LeafTokenNode";
import {
  BranchTokenNode,
  $createBranchTokenNode,
  $isBranchTokenNode,
} from "./BranchTokenNode";

export const SYNC_TAG = "sync-from-store";

type SearchTokenNode = LeafTokenNode | BranchTokenNode;
const $isTokenNode = (
  n: LexicalNode | null | undefined,
): n is SearchTokenNode => $isLeafTokenNode(n) || $isBranchTokenNode(n);

const eq = (a: Qid[], b: readonly Qid[]) =>
  a.length === b.length && a.every((x, i) => x === b[i]);

/**
 * Project the store's children of `branchQid` onto the editor's token nodes,
 * BY KIND: a leaf child -> LeafTokenNode, a branch child -> BranchTokenNode.
 * Structural-only: if the ordered qids already match, no-op (value edits never
 * touch the editor). Unchanged tokens keep their NodeKey (append moves, not
 * recreates) so their decorators — including a branch's nested editor — don't
 * remount. Tagged so no listener mistakes this projection for a user edit.
 */
export function reconcileBranch<R extends Registry>(
  editor: LexicalEditor,
  store: SearchStore<R>,
  branchQid: Qid,
): void {
  editor.update(
    () => {
      const state = store.getState();
      const branch = state.nodes[branchQid];
      const desired: readonly Qid[] =
        branch && branch.kind === "branch" ? branch.childIds : [];

      const first = $getRoot().getFirstChild();
      let para: ParagraphNode;
      if ($isParagraphNode(first)) {
        para = first;
      } else {
        para = $createParagraphNode();
        $getRoot().clear().append(para);
      }

      const existing = para.getChildren().filter($isTokenNode);
      if (
        eq(
          existing.map((n) => n.getQid()),
          desired,
        )
      )
        return; // structural no-op

      const byQid = new Map(existing.map((n) => [n.getQid(), n]));
      const keep = new Set(desired);
      for (const n of existing) if (!keep.has(n.getQid())) n.remove();
      for (const qid of desired) {
        let n = byQid.get(qid);
        if (!n) {
          const kind = state.nodes[qid]?.kind;
          n =
            kind === "branch"
              ? $createBranchTokenNode(qid)
              : $createLeafTokenNode(qid);
        }
        para.append(n); // move existing (stable key) or append new, in desired order
      }
    },
    { tag: SYNC_TAG },
  );
}

/**
 * Bridge: initial hydrate + subscribe (store -> editor projection) + delete-key
 * interception (editor -> store, store-first). removeSubtree recurses, so
 * deleting a branch token drops its whole subtree.
 */
export function createStoreBridgeExtension<R extends Registry>(
  store: SearchStore<R>,
  branchQid: Qid,
) {
  return defineExtension({
    name: "search/store-bridge",
    register(editor: LexicalEditor) {
      reconcileBranch(editor, store, branchQid);
      const unsub = store.subscribe(() =>
        reconcileBranch(editor, store, branchQid),
      );

      const onDelete = (): boolean => {
        const sel = $getSelection();
        if (!$isNodeSelection(sel)) return false;
        const tokens = sel.getNodes().filter($isTokenNode);
        if (tokens.length === 0) return false;
        for (const t of tokens)
          store.dispatch({ type: "removeSubtree", qid: t.getQid() } as never);
        return true;
      };
      const offB = editor.registerCommand(
        KEY_BACKSPACE_COMMAND,
        onDelete,
        COMMAND_PRIORITY_LOW,
      );
      const offD = editor.registerCommand(
        KEY_DELETE_COMMAND,
        onDelete,
        COMMAND_PRIORITY_LOW,
      );
      return () => {
        unsub();
        offB();
        offD();
      };
    },
  });
}
