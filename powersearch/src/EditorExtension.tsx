import {
  defineExtension,
  configExtension,
  KEY_ENTER_COMMAND,
  COMMAND_PRIORITY_LOW,
} from "lexical";
import { AutoFocusExtension, NestedEditorExtension } from "@lexical/extension";
import { PlainTextExtension } from "@lexical/plain-text";
import { ReactExtension } from "@lexical/react/ReactExtension";
import { ContentEditable } from "@lexical/react/LexicalContentEditable";
import type { Registry, LeafKeys, BranchKeys } from "./operators";
import type { SearchStore } from "./store";
import type { Qid } from "./query";
import { LeafTokenNode } from "./LeafTokenNode";
import { BranchTokenNode } from "./BranchTokenNode";
import { createStoreBridgeExtension } from "./storeBridge";
import { makeTypeaheadDecorator } from "./typeahead";
import { FocusNavExtension, type ParentLink } from "./nav";

const disableMultilineExtension = defineExtension({
  name: "disable-multiline",
  namespace: "tokenized-search",
  register(editor) {
    return editor.registerCommand(
      KEY_ENTER_COMMAND,
      (e) => {
        e?.preventDefault();
        return true;
      },
      COMMAND_PRIORITY_LOW,
    );
  },
});

/**
 * ONE editor recipe for every branch level. The root editor calls it with no
 * parent; a nested branch editor passes { $getParentEditor } so NestedEditor
 * Extension links it to its parent (command/selection propagation, editable
 * inheritance). The bridge is scoped to `branchQid`, so each editor projects
 * exactly its own children; a branch child becomes a BranchTokenNode whose
 * decorator mounts another one of these — recursion.
 */
export function buildBranchExtension<R extends Registry>(
  store: SearchStore<R>,
  registry: R,
  branchQid: Qid,
  leafKeywords: readonly LeafKeys<R>[],
  branchKeywords: readonly BranchKeys<R>[],
  parent?: ParentLink,
) {
  const TypeaheadMenu = makeTypeaheadDecorator(
    store,
    registry,
    branchQid,
    leafKeywords,
    branchKeywords,
  );
  return defineExtension({
    name: parent ? "search/branch" : "search/root",
    namespace: "tokenized-search",
    nodes: [LeafTokenNode, BranchTokenNode],
    dependencies: [
      PlainTextExtension,
      createStoreBridgeExtension(store, branchQid),
      configExtension(ReactExtension, {
        contentEditable: (
          <ContentEditable className="lt-input" ariaLabel="search query" />
        ),
        decorators: [<TypeaheadMenu key="typeahead" />],
      }),
      disableMultilineExtension,
      ...(parent
        ? [
            configExtension(NestedEditorExtension, {
              $getParentEditor: () => parent.editor,
              inheritEditableFromParent: true,
            }),
            configExtension(FocusNavExtension, {
              parent,
            }),
          ]
        : [AutoFocusExtension, FocusNavExtension]), // autofocus the root only (avoid nested focus fights)
    ],
  });
}
