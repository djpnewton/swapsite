// The regtest control panel.
//
// Dev only. Everything it shows comes from one `/status` call; the only things
// it does are mine blocks and drive the taker's node as a wallet, because a swap
// needs both and shelling into a container to do either is the alternative.

import {
  PAGE_SIZES,
  newPager,
  paginate,
  pagerCommand,
  pagerHtml,
} from "/pager.mjs";

const $ = (id) => document.getElementById(id);
const sats = (n) => `${Number(n).toLocaleString()} sat`;
const short = (text, head = 10, tail = 6) =>
  text.length <= head + tail ? text : `${text.slice(0, head)}…${text.slice(-tail)}`;

/** A deadline this close is worth showing in red. Matches URGENT_BLOCKS. */
const URGENT_BLOCKS = 12;

// One pager and one set of filters per list. Held here rather than read back off
// the controls, because the panel re-renders on a poll and a choice has to
// survive that.
const lists = {
  history: {
    pager: newPager(),
    filters: { kind: "", direction: "", state: "" },
  },
  swaps: { pager: newPager(), filters: { state: "" } },
};

const when = (seconds) =>
  seconds > 0 ? new Date(seconds * 1000).toLocaleTimeString() : "-";

async function api(path, body) {
  const response = await fetch(
    path,
    body === undefined
      ? {}
      : {
          method: "POST",
          headers: { "Content-Type": "text/plain" },
          body: typeof body === "string" ? body : JSON.stringify(body),
        },
  );

  const payload = await response.json();
  if (!response.ok) {
    throw new Error(payload.error ?? `HTTP ${response.status}`);
  }
  return payload;
}

// --- rendering -------------------------------------------------------------

function chainLine(status) {
  const height = status.height;
  if (height?.error) return `chain unreachable: ${height.error}`;

  const balance = status.bitcoin?.error
    ? `wallet unreachable: ${status.bitcoin.error}`
    : `wallet ${sats(status.bitcoin.sat)}`;

  return `height ${height} · ${balance}`;
}

function nodeCard(role, node) {
  if (node.error) return `<p class="error">${role}: ${node.error}</p>`;

  // The trap this exists to make obvious: a channel can be up and still unable
  // to pay, and the payment failure says nothing about why.
  const starved = node.outbound_sat === 0;

  return `
    <article class="swap">
      <div class="swap-head">
        <span class="swap-id">${node.alias} (${role})</span>
        <span class="badge ${starved ? "bad" : "good"}">${
          starved ? "cannot send" : "can send"
        }</span>
      </div>
      <dl>
        <dt>pubkey</dt><dd>${short(node.pubkey, 12, 6)}</dd>
        <dt>can send</dt><dd>${sats(node.outbound_sat)}</dd>
        <dt>can receive</dt><dd>${sats(node.inbound_sat)}</dd>
        <dt>channels</dt><dd>${node.channels}</dd>
        <dt>on chain</dt><dd>${sats(node.wallet_sat)}</dd>
      </dl>
      ${
        starved
          ? `<p class="error urgent">No spendable outbound, so a payment will fail. Reset with
             <code>docker compose down -v &amp;&amp; ./scripts/regtest-init.sh</code>.</p>`
          : ""
      }
    </article>`;
}

/** The values a filter can take, taken from the rows themselves. */
function values(rows, field) {
  return [...new Set(rows.map((row) => row[field]))].sort();
}

/** A set of selects whose options are what the data actually contains. */
function filtersMarkup(list, rows, fields) {
  return fields
    .map((field) => {
      const chosen = lists[list].filters[field];
      const options = ["", ...values(rows, field)]
        .map(
          (value) =>
            `<option value="${value}"${value === chosen ? " selected" : ""}>` +
            `${value === "" ? "any" : value.toLowerCase().replace(/_/g, " ")}</option>`,
        )
        .join("");

      return `<label>${field} <select data-list="${list}" data-field="${field}">${options}</select></label>`;
    })
    .join("");
}

/** Every set filter agrees with the row, so an empty filter means "any". */
function matches(row, filters) {
  return Object.entries(filters).every(
    ([field, wanted]) => wanted === "" || row[field] === wanted,
  );
}

