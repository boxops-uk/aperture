import { t } from "@traversable/schema";
import { defineOperators, leaf, branch, type LeafControl } from "./operators";

// The schema IS the validity test. This one accepts any string, empty included.
// The paired defaultValue ('' on the operator below) must satisfy it — that is
// what makes "just added" and "typed then cleared" the same state, instead of
// undefined-on-create failing a guard that '' passes.
const stringValue = (u: unknown): u is string => typeof u === "string";
const dateValue = t.object({
  date: (u: unknown): u is string => typeof u === "string" && u.length > 0,
});

// Operator-provided value editors (the whole point of Control living on the op).
// Typed against the operator's T — schema <-> control cannot drift.
const StringControl: LeafControl<string> = ({ value, onChange }) => (
  <input
    value={value ?? ""}
    placeholder="text…"
    onChange={(e) => onChange(e.target.value)}
  />
);

const DateControl: LeafControl<{ date: string }> = ({ value, onChange }) => (
  <input
    type="date"
    value={value?.date ?? ""}
    onChange={(e) => onChange({ date: e.target.value })}
  />
);

export const registry = defineOperators({
  Title: leaf({
    schema: stringValue,
    predicates: ["is", "contains"],
    defaultPredicate: "contains",
    defaultValue: "",
    Control: StringControl,
  }),
  "Last modified": leaf({
    schema: dateValue,
    predicates: ["after", "before", "on"],
    defaultPredicate: "after",
    Control: DateControl,
  }),
  anyOf: branch(),
  allOf: branch(),
});

export type Reg = typeof registry;
