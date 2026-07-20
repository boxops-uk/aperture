import { createState } from "lexical";
import type { Qid } from "./query";

// Shared flat NodeState for the qid, used by BOTH token classes. The qid is the
// only datum either node carries; everything else lives in the store keyed by it.
export const qidState = createState("qid", {
  parse: (v): Qid => (typeof v === "string" ? v : ""),
});