function historyTable(view) {
  const rows = view.rows
    .map(
      (entry) => `<tr>
        <td>${entry.kind}</td>
        <td>${entry.direction}</td>
        <td>${sats(entry.sats)}</td>
        <td>${entry.state.toLowerCase()}</td>
        <td><code>${short(entry.hash, 8, 4)}</code></td>
        <td>${when(entry.at)}</td>
      </tr>`,
    )
    .join("");

  return `<table>
    <thead>
      <tr>
        <th>layer</th><th></th><th>amount</th><th>state</th><th>hash</th><th>at</th>
      </tr>
    </thead>
    <tbody>${rows}</tbody>
  </table>`;
}

function swapTable(view, height) {
  const rows = view.rows
    .map((swap) => {
      const left =
        swap.state === "refunded" || swap.state === "settled"
          ? null
          : swap.locktime - height;

      const cell =
        left === null
          ? `<td class="gone">-</td>`
          : `<td class="${left <= URGENT_BLOCKS ? "close" : ""}">${left}</td>`;

      return `<tr>
        <td><code>${short(swap.id, 8, 4)}</code></td>
        <td>${swap.state.replace(/_/g, " ")}</td>
        <td>${sats(swap.amount_sat)}</td>
        <td>${sats(swap.deposit_sat)}</td>
        <td>${swap.locktime}</td>
        ${cell}
      </tr>`;
    })
    .join("");

  return `<table>
    <thead>
      <tr>
        <th>swap</th><th>state</th><th>amount</th><th>deposit</th>
        <th>refund at</th><th>blocks left</th>
      </tr>
    </thead>
    <tbody>${rows}</tbody>
  </table>`;
}

// Only touch the DOM when something changed: this polls, and a repaint every
// couple of seconds is enough to take the cursor out of a field being typed in.
const painted = {};

/** The last status, so a filter or a page can re-render without a round trip. */
let last = null;

function paint(id, markup) {
  if (painted[id] === markup) return;
  painted[id] = markup;
  $(id).innerHTML = markup;
}

/** Which list a control belongs to. Both the filters and the pagers say. */
const listOf = (control) =>
  control?.closest?.("[data-list]")?.dataset.list ?? null;

/**
 * Paint one filtered, paged list.
 *
 * The filters, the rows and the pager are painted separately, so a poll that
 * changes nothing visible leaves every control exactly where it was.
 */
function list(id, section, fields, table) {
  if (section?.error) {
    paint(`${id}-filters`, "");
    paint(id, `<p class="error">${section.error}</p>`);
    paint(`${id}-pager`, "");
    return;
  }

  const rows = Array.isArray(section) ? section : [];
  const state = lists[id];

  paint(
    `${id}-filters`,
    `<div class="filters">${filtersMarkup(id, rows, fields)}</div>`,
  );

  const view = paginate(
    rows.filter((row) => matches(row, state.filters)),
    state.pager,
  );

  paint(
    id,
    view.total > 0
      ? table(view)
      : `<p class="quote">${
          rows.length === 0 ? "nothing yet" : "nothing matches the filter"
        }</p>`,
  );
  paint(
    `${id}-pager`,
    view.total > PAGE_SIZES[0]
      ? `<div data-list="${id}">${pagerHtml(view)}</div>`
      : "",
  );
}

function render(status) {
  last = status;
  const height = typeof status.height === "number" ? status.height : null;

  paint(
    "nodes",
    Object.entries(status.nodes)
      .map(([role, node]) => nodeCard(role, node))
      .join(""),
  );

  list("history", status.history, ["kind", "direction", "state"], historyTable);
  list("swaps", status.swaps, ["state"], (view) => swapTable(view, height));

  $("chain").textContent = chainLine(status);
  $("foot").textContent = status.taker_url
    ? `taker page: ${status.taker_url}`
    : "taker page: not configured";
}

// --- actions ---------------------------------------------------------------

async function refresh() {
  try {
    render(await api("/status"));
  } catch (error) {
    $("chain").textContent = `panel cannot reach its own service: ${error.message}`;
  }
}

