import {
  $getRoot,
  $createParagraphNode,
  $isParagraphNode,
  $getSelection,
  $isNodeSelection,
  $isRangeSelection,
  $isElementNode,
  $isTextNode,
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

      // Adjacent-token lookup for a collapsed caret: paragraphs here hold no
      // text (only token decorators), so the caret's anchor is an ELEMENT
      // point (paragraph, childIndex), not a text offset — except right by a
      // typeahead's in-progress query text, where it's a TEXT point instead.
      const $tokenAdjacentToCaret = (
        forward: boolean,
      ): SearchTokenNode | null => {
        const sel = $getSelection();
        if (!$isRangeSelection(sel) || !sel.isCollapsed()) return null;
        const anchor = sel.anchor;
        if (anchor.type === "element") {
          const parent = anchor.getNode();
          if (!$isElementNode(parent)) return null;
          const idx = forward ? anchor.offset : anchor.offset - 1;
          const sibling = parent.getChildAtIndex(idx);
          return $isTokenNode(sibling) ? sibling : null;
        }
        const node = anchor.getNode();
        if (!$isTextNode(node)) return null;
        const atEdge = forward
          ? anchor.offset === node.getTextContentSize()
          : anchor.offset === 0;
        if (!atEdge) return null;
        const sibling = forward
          ? node.getNextSibling()
          : node.getPreviousSibling();
        return $isTokenNode(sibling) ? sibling : null;
      };

      // Deletes tokens store-first: whether the token arrived at "selected"
      // via a click (NodeSelection) or the caret merely sits next to it
      // (collapsed RangeSelection), the store is the one place removal is
      // dispatched from. Two things are required to make that stick, not
      // just returning true:
      //  - preventDefault() the native event. KEY_BACKSPACE/DELETE_COMMAND
      //    fire on keydown, before the browser's own contenteditable
      //    deletion runs; without preventDefault that native deletion still
      //    fires on the decorator's DOM (browsers treat a contenteditable
      //    "false" island as one atomic unit to delete), racing our
      //    store-driven reconcile and corrupting the DOM out from under
      //    Lexical's model — observed as a second, untouched sibling token
      //    vanishing along with the intended one.
      //  - dispatching to the store at all. Left to Lexical's own default
      //    backspace/delete handling (i.e. if we returned false here), the
      //    adjacent decorator gets removed from the EDITOR tree directly,
      //    without telling the store — the store then re-adds it on the
      //    next reconcile (e.g. accepting a typeahead), since it never
      //    learned the token was gone.
      const makeOnDelete =
        (forward: boolean) =>
        (event: KeyboardEvent): boolean => {
          const sel = $getSelection();
          if ($isNodeSelection(sel)) {
            const tokens = sel.getNodes().filter($isTokenNode);
            if (tokens.length === 0) return false;
            event.preventDefault();
            for (const t of tokens)
              store.dispatch({
                type: "removeSubtree",
                qid: t.getQid(),
              } as never);
            return true;
          }
          const adjacent = $tokenAdjacentToCaret(forward);
          if (!adjacent) return false;
          event.preventDefault();
          store.dispatch({
            type: "removeSubtree",
            qid: adjacent.getQid(),
          } as never);
          return true;
        };
      const offB = editor.registerCommand(
        KEY_BACKSPACE_COMMAND,
        makeOnDelete(false),
        COMMAND_PRIORITY_LOW,
      );
      const offD = editor.registerCommand(
        KEY_DELETE_COMMAND,
        makeOnDelete(true),
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
