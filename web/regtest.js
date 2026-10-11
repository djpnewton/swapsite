// The regtest control panel.
//
// Dev only. Everything it shows comes from one `/status` call, and the only
// thing it does is mine blocks - the action a swap test needs most, and the one
// nothing else should be allowed to do.

const $ = (id) => document.getElementById(id);
const sats = (n) => `${Number(n).toLocaleString()} sat`;
const short = (text, head = 10, tail = 6) =>
  text.length <= head + tail ? text : `${text.slice(0, head)}…${text.slice(-tail)}`;

/** A deadline this close is worth showing in red. Matches URGENT_BLOCKS. */
const URGENT_BLOCKS = 12;

async function api(path, body) {
  const options =
    body === undefined
      ? {}
      : {
          method: "POST",
          headers: { "Content-Type": "text/plain" },
          body: typeof body === "string" ? body : JSON.stringify(body),
        };

  const response = await fetch(path, options);
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
      </dl>
      ${
        starved
          ? `<p class="error urgent">No spendable outbound, so a payment will fail. Reset with
             <code>docker compose down -v &amp;&amp; ./scripts/regtest-init.sh</code>.</p>`
          : ""
      }
    </article>`;
}

function swapTable(swaps, height) {
  if (swaps.error) return `<p class="error">${swaps.error}</p>`;
  if (swaps.length === 0) return `<p class="quote">no swaps yet</p>`;

  const rows = swaps
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

let lastMarkup = "";

function render(status) {
  const height = typeof status.height === "number" ? status.height : null;

  const markup = [
    Object.entries(status.nodes)
      .map(([role, node]) => nodeCard(role, node))
      .join(""),
    swapTable(status.swaps, height),
  ].join("");

  // Only touch the DOM when something changed: this polls, and a repaint every
  // couple of seconds is enough to make the buttons feel unreliable.
  if (markup !== lastMarkup) {
    lastMarkup = markup;
    $("nodes").innerHTML = Object.entries(status.nodes)
      .map(([role, node]) => nodeCard(role, node))
      .join("");
    $("swaps").innerHTML = swapTable(status.swaps, height);
  }

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

// --- wiring ----------------------------------------------------------------

$("mine").addEventListener("click", () => mine(Number($("blocks").value)));

$("blocks").addEventListener("keydown", (event) => {
  if (event.key === "Enter") mine(Number($("blocks").value));
});

for (const button of document.querySelectorAll("button[data-mine]")) {
  button.addEventListener("click", () => mine(Number(button.dataset.mine)));
}

await refresh();
setInterval(refresh, 2000);