async function mine(blocks) {
  $("mine").disabled = true;
  try {
    const { mined, height } = await api("/mine", { blocks });
    $("chain").textContent = `mined ${mined} blocks, now at height ${height}`;
  } catch (error) {
    $("chain").textContent = `mining failed: ${error.message}`;
  } finally {
    $("mine").disabled = false;
    await refresh();
  }
}

async function makeInvoice() {
  $("make-invoice").disabled = true;
  try {
    const { bolt11 } = await api("/taker/invoice", {
      sats: Number($("inv-amt").value),
      memo: $("inv-memo").value,
    });
    $("bolt11").textContent = bolt11;
    $("bolt11").hidden = false;
    $("node-note").textContent = "invoice ready: pay it with anything on this network";
  } catch (error) {
    $("node-note").textContent = `could not make an invoice: ${error.message}`;
  } finally {
    $("make-invoice").disabled = false;
    await refresh();
  }
}

async function payInvoice() {
  $("pay-invoice").disabled = true;
  $("node-note").textContent = "paying…";
  try {
    const { preimage } = await api("/taker/pay", { bolt11: $("pay-req").value.trim() });
    $("node-note").innerHTML = `paid, preimage <span class="preimage">${preimage}</span>`;
    $("pay-req").value = "";
  } catch (error) {
    $("node-note").textContent = `payment failed: ${error.message}`;
  } finally {
    $("pay-invoice").disabled = false;
    await refresh();
  }
}

async function makeAddress() {
  $("make-address").disabled = true;
  try {
    // Deliberately a button and not a poll: the node returns a new address on
    // every call, so asking twice a second would empty the keypool.
    const { address } = await api("/taker/address", "");
    $("address").textContent = address;
    $("address").hidden = false;
    $("node-note").textContent = "send on-chain coins to this address";
  } catch (error) {
    $("node-note").textContent = `could not get an address: ${error.message}`;
  } finally {
    $("make-address").disabled = false;
    await refresh();
  }
}

async function sendCoins() {
  $("send-coins").disabled = true;
  $("node-note").textContent = "sending…";
  try {
    const { txid } = await api("/taker/send", {
      address: $("send-addr").value.trim(),
      sats: Number($("send-amt").value),
    });
    $("node-note").innerHTML = `sent on chain, tx <span class="preimage">${txid}</span>`;
    $("send-addr").value = "";
  } catch (error) {
    $("node-note").textContent = `send failed: ${error.message}`;
  } finally {
    $("send-coins").disabled = false;
    await refresh();
  }
}

// --- wiring ----------------------------------------------------------------

$("mine").addEventListener("click", () => mine(Number($("blocks").value)));

$("blocks").addEventListener("keydown", (event) => {
  if (event.key === "Enter") mine(Number($("blocks").value));
});

for (const button of document.querySelectorAll("button[data-mine]")) {
  button.addEventListener("click", () => mine(Number(button.dataset.mine)));
}

$("make-invoice").addEventListener("click", makeInvoice);
$("pay-invoice").addEventListener("click", payInvoice);
$("make-address").addEventListener("click", makeAddress);
$("send-coins").addEventListener("click", sendCoins);

// Filters and page sizes are selects, so they arrive as changes. The panel
// remembers the choice rather than reading it back off the control, because the
// next poll repaints the list.
document.addEventListener("change", (event) => {
  const control = event.target;
  const id = listOf(control);

  if (control.dataset.field) {
    lists[id].filters[control.dataset.field] = control.value;
    lists[id].pager.page = 1;
  } else {
    const command = pagerCommand(control.closest("[data-size]"));
    if (!command?.size) return;
    lists[id].pager.size = command.size;
    lists[id].pager.page = 1;
  }

  if (last) render(last);
});

document.addEventListener("click", (event) => {
  const control = event.target.closest("[data-pager]");
  const id = listOf(control);
  const command = pagerCommand(control);
  if (!id || !command?.move) return;

  lists[id].pager.page += command.move;
  if (last) render(last);
});

await refresh();
setInterval(refresh, 2000);
