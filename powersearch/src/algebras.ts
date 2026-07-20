import type { Algebra } from "./operators";
import type { Reg } from "./demoRegistry";

// Search IR — the designated search interpretation (carrier of onDebouncedChange).
export type IR =
  | { field: "title"; op: string; value: string }
  | { field: "last-modified"; op: string; value: string }
  | { bool: "any" | "all"; clauses: IR[] };

export const MATCH_NONE: IR = { bool: "all", clauses: [] }; // the ONE zero, in R

// Module constants -> stable identities -> safe in hook deps / useMemo.
export const toIR: Algebra<Reg, IR> = {
  Title: (op, v) => ({ field: "title", op, value: v }),
  "Last modified": (op, v) => ({ field: "last-modified", op, value: v.date }),
  anyOf: (kids) => ({ bool: "any", clauses: kids }),
  allOf: (kids) => ({ bool: "all", clauses: kids }),
};

// A SECOND algebra over the same functor, rendered live as chip/preview labels.
export const toLabel: Algebra<Reg, string> = {
  Title: (op, v) => `title ${op} "${v}"`,
  "Last modified": (op, v) => `last modified ${op} ${v.date}`,
  anyOf: (kids) => `(${kids.join(" OR ")})`,
  allOf: (kids) => `(${kids.join(" AND ")})`,
};
