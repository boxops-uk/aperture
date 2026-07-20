import { SearchRoot, useQueryFold } from "./searchKit";
import { SearchBar } from "./SearchBar";
import { toIR, toLabel, MATCH_NONE, type IR } from "./algebras";
import "./App.css";

// Live human-readable readout of the whole query (a second algebra, `toLabel`,
// folding the same tree the search algebra folds).
function Preview() {
  const label = useQueryFold(toLabel, "∅");
  return (
    <div className="lt-preview">
      <strong>query:</strong> {label.isEmpty ? "(nothing)" : label.result}
      {label.isPartial && (
        <span className="lt-partial"> · has incomplete conditions</span>
      )}
    </div>
  );
}

export default function App() {
  return (
    <SearchRoot
      searchAlgebra={toIR} // fold -> your backend IR
      rootEmpty={MATCH_NONE} // the ONE identity, applied once at the root
      rootOp="All of" // top-level connective
      debounceMs={1000}
      onDebouncedChange={(ir: IR, meta, _signal) => {
        if (meta.isPartial) {
          console.log("partial query, not sending to backend");
          return;
        }

        if (meta.isEmpty) {
          console.log("empty query, not sending to backend");
          return;
        }

        // this is where you hit your API; _signal aborts superseded runs
        // fetch('/api/search', { method: 'POST', body: JSON.stringify(ir), signal: _signal })
        console.log(JSON.stringify(ir));
      }}
    >
      <div className="lt-shell">
        <SearchBar />
      </div>
      <Preview />
    </SearchRoot>
  );
}
