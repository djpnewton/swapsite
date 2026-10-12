// Tests for the shared pager, run by `node --test`.
//
// The list arithmetic is the part of the pages that can be wrong without
// looking wrong, and it is the part that has no browser in it.

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  DEFAULT_PAGE_SIZE,
  PAGE_SIZES,
  newPager,
  paginate,
  pagerCommand,
  pagerHtml,
} from "./pager.mjs";

const rows = (n) => Array.from({ length: n }, (_, i) => i + 1);

test("an empty list is one empty page", () => {
  const view = paginate([], newPager());
  assert.equal(view.page, 1);
  assert.equal(view.pages, 1);
  assert.equal(view.total, 0);
  assert.equal(view.from, 0);
  assert.equal(view.to, 0);
  assert.deepEqual(view.rows, []);
});

test("a short list is one page", () => {
  const view = paginate(rows(3), newPager());
  assert.equal(view.pages, 1);
  assert.equal(view.from, 1);
  assert.equal(view.to, 3);
  assert.deepEqual(view.rows, [1, 2, 3]);
});

test("the last page is partial and stops at the end", () => {
  const view = paginate(rows(43), { page: 5, size: 10 });
  assert.equal(view.pages, 5);
  assert.equal(view.from, 41);
  assert.equal(view.to, 43);
  assert.deepEqual(view.rows, [41, 42, 43]);
});

test("a page past the end is clamped to the last one", () => {
  // What happens when a filter or a tab removes rows while page 4 is showing.
  const view = paginate(rows(12), { page: 9, size: 10 });
  assert.equal(view.page, 2);
  assert.equal(view.from, 11);
  assert.equal(view.to, 12);
  assert.deepEqual(view.rows, [11, 12]);
});

test("only listed page sizes are honoured", () => {
  assert.equal(paginate(rows(5), { page: 1, size: 7 }).size, DEFAULT_PAGE_SIZE);
  for (const size of PAGE_SIZES) {
    assert.equal(paginate(rows(5), { page: 1, size }).size, size);
  }
});

test("a click says which way to move", () => {
  assert.deepEqual(pagerCommand({ dataset: { pager: "prev" } }), { move: -1 });
  assert.deepEqual(pagerCommand({ dataset: { pager: "next" } }), { move: 1 });
  assert.equal(pagerCommand({ dataset: {} }), null);
  assert.equal(pagerCommand(null), null);
  assert.equal(pagerCommand(undefined), null);
});

test("the size control reads its own value", () => {
  // `data-size` carries no value of its own: the chosen size is the select's.
  assert.deepEqual(
    pagerCommand({ dataset: { size: "" }, value: "25" }),
    { size: 25 },
  );
  assert.equal(pagerCommand({ dataset: { size: "" }, value: "0" }), null);
  assert.equal(pagerCommand({ dataset: { size: "" }, value: "lots" }), null);
});

test("the markup disables what cannot be done and marks the size", () => {
  const first = pagerHtml(paginate(rows(43), newPager()));
  assert.match(first, /1\u201310 of 43/);
  assert.match(first, /data-pager="prev" disabled/);
  assert.doesNotMatch(first, /data-pager="next" disabled/);
  assert.match(first, /value="10" selected/);

  const last = pagerHtml(paginate(rows(43), { page: 5, size: 10 }));
  assert.match(last, /data-pager="next" disabled/);
  assert.doesNotMatch(last, /data-pager="prev" disabled/);
});
