// The page shell: which direction you are starting, and the history of every
// swap either direction has done.
//
// The two flows look after themselves — ln2btc.js owns the Lightning-to-on-chain
// side, btc2ln.js the on-chain-to-Lightning side — so this file owns the
// chooser and the history and nothing else. That way a bug in one direction's
// flow cannot take the other down with it.
//
// The history uses its own attribute names for its tabs and pager rather than
// the ones the two flows listen for on `document`. Sharing them would mean
// clicking the history's pager also paged the swaps list underneath.

import { counts, select, tableHtml, TABS } from "./history.mjs";
import { installCopy } from "./pay.mjs";
import { newPager, paginate } from "./pager.mjs";

const $ = (id) => document.getElementById(id);
const DIRECTIONS = ["ln2btc", "btc2ln"];

let rows = [];
let tab = "open";
const pager = newPager();

/** Show one direction's flow, and remember which. */
function choose(kind) {
  for (const direction of DIRECTIONS) {
    for (const panel of document.querySelectorAll(`.${direction}-panel`)) {
      panel.hidden = direction !== kind;
    }
  }
  for (const button of document.querySelectorAll("[data-direction]")) {
    button.classList.toggle("on", button.dataset.direction === kind);
  }
  try {
    localStorage.setItem("swapsite.direction", kind);
  } catch {
    // A page that cannot remember is still a page that works.
  }
}

function tabBar() {
  const n = counts(rows);
  return TABS.map(
    ([id, label]) =>
      `<button type="button" data-htab="${id}" class="${tab === id ? "on" : ""}">` +
      `${label} <span class="count">${n[id]}</span></button>`,
  ).join("");
}

/** The pager, spelled the way this page's own attributes are. */
function pagerBar(view) {
  if (view.pages <= 1) return "";
  const sizes = [10, 25, 50]
    .map((size) => `<option value="${size}"${size === view.size ? " selected" : ""}>${size}</option>`)
    .join("");
  return (
    `<button type="button" data-hpage="-1"${view.page <= 1 ? " disabled" : ""}>prev</button>` +
    `<span>${view.from}–${view.to} of ${view.total}</span>` +
    `<button type="button" data-hpage="1"${view.page >= view.pages ? " disabled" : ""}>next</button>` +
    `<select data-hsize>${sizes}</select>`
  );
}

function paint() {
  const view = paginate(select(rows, { tab }), pager);
  $("history-tabs").innerHTML = tabBar();
  $("history-list").innerHTML = tableHtml(view.rows);
  $("history-pager").innerHTML = pagerBar(view);
}

async function refresh() {
  try {
    const response = await fetch("/history");
    const answer = await response.json();
    // An error comes back as JSON as well, and an unreadable history would
    // otherwise paint as an empty one: "no swaps" when the truth is "no
    // answer". A history is a list, so anything else is a failure.
    if (!response.ok || !Array.isArray(answer.swaps)) {
      throw new Error(answer.error ?? `HTTP ${response.status}`);
    }
    rows = answer.swaps;
    paint();
  } catch (e) {
    $("history-list").innerHTML = `<p class="bad">the history is not answering: ${
      e.message ?? e
    }</p>`;
  }
}

document.addEventListener("click", (event) => {
  const direction = event.target.closest("[data-direction]");
  if (direction) {
    choose(direction.dataset.direction);
    return;
  }

  const chosen = event.target.closest("[data-htab]");
  if (chosen) {
    tab = chosen.dataset.htab;
    pager.page = 1;
    paint();
    return;
  }

  const move = event.target.closest("[data-hpage]");
  if (move) {
    pager.page += Number(move.dataset.hpage);
    paint();
  }
});

document.addEventListener("change", (event) => {
  const size = event.target.closest("[data-hsize]");
  if (!size) return;
  pager.size = Number(size.value);
  pager.page = 1;
  paint();
});

// The copy controls belong to no single flow, and their cards are rebuilt on
// every poll, so one listener here serves all of them.
installCopy();

let remembered = "ln2btc";
try {
  remembered = localStorage.getItem("swapsite.direction") ?? remembered;
} catch {
  // Same as above: the default is fine.
}
choose(DIRECTIONS.includes(remembered) ? remembered : "ln2btc");

refresh();
setInterval(refresh, 5000);
