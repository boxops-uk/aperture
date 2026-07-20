import {
  $getRoot,
  $getSelection,
  $getNodeByKey,
  $isRangeSelection,
  $isElementNode,
  $isTextNode,
  defineExtension,
  safeCast,
  KEY_ARROW_LEFT_COMMAND,
  KEY_ARROW_RIGHT_COMMAND,
  COMMAND_PRIORITY_LOW,
  type LexicalEditor,
  type LexicalNode,
  type NodeKey,
} from "lexical";
import { mergeRegister } from "@lexical/utils";
import { getExtensionDependencyFromEditor } from "@lexical/extension";
import { $isLeafTokenNode } from "./LeafTokenNode";
import { $isBranchTokenNode } from "./BranchTokenNode";
import { useEffect } from "react";
import { useLexicalComposerContext } from "@lexical/react/LexicalComposerContext";

const Direction = {
  forward: "forward" as const,
  backward: "backward" as const,
} as const;

type Direction = (typeof Direction)[keyof typeof Direction];

const $isTokenNode = (n: LexicalNode | null | undefined): n is LexicalNode =>
  $isLeafTokenNode(n) || $isBranchTokenNode(n);

// ---- crossing-layer types --------------------------------------------------

/** Registered per token in its CONTAINING editor's map, keyed by its NodeKey. */
export interface TokenFocusHandle {
  readonly key: NodeKey;
  focusFirst(): void; // enter from the left  (forward motion) -> first stop / editor start
  focusLast(): void; // enter from the right (backward motion) -> last stop / editor end
}

/** A nested editor's link to the branch token that hosts it in the parent editor. */
export interface ParentLink {
  readonly editor: LexicalEditor;
  readonly hostKey: NodeKey;
}

export type HandleMap = Map<NodeKey, TokenFocusHandle>;

export interface FocusNavConfig {
  readonly parent: ParentLink | null;
}

// ---- editor-level zipper: adjacency + edge from the live selection ----------
// (This is "childIds split at the caret" in Lexical terms — derived, not stored.)

function $tokenAdjacentToCaret(dir: Direction): LexicalNode | null {
  const sel = $getSelection();
  if (!$isRangeSelection(sel) || !sel.isCollapsed()) {
    console.log(
      "[nav] $tokenAdjacentToCaret",
      dir,
      "-> null (no collapsed range selection)",
    );
    return null;
  }
  const p = sel.anchor;
  const node = p.getNode();
  let candidate: LexicalNode | null = null;
  if ($isTextNode(node)) {
    if (dir === Direction.forward)
      candidate =
        p.offset === node.getTextContentSize() ? node.getNextSibling() : null;
    else candidate = p.offset === 0 ? node.getPreviousSibling() : null;
  } else if ($isElementNode(node)) {
    candidate =
      dir === Direction.forward
        ? node.getChildAtIndex(p.offset)
        : node.getChildAtIndex(p.offset - 1);
  }
  const result = $isTokenNode(candidate) ? candidate : null;
  console.log("[nav] $tokenAdjacentToCaret", dir, {
    anchorType: p.type,
    anchorOffset: p.offset,
    anchorNodeType: node.getType(),
    anchorNodeKey: node.getKey(),
    candidateType: candidate?.getType() ?? null,
    candidateKey: candidate?.getKey() ?? null,
    isToken: result !== null,
  });
  return result;
}

function $atEditorEdge(dir: Direction): boolean {
  const sel = $getSelection();
  if (!$isRangeSelection(sel) || !sel.isCollapsed()) {
    console.log(
      "[nav] $atEditorEdge",
      dir,
      "-> false (no collapsed range selection)",
    );
    return false;
  }
  const p = sel.anchor;
  const node = p.getNode();
  let result: boolean;
  if (dir === Direction.forward) {
    if ($isElementNode(node)) result = p.offset === node.getChildrenSize();
    else if ($isTextNode(node))
      result =
        p.offset === node.getTextContentSize() &&
        node.getNextSibling() === null;
    else result = false;
  } else {
    if ($isElementNode(node)) result = p.offset === 0;
    else if ($isTextNode(node))
      result = p.offset === 0 && node.getPreviousSibling() === null;
    else result = false;
  }
  console.log("[nav] $atEditorEdge", dir, {
    anchorType: p.type,
    anchorOffset: p.offset,
    anchorNodeType: node.getType(),
    anchorNodeKey: node.getKey(),
    result,
  });
  return result;
}

