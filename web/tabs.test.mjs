import assert from "node:assert/strict";
import test from "node:test";

import { HOLD_MS, TABS, makeList, tabBar } from "./tabs.mjs";

test("the tab bar marks the chosen tab and counts each one", () => {
  const markup = tabBar("closed", { active: 2, closed: 5, all: 7 });

  assert.match(markup, /data-tab="closed" class="on">Closed <span class="count">5</),
  assert.match(markup, /data-tab="active">Active <span class="count">2</),
  assert.equal((markup.match(/class="on"/g) ?? []).length, 1);
});

test("the tabs come in the order the list shows them", () => {
  assert.deepEqual(
    TABS.map(([key, label]) => `${key}:${label}`),
    ["active:Active", "closed:Closed", "all:All"],
  );
});

test("the same attribute name can be swapped for another list's", () => {
  const markup = tabBar("all", { active: 0, closed: 0, all: 0 }, "data-btab");

  assert.match(markup, /data-btab="all"/);
  assert.doesNotMatch(markup, /data-tab/);
});

test("a swap that was already over when the list arrived is not held", () => {
  const list = makeList(() => true);

  assert.equal(list.gone("a"), true);
  assert.deepEqual(list.counts(["a"]), { active: 0, closed: 1, all: 1 });
});

test("a swap the list watched finish is held, then let go", () => {
  let over = false;
  const list = makeList(() => over);

  assert.equal(list.gone("a", 0), false); // seen while it was still going

  over = true;
  assert.equal(list.gone("a", 1), false); // first seen over, so held from here
  assert.equal(list.gone("a", HOLD_MS), false); // 1ms short of the hold
  assert.equal(list.gone("a", 1 + HOLD_MS), true);
});

test("a held swap is counted as active, so the count matches the cards", () => {
  let over = false;
  const list = makeList(() => over, 1000);

  list.gone("a", 0);
  over = true;

  assert.deepEqual(list.counts(["a"], 500), { active: 1, closed: 0, all: 1 });
  assert.deepEqual(list.pick(["a"], 500), ["a"]);
  assert.deepEqual(list.counts(["a"], 5000), { active: 0, closed: 1, all: 1 });
  assert.deepEqual(list.pick(["a"], 5000), []);
});

test("a swap that stops being over is held again from the next sighting", () => {
  let over = false;
  const list = makeList(() => over, 1000);

  list.gone("a", 0);
  over = true;
  assert.equal(list.gone("a", 100), false); // being held

  over = false; // it came back, as a re-read of the maker's record can do
  assert.equal(list.gone("a", 200), false);

  over = true;
  assert.equal(list.gone("a", 300), false); // held afresh, from this sighting
  assert.equal(list.gone("a", 1299), false);
  assert.equal(list.gone("a", 1300), true);
});

test("each tab picks its own swaps", () => {
  const over = new Set(["b"]);
  const list = makeList((id) => over.has(id), 0);
  const ids = ["a", "b"];

  assert.deepEqual(list.pick(ids, 0), ["a"]);

  list.choose("closed");
  assert.deepEqual(list.pick(ids, 0), ["b"]);

  list.choose("all");
  assert.deepEqual(list.pick(ids, 0), ["a", "b"]);
});
