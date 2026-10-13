import assert from "node:assert/strict";
import test from "node:test";

import { counts, direction, isOpen, rowHtml, select, tableHtml } from "./history.mjs";
import { paginate } from "./pager.mjs";

/// Two rows as the coordinator's `/history` returns them, one settled and one
/// still running, plus one whose state this page has never heard of.
const HISTORY = [
  { id: "a", kind: "ln2btc", at: 1_791_842_086, from: 30_030, to: 30_000, cost_sat: 30, state: "settled", locktime: 4_100 },
  { id: "b", kind: "btc2ln", at: 1_791_842_082, from: 22_100, to: 20_000, cost_sat: 2_100, state: "funded", locktime: 5_701 },
  { id: "c", kind: "btc2ln", at: 1_791_842_080, from: 22_100, to: 20_000, cost_sat: 2_100, state: "claimed", locktime: 5_768 },
  { id: "d", kind: "btc2ln", at: 1_791_842_079, from: 22_100, to: 20_000, cost_sat: 2_100, state: "something_new", locktime: 5_770 },
];

test("a direction is named for what the taker does", () => {
  assert.equal(direction("btc2ln").label, "BTC → LN");
  assert.equal(direction("ln2btc").label, "LN → BTC");
  // Nothing known, so the kind stands in rather than the row being dropped.
  assert.equal(direction("lbtc").label, "lbtc");
});

test("a swap is finished only when it says so", () => {
  assert.equal(isOpen({ state: "funded" }), true);
  assert.equal(isOpen({ state: "settled" }), false);
  assert.equal(isOpen({ state: "refunded" }), false);
  assert.equal(isOpen({ state: "claimed" }), false);
  // A state this page has never seen could be anything, so it is not hidden.
  assert.equal(isOpen({ state: "something_new" }), true);
});

test("the tabs split running from finished", () => {
  assert.deepEqual(
    select(HISTORY, { tab: "open" }).map((row) => row.id),
    ["b", "d"],
  );
  assert.deepEqual(
    select(HISTORY, { tab: "closed" }).map((row) => row.id),
    ["a", "c"],
  );
  assert.equal(select(HISTORY, { tab: "all" }).length, 4);
});

test("a direction filter and a tab compose", () => {
  assert.deepEqual(
    select(HISTORY, { kind: "btc2ln", tab: "closed" }).map((row) => row.id),
    ["c"],
  );
  assert.deepEqual(select(HISTORY, { kind: "ln2btc", tab: "open" }), []);
  assert.equal(select(HISTORY, { kind: "lbtc", tab: "all" }).length, 0);
});

test("the counts say what each tab would show, for the chosen direction", () => {
  assert.deepEqual(counts(HISTORY), { open: 2, closed: 2, all: 4 });
  assert.deepEqual(counts(HISTORY, "btc2ln"), { open: 2, closed: 1, all: 3 });
});

test("a row shows both sides, the cost and where the refund opens", () => {
  const html = rowHtml(HISTORY[1]);
  assert.match(html, /BTC → LN/);
  assert.match(html, /22100/);
  assert.match(html, /20000/);
  assert.match(html, /2100/);
  assert.match(html, /funded/);
  assert.match(html, /5701/);
  assert.match(html, /class="open"/);
  assert.match(rowHtml(HISTORY[0]), /class="closed"/);
});

test("a cost that is not comparable is not printed as a number", () => {
  // `cost_sat` is null when a mechanism's sides are not both satoshi.
  assert.match(rowHtml({ ...HISTORY[1], cost_sat: null }), /<td>—<\/td>/);
});

test("an empty history says so rather than rendering an empty table", () => {
  assert.match(tableHtml([]), /No swaps here yet/);
  assert.equal(tableHtml([]).includes("<table>"), false);
});

test("the history pages with the same pager as everything else", () => {
  const rows = select(HISTORY, { tab: "all" });
  const first = paginate(rows, { page: 1, size: 10 });
  assert.equal(first.total, 4);
  assert.equal(first.from, 1);
  assert.equal(first.to, 4);
  assert.equal(first.rows.length, 4);
  assert.match(tableHtml(first.rows), /<tr><th>when<\/th>/);
});