// ---- caret placement across the editor boundary (selection part is headless-
// testable; the .focus() is the only DOM-only bit, deferred to onUpdate) ------

const $describeEl = (el: HTMLElement | null): string =>
  el
    ? `<${el.tagName.toLowerCase()} class="${el.className}" connected=${el.isConnected}>`
    : "null";

function focusEditorEdge(editor: LexicalEditor, dir: Direction): void {
  console.log(
    "[nav] focusEditorEdge start",
    dir,
    "rootEl=",
    $describeEl(editor.getRootElement()),
  );
  editor.update(
    () => {
      dir === Direction.forward
        ? $getRoot().selectStart()
        : $getRoot().selectEnd();
      const sel = $getSelection();
      console.log(
        "[nav] focusEditorEdge inside update, selection type=",
        sel?.constructor?.name,
        "isCollapsed=",
        $isRangeSelection(sel) ? sel.isCollapsed() : null,
      );
    },
    {
      onUpdate: () => {
        const el = editor.getRootElement();
        console.log(
          "[nav] focusEditorEdge onUpdate fired, rootEl=",
          $describeEl(el),
          "activeElement before focus()=",
          $describeEl(document.activeElement as HTMLElement | null),
        );
        el?.focus();
        console.log(
          "[nav] focusEditorEdge after focus(), activeElement=",
          $describeEl(document.activeElement as HTMLElement | null),
        );
      },
    },
  );
}

function escapeToParent(
  parent: LexicalEditor,
  hostKey: NodeKey,
  dir: Direction,
): void {
  parent.update(
    () => {
      const host = $getNodeByKey(hostKey);
      if (!host) return;
      // forward exit -> caret AFTER host; backward exit -> caret BEFORE host
      if (dir === Direction.forward) host.selectNext(0, 0);
      else host.selectPrevious();
    },
    { onUpdate: () => parent.getRootElement()?.focus() },
  );
}
// Its only call site (below, in onArrow) is commented out while debugging;
// this keeps it "used" for noUnusedLocals without changing its behavior.
void escapeToParent;

// ---- the extension ---------------------------------------------------------

export function getFocusHandles(editor: LexicalEditor): HandleMap {
  return getExtensionDependencyFromEditor(editor, FocusNavExtension).output;
}

let $regCounter = 0;
let $editorIdCounter = 0;
const $editorIds = new WeakMap<LexicalEditor, number>();
const $editorIdOf = (editor: LexicalEditor): number => {
  let id = $editorIds.get(editor);
  if (id === undefined) {
    id = ++$editorIdCounter;
    $editorIds.set(editor, id);
  }
  return id;
};

