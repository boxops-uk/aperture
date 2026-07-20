import { useEffect, useRef, useState } from "react";

// A native <select>'s popup can't be styled consistently across browsers
// (Chrome mostly ignores CSS on it) — so predicate picking is a small
// listbox instead, built from the exact same .lt-typeahead markup/classes
// as the operator typeahead, for real visual parity rather than an
// approximation.
export function PredicateMenu<P extends string>({
  value,
  predicates,
  onChange,
}: {
  value: P;
  predicates: readonly P[];
  onChange: (predicate: P) => void;
}) {
  const [open, setOpen] = useState(false);
  const [highlighted, setHighlighted] = useState(0);
  const rootRef = useRef<HTMLSpanElement>(null);

  useEffect(() => {
    if (!open) return;
    setHighlighted(Math.max(0, predicates.indexOf(value)));
    const onDocMouseDown = (e: MouseEvent) => {
      if (!rootRef.current?.contains(e.target as Node)) setOpen(false);
    };
    document.addEventListener("mousedown", onDocMouseDown);
    return () => document.removeEventListener("mousedown", onDocMouseDown);
  }, [open, predicates, value]);

  const commit = (p: P) => {
    onChange(p);
    setOpen(false);
  };

  return (
    <span
      className="lt-select"
      ref={rootRef}
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          setOpen(false);
        } else if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          if (open) commit(predicates[highlighted]);
          else setOpen(true);
        } else if (e.key === "ArrowDown") {
          e.preventDefault();
          if (!open) setOpen(true);
          else setHighlighted((i) => Math.min(i + 1, predicates.length - 1));
        } else if (e.key === "ArrowUp") {
          e.preventDefault();
          if (!open) setOpen(true);
          else setHighlighted((i) => Math.max(i - 1, 0));
        }
      }}
    >
      <button
        type="button"
        className="lt-select-trigger"
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => setOpen((o) => !o)}
      >
        {value}
      </button>
      {open && (
        <ul className="lt-typeahead lt-select-menu" role="listbox">
          {predicates.map((p, i) => (
            <li
              key={p}
              role="option"
              aria-selected={p === value}
              className={i === highlighted ? "sel" : ""}
              onMouseEnter={() => setHighlighted(i)}
              onMouseDown={(e) => {
                e.preventDefault();
                commit(p);
              }}
            >
              {p}
            </li>
          ))}
        </ul>
      )}
    </span>
  );
}
