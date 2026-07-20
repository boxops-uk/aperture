import { useCallback, useMemo, useState, type JSX } from "react";
import { createPortal } from "react-dom";
import { useLexicalComposerContext } from "@lexical/react/LexicalComposerContext";
import {
  LexicalTypeaheadMenuPlugin,
  MenuOption,
} from "@lexical/react/LexicalTypeaheadMenuPlugin";
import type {
  TriggerFn,
  MenuTextMatch,
} from "@lexical/react/LexicalTypeaheadMenuPlugin";
import type { TextNode } from "lexical";
import type { Registry, LeafKeys, BranchKeys } from "./operators";
import type { SearchStore } from "./store";
import { createLeaf, createBranch } from "./store";
import type { Qid } from "./query";

class OpOption extends MenuOption {
  constructor(
    public readonly keyword: string,
    public readonly kind: "leaf" | "branch",
  ) {
    super(keyword);
  }
}

// Trigger on a trailing run of letters — typing "cont"/"any" suggests operators.
const wordTrigger: TriggerFn = (text): MenuTextMatch | null => {
  const m = /(^|\s)([a-zA-Z][a-zA-Z0-9]*)$/.exec(text);
  if (!m) return null;
  const matchingString = m[2];
  return {
    leadOffset: text.length - matchingString.length,
    matchingString,
    replaceableString: matchingString,
  };
};

/**
 * Typeahead, contributed as a ReactExtension DECORATOR. Offers BOTH leaf and
 * branch operators; on select it dispatches createLeaf or createBranch by kind
 * (store-first), and the bridge projects the token. Registry-agnostic — the
 * keyword lists and registry are passed in from buildBranchExtension.
 */
export function makeTypeaheadDecorator<R extends Registry>(
  store: SearchStore<R>,
  registry: R,
  branchQid: Qid,
  leafKeywords: readonly LeafKeys<R>[],
  branchKeywords: readonly BranchKeys<R>[],
) {
  return function TypeaheadMenu(): JSX.Element {
    const [editor] = useLexicalComposerContext();
    const [query, setQuery] = useState("");

    const all = useMemo(
      () => [
        ...leafKeywords.map((k) => new OpOption(k as string, "leaf")),
        ...branchKeywords.map((k) => new OpOption(k as string, "branch")),
      ],
      [],
    );
    const options = useMemo(() => {
      const q = query.toLowerCase();
      return q ? all.filter((o) => o.keyword.toLowerCase().includes(q)) : all;
    }, [query, all]);

    const onSelectOption = useCallback(
      (
        option: OpOption,
        nodeToReplace: TextNode | null,
        closeMenu: () => void,
      ) => {
        editor.update(() => {
          nodeToReplace?.remove();
        });
        const node =
          option.kind === "branch"
            ? createBranch(registry, option.keyword as BranchKeys<R>)
            : createLeaf(registry, option.keyword as LeafKeys<R>);
        store.dispatch({
          type: "insertChild",
          parentQid: branchQid,
          node: node as never,
        });
        closeMenu();
      },
      [editor],
    );

    return (
      <LexicalTypeaheadMenuPlugin<OpOption>
        options={options}
        triggerFn={wordTrigger}
        onQueryChange={(s) => setQuery(s ?? "")}
        onSelectOption={onSelectOption}
        menuRenderFn={(
          anchorRef,
          {
            options: opts,
            selectedIndex,
            selectOptionAndCleanUp,
            setHighlightedIndex,
          },
        ) =>
          anchorRef.current && opts.length
            ? createPortal(
                <ul className="lt-typeahead">
                  {opts.map((o, i) => (
                    <li
                      key={o.key}
                      className={`${i === selectedIndex ? "sel" : ""} lt-opt--${(o as OpOption).kind}`}
                      onMouseEnter={() => setHighlightedIndex(i)}
                      onMouseDown={(e) => {
                        e.preventDefault();
                        selectOptionAndCleanUp(o);
                      }}
                    >
                      {(o as OpOption).keyword}
                    </li>
                  ))}
                </ul>,
                anchorRef.current,
              )
            : null
        }
      />
    );
  };
}
