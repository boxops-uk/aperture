// Single shared kit so App AND the Lexical decorator's React view resolve the
// same store context. Bound once at module scope (registry is a build-time const).
import { createSearchKit } from "./react";
import { registry } from "./demoRegistry";

export const kit = createSearchKit(registry);
export const {
  SearchRoot,
  useStore,
  useNode,
  useChildIds,
  useRootId,
  useDispatch,
  useQueryFold,
  useNodeFold,
} = kit;
