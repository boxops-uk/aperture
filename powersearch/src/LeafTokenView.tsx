import type { FC } from "react";
import type { Qid } from "./query";
import type { LeafDef } from "./operators";
import { useNode, useDispatch, useRegistry } from "./SearchContext";

type AnyControl = FC<{ value: unknown; onChange: (v: unknown) => void }>;

// Registry-agnostic: reads the leaf + its operator's Control from whatever
// runtime the surrounding <SearchRoot> provides. No specific kit or registry is
// imported, so the same decorator serves any search bar.
export function LeafTokenView({ qid }: { qid: Qid }) {
  const node = useNode(qid);
  const dispatch = useDispatch();
  const registry = useRegistry();
  if (!node || node.kind !== "leaf") return null;

  const def = registry[node.op] as LeafDef<unknown, string>;
  const Control = def.Control as unknown as AnyControl;
  const partial = !def.schema(node.value);

  return (
    <span
      className={`lt-token${partial ? " lt-token--partial" : ""}`}
      data-qid={qid}
    >
      <span className="lt-op">{node.op}</span>
      <select
        value={node.predicate}
        onChange={(e) =>
          dispatch({ type: "setPredicate", qid, predicate: e.target.value })
        }
      >
        {def.predicates.map((p) => (
          <option key={p} value={p}>
            {p}
          </option>
        ))}
      </select>
      <Control
        value={node.value}
        onChange={(v) => dispatch({ type: "setValue", qid, value: v })}
      />
      <button
        className="lt-x"
        title="remove"
        onClick={() => dispatch({ type: "removeSubtree", qid })}
      >
        ×
      </button>
    </span>
  );
}
