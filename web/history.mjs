// The unified history's decisions, kept away from the DOM so they can be
// tested. The rows come from the coordinator's `/history`, which is the order
// book joined with the live state of each swap's record.
//
// Two rules matter here. A direction is named for what the taker does, in the
// terms the rest of the code uses. And a state the page does not recognise is
// shown as still running rather than hidden, because the one failure this list
// must not have is a live swap that quietly disappears from it.

/** How each mechanism reads to a person, named for what the taker does. */
const DIRECTIONS = {
  btc2ln: { label: "BTC → LN", note: "pay on chain, receive over Lightning" },
  ln2btc: { label: "LN → BTC", note: "pay over Lightning, receive on chain" },
};

/** States that mean the swap is over, whichever mechanism carried it. */
const DONE = new Set(["settled", "refunded", "claimed"]);

export const TABS = [
  ["open", "Active"],
  ["closed", "Closed"],
  ["all", "All"],
];

/** What to call a mechanism, and what it does. */
export function direction(kind) {
  return DIRECTIONS[kind] ?? { label: kind, note: "" };
}

/** Whether a swap is still doing something. */
export function isOpen(row) {
  return !DONE.has(String(row.state));
}

/** The rows a tab and a direction filter leave. */
export function select(history, { kind = "", tab = "open" } = {}) {
  const rows = history ?? [];
  return rows.filter((row) => {
    if (kind && row.kind !== kind) return false;
    if (tab === "open") return isOpen(row);
    if (tab === "closed") return !isOpen(row);
    return true;
  });
}

/** How many rows each tab would show, for the tab labels. */
export function counts(history, kind = "") {
  const rows = (history ?? []).filter((row) => !kind || row.kind === kind);
  return {
    open: rows.filter(isOpen).length,
    closed: rows.filter((row) => !isOpen(row)).length,
    all: rows.length,
  };
}

/** One row, as a table row. Styling waits for the pass that can see the sheet. */
export function rowHtml(row) {
  const { label, note } = direction(row.kind);
  const cost = row.cost_sat === null || row.cost_sat === undefined ? "—" : `${row.cost_sat}`;
  const at = Number(row.at) ? new Date(row.at * 1000).toISOString().slice(0, 16).replace("T", " ") : "—";

  return (
    `<tr class="${isOpen(row) ? "open" : "closed"}">` +
    `<td>${at}</td>` +
    `<td title="${note}">${label}</td>` +
    `<td>${row.from}</td>` +
    `<td>${row.to}</td>` +
    `<td>${cost}</td>` +
    `<td>${row.state}</td>` +
    `<td>${row.locktime ?? "—"}</td>` +
    `</tr>`
  );
}

/** The whole table, from rows already chosen and paged. */
export function tableHtml(rows) {
  if (!rows.length) {
    return "<p>No swaps here yet.</p>";
  }
  const head =
    "<tr><th>when</th><th>direction</th><th>from</th><th>to</th><th>cost</th>" +
    "<th>state</th><th>refund at</th></tr>";
  return `<table>${head}${rows.map(rowHtml).join("")}</table>`;
}
