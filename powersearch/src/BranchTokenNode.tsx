import { DecoratorNode, $getState, $setState, type LexicalNode } from "lexical";
import type { JSX } from "react";
import type { Qid } from "./query";
import { qidState } from "./nodeState";
import { BranchTokenView } from "./BranchTokenView";

export class BranchTokenNode extends DecoratorNode<JSX.Element> {
  $config() {
    return this.config("search-branch-token", {
      stateConfigs: [{ stateConfig: qidState, flat: true }],
    });
  }
  getQid(): Qid {
    return $getState(this, qidState);
  }
  createDOM(): HTMLElement {
    const span = document.createElement("span");
    span.className = "bt-host";
    return span;
  }
  updateDOM(): false {
    return false;
  }
  isInline(): true {
    return true;
  }
  decorate(): JSX.Element {
    return <BranchTokenView qid={this.getQid()} hostKey={this.getKey()} />;
  }
}

export function $createBranchTokenNode(qid: Qid): BranchTokenNode {
  return $setState(new BranchTokenNode(), qidState, qid);
}
export function $isBranchTokenNode(
  node: LexicalNode | null | undefined,
): node is BranchTokenNode {
  return node instanceof BranchTokenNode;
}
