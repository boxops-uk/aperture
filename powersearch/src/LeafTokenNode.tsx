import { DecoratorNode, $getState, $setState, type LexicalNode } from "lexical";
import type { JSX } from "react";
import type { Qid } from "./query";
import { LeafTokenView } from "./LeafTokenView";
import { qidState } from "./nodeState";

export class LeafTokenNode extends DecoratorNode<JSX.Element> {
  $config() {
    return this.config("search-leaf-token", {
      stateConfigs: [{ stateConfig: qidState, flat: true }],
    });
  }

  getQid(): Qid {
    return $getState(this, qidState);
  }

  createDOM(): HTMLElement {
    const span = document.createElement("span");
    span.className = "lt-host";
    return span;
  }
  updateDOM(): false {
    return false;
  }
  isInline(): true {
    return true;
  }

  decorate(): JSX.Element {
    return <LeafTokenView qid={this.getQid()} />;
  }
}

export function $createLeafTokenNode(qid: Qid): LeafTokenNode {
  return $setState(new LeafTokenNode(), qidState, qid);
}
export function $isLeafTokenNode(
  node: LexicalNode | null | undefined,
): node is LeafTokenNode {
  return node instanceof LeafTokenNode;
}