export const FocusNavExtension = defineExtension({
  name: "search/focus-nav",
  config: safeCast<FocusNavConfig>({ parent: null }),
  build: (): HandleMap => new Map(),
  register(editor: LexicalEditor, config: FocusNavConfig, state) {
    const regId = ++$regCounter;
    const handles = state.getOutput();
    const editorLabel = config.parent
      ? `nested#${regId}(parentHost=${config.parent.hostKey})`
      : `root#${regId}`;
    console.log(
      `[nav] register() called, editor=${editorLabel}, editorObjId=${$editorIdOf(editor)}, rootEl=`,
      $describeEl(editor.getRootElement()),
    );

    const onArrow =
      (dir: Direction) =>
      (_event: KeyboardEvent): boolean => {
        console.log(
          `[nav] ===== onArrow(${dir}) fired ===== editor=${editorLabel} rootElHTML=`,
          editor.getRootElement()?.outerHTML?.slice(0, 120),
        );
        const token = $tokenAdjacentToCaret(dir);
        if (token) {
          const handle = handles.get(token.getKey());
          if (!handle) {
            console.log(
              "[nav] intent: token adjacent but NO HANDLE registered for key",
              token.getKey(),
              "-> not trapping, default caret movement applies",
            );

            editor.update(() => {
              // If the adjacent token is a leaf, select it so that the next
              // arrow press will move past it. If it's a branch, do nothing
              // because the next arrow press will enter it.
              if ($isLeafTokenNode(token)) {
                if (dir === Direction.forward) token.selectNext(0, 0);
                else token.selectPrevious();
              }
            });

            return true; // no handle: let default (don't trap)
          }
          console.log(
            "[nav] intent: token adjacent, handle found for key",
            token.getKey(),
            "-> call",
            dir === Direction.forward
              ? "handle.focusFirst()"
              : "handle.focusLast()",
          );
          _event?.preventDefault();
          if (dir === Direction.forward) handle.focusFirst();
          else handle.focusLast();
          return true;
        }
        if ($atEditorEdge(dir)) {
          if (!config.parent) {
            console.log(
              "[nav] intent: at editor edge, no parent (root editor) -> not trapping, focus leaves normally",
            );
            return false; // root edge: focus leaves normally
          }
          console.log(
            "[nav] intent: at editor edge, parent present -> call escapeToParent(hostKey=",
            config.parent.hostKey,
            ", dir=",
            dir,
            ")",
          );
          _event?.preventDefault();
          escapeToParent(config.parent.editor, config.parent.hostKey, dir);
          return true;
        }
        console.log(
          "[nav] intent: mid-text -> normal caret movement, not trapping",
        );
        return false; // mid-text: normal caret movement
      };

    const cleanups: Array<() => void> = [
      editor.registerCommand(
        KEY_ARROW_RIGHT_COMMAND,
        onArrow(Direction.forward),
        COMMAND_PRIORITY_LOW,
      ),
      editor.registerCommand(
        KEY_ARROW_LEFT_COMMAND,
        onArrow(Direction.backward),
        COMMAND_PRIORITY_LOW,
      ),
    ];

    // UPWARD registration: a nested editor exposes its own edge as a handle in
    // the PARENT's map, keyed by the host branch token's NodeKey. This is the
    // reason the parent never needs the child editor instance.
    if (config.parent) {
      const { editor: parentEditor, hostKey } = config.parent;
      const parentHandles = getFocusHandles(parentEditor);
      const handle: TokenFocusHandle = {
        key: hostKey,
        focusFirst: () => {
          console.log(`[nav] handle#${regId}.focusFirst() invoked`);
          focusEditorEdge(editor, Direction.forward);
        },
        focusLast: () => {
          console.log(`[nav] handle#${regId}.focusLast() invoked`);
          focusEditorEdge(editor, Direction.backward);
        },
      };
      // Publish this handle only while THIS editor's root is actually
      // attached, rather than unconditionally on register(). StrictMode
      // double-invokes register() per nested editor, producing two distinct
      // LexicalEditor objects; only one of them ever gets a real DOM root,
      // and registration order doesn't predict which — a plain
      // `parentHandles.set()` here let the orphaned duplicate's dead handle
      // overwrite the live one whenever it happened to register second,
      // which is why focusFirst/focusLast silently no-op'd (their
      // `editor.getRootElement()` was permanently null). registerRootListener
      // fires with the live element on attach and `null` on detach, so the
      // orphan (root always null) never publishes over the live one, and the
      // live one withdraws cleanly if it's ever torn down.
      const offRootListener = editor.registerRootListener((rootElement) => {
        console.log(
          `[nav] UPWARD rootListener: nested#${regId} root=`,
          $describeEl(rootElement),
        );
        if (rootElement) {
          parentHandles.set(hostKey, handle);
        } else if (parentHandles.get(hostKey) === handle) {
          parentHandles.delete(hostKey);
        }
      });
      cleanups.push(offRootListener, () => {
        if (parentHandles.get(hostKey) === handle)
          parentHandles.delete(hostKey);
      });
    }

    return mergeRegister(...cleanups);
  },
});

export function selectAdjacentToToken(
  editor: LexicalEditor,
  tokenKey: NodeKey,
  dir: Direction,
): void {
  editor.update(
    () => {
      const node = $getNodeByKey(tokenKey);
      if (!node) return;
      if (dir === Direction.forward) node.selectNext(0, 0);
      else node.selectPrevious();
    },
    { onUpdate: () => editor.getRootElement()?.focus() },
  );
}

export function useRegisterTokenFocusHandle(
  makeHandle: () => TokenFocusHandle,
): void {
  const [editor] = useLexicalComposerContext();
  useEffect(() => {
    const handles = getFocusHandles(editor);
    const handle = makeHandle();
    handles.set(handle.key, handle);
    return () => {
      if (handles.get(handle.key) === handle) handles.delete(handle.key);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [editor]);
}
