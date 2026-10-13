// The page, driven in a real browser.
//
// The other tests read the page's modules directly, which cannot tell whether
// anything is wired to them: a button that no listener reaches looks fine to
// every unit test and does nothing for a user. This one clicks the controls and
// reads the DOM back, so it covers the part the modules cannot - the markup, the
// handlers, and the order the two have to happen in.
//
// Dependency-free by construction. Chrome speaks the DevTools protocol on a
// websocket, and Node has both a websocket client and fetch built in, so a
// driver library would be a third-party build step for something the platform
// already provides. It needs a coordinator, a maker, a relay and bitcoind
// running, which is what scripts/regtest-ui.sh is for.
//
// Usage:
//   node web/e2e.mjs --base=<coordinator> --bitcoin=<rpc-url-with-credentials>
//     --invoice=<bolt11> --refund-address=<bcrt1...> [--headed] [--chrome=<path>]
//
// `--headed` shows the browser instead of running it off-screen, which is how to
// watch a run rather than read its output. `--debug-port` moves the devtools
// port off its default of 9333.

const args = new Map(
  process.argv.slice(2).map((arg) => {
    const [key, ...rest] = arg.replace(/^--/, "").split("=");
    return [key, rest.join("=")];
  }),
);

const BASE = (args.get("base") ?? "http://127.0.0.1:8090").replace(/\/$/, "");
const BITCOIN = args.get("bitcoin") ?? "http://bitcoin:bitcoin@127.0.0.1:18443/wallet/swap";
const INVOICE = args.get("invoice") ?? "";
const REFUND_ADDRESS = args.get("refund-address") ?? "";
const CHROME = args.get("chrome") ?? "google-chrome";
const DEBUG_PORT = Number(args.get("debug-port") ?? 9333);

/**
 * Whether to watch the browser rather than run it off-screen.
 *
 * Headless by default, because a test that needs a desktop is a test that does
 * not run everywhere. Shown on request: it is the fastest way to see what a
 * failure was actually looking at.
 */
const HEADED = args.has("headed") || args.has("show");

const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// --- what the run found -----------------------------------------------------

const results = [];

/** Record one assertion. Every one of these prints, pass or fail. */
function check(name, ok, detail = "") {
  results.push({ name, ok });
  const note = ok || !detail ? "" : ` — ${detail}`;
  console.log(`${ok ? "ok:  " : "FAIL:"} ${name}${note}`);
}

// --- a browser, over the DevTools protocol ----------------------------------

/** Chrome with the debugging port open, on its own profile. */
async function launchChrome(port) {
  const { spawn } = await import("node:child_process");
  const { mkdtemp } = await import("node:fs/promises");
  const { tmpdir } = await import("node:os");
  const { join } = await import("node:path");

  // A browser already on this port would be adopted silently, and the run would
  // fail later with something unrelated: better to say what is in the way.
  try {
    await fetch(`http://127.0.0.1:${port}/json/version`);
    throw new Error(`something is already listening on the devtools port ${port}`);
  } catch (error) {
    if (String(error.message).includes("already listening")) throw error;
  }

  const profile = await mkdtemp(join(tmpdir(), "swapsite-e2e-"));
  const flags = [
    // A window on request, off-screen otherwise: the driver only needs the
    // debugging port, and a window is only worth having for a person.
    ...(HEADED ? ["--window-size=1280,1000"] : ["--headless=new"]),
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-gpu",
    "--disable-dev-shm-usage",
    "about:blank",
  ];

  const chrome = spawn(CHROME, flags, { stdio: ["ignore", "ignore", "pipe"] });

  // Said out loud when the browser is shown: it is the one difference between
  // watching a run and reading one, and worth being able to check.
  if (HEADED) {
    console.log(`chrome: ${flags.filter((flag) => !flag.includes("user-data-dir")).join(" ")}`);
  }

  try {
    // The port is open before the protocol answers, so the version endpoint is
    // what to wait on rather than the process.
    for (let attempt = 0; attempt < 100; attempt++) {
      try {
        const response = await fetch(`http://127.0.0.1:${port}/json/version`);
        const version = await response.json();
        return { chrome, profile, webSocketDebuggerUrl: version.webSocketDebuggerUrl };
      } catch {
        await delay(100);
      }
    }

    throw new Error(
      HEADED && !process.env.DISPLAY && !process.env.WAYLAND_DISPLAY
        ? "chrome never opened its debugging port, and a visible browser needs a display"
        : "chrome never opened its debugging port",
    );
  } catch (error) {
    // Started and then something went wrong: a browser left behind would hold
    // the port and turn the next run into a puzzle.
    chrome.kill();
    throw error;
  }
}

/** One CDP connection, with request ids and the events collected as they come. */
function connect(webSocketDebuggerUrl) {
  const socket = new WebSocket(webSocketDebuggerUrl);
  const pending = new Map();
  const events = [];
  let next = 1;

  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    const waiting = pending.get(message.id);
    if (waiting) {
      pending.delete(message.id);
      if (message.error) waiting.reject(new Error(message.error.message));
      else waiting.resolve(message.result);
      return;
    }
    events.push(message);
  });

  const opened = new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", () => reject(new Error("the devtools socket refused")), {
      once: true,
    });
  });

  return {
    events,
    opened,
    close: () => socket.close(),
    send(method, params = {}, sessionId) {
      const id = next++;
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject });
        socket.send(JSON.stringify({ id, method, params, ...(sessionId && { sessionId }) }));
      });
    },
  };
}

/**
 * A page to drive: real mouse clicks, real form input, and the DOM read back.
 *
 * Clicks go through `Input.dispatchMouseEvent` rather than `element.click()`,
 * because a synthesised click carries no user activation - and the clipboard
 * control is refused without one, which is exactly the kind of thing this test
 * exists to catch.
 */
async function openPage(debuggingPort, url) {
  const { chrome, profile, webSocketDebuggerUrl } = await launchChrome(debuggingPort);
  const browser = connect(webSocketDebuggerUrl);
  await browser.opened;

  const { targetId } = await browser.send("Target.createTarget", { url });
  const { sessionId } = await browser.send("Target.attachToTarget", { targetId, flatten: true });

  await browser.send("Page.enable", {}, sessionId);
  await browser.send("Runtime.enable", {}, sessionId);

  const evaluate = async (expression) => {
    const answer = await browser.send(
      "Runtime.evaluate",
      { expression, awaitPromise: true, returnByValue: true },
      sessionId,
    );
    if (answer.exceptionDetails) {
      const thrown = answer.exceptionDetails.exception;
      // The expression is carried into the error: a bare TypeError says nothing
      // about which of the page's moving parts was not there yet.
      throw new Error(
        `the page threw: ${thrown?.description ?? answer.exceptionDetails.text}` +
          `\n  in: ${expression.replace(/\s+/g, " ").slice(0, 160)}`,
      );
    }
    return answer.result.value;
  };

  /** Run something in the page with the element found first, or fail trying. */
  const withElement = async (selector, body) =>
    evaluate(
      `(() => {
         const element = document.querySelector(${JSON.stringify(selector)});
         if (!element) return null;
         element.scrollIntoView({ block: "center" });
         return (${body})(element);
       })()`,
    );

  const page = {
    evaluate,
    browser,

    text: (selector) => withElement(selector, "(el) => el.textContent.trim()"),

    /** A real click, at the element's own coordinates. */
    async click(selector) {
      const box = await withElement(
        selector,
        "(el) => { const r = el.getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; }",
      );
      if (!box) throw new Error(`nothing to click for ${selector}`);

      const at = { x: box.x, y: box.y, button: "left", clickCount: 1 };
      await browser.send("Input.dispatchMouseEvent", { type: "mousePressed", ...at }, sessionId);
      await browser.send("Input.dispatchMouseEvent", { type: "mouseReleased", ...at }, sessionId);
    },

    /** Type into a field the way the page listens for, so its state updates. */
    fill: (selector, value) =>
      withElement(selector, `(el) => {
        el.value = ${JSON.stringify(value)};
        el.dispatchEvent(new Event("input", { bubbles: true }));
        el.dispatchEvent(new Event("change", { bubbles: true }));
      }`),

    /** Wait for something in the page to become true, or say what never did. */
    async waitFor(label, expression, timeoutMs = 30_000) {
      const deadline = Date.now() + timeoutMs;
      while (Date.now() < deadline) {
        if (await evaluate(expression)) return true;
        await delay(250);
      }
      throw new Error(`waited ${timeoutMs}ms for ${label}`);
    },

    /** Anything the page logged as an error or threw, for the run to report. */
    pageErrors() {
      return browser.events
        .filter((event) => event.method === "Runtime.exceptionThrown")
        .map((event) => event.params.exceptionDetails.exception?.description ?? "exception")
        .concat(
          browser.events
            .filter(
              (event) =>
                event.method === "Runtime.consoleAPICalled" && event.params.type === "error",
            )
            .map((event) => event.params.args.map((arg) => arg.value ?? arg.description).join(" ")),
        );
    },

    async stop() {
      browser.close();
      chrome.kill();

      const { rm } = await import("node:fs/promises");
      await rm(profile, { recursive: true, force: true }).catch(() => {});
    },
  };

  return page;
}

// --- bitcoind, for the parts a wallet would do ------------------------------

const bitcoin = (() => {
  const url = new URL(BITCOIN);
  const credentials = Buffer.from(`${url.username}:${url.password}`).toString("base64");
  url.username = "";
  url.password = "";

  return async (method, params = []) => {
    // The wallet name lives in the path, and the wallet RPCs need it.
    const response = await fetch(url.toString(), {
      method: "POST",
      headers: { "content-type": "text/plain", authorization: `Basic ${credentials}` },
      body: JSON.stringify({ jsonrpc: "1.0", id: "e2e", method, params }),
    });
    const answer = await response.json();
    if (answer.error) throw new Error(`bitcoind ${method}: ${answer.error.message}`);
    return answer.result;
  };
})();

const toBtc = (sats) => (sats / 100000000).toFixed(8);

const height = () => bitcoin("getblockcount");

/** Mine up to a height, one batch: the point is to pass a locktime, not to wait. */
async function mineTo(target) {
  const now = await height();
  if (target > now) {
    await bitcoin("generatetoaddress", [target - now, await bitcoin("getnewaddress")]);
  }
  return height();
}

/** Send exactly `sats` to `address` and say which output paid it. */
async function fund(address, sats) {
  const txid = await bitcoin("sendtoaddress", [address, toBtc(sats)]);
  await bitcoin("generatetoaddress", [1, await bitcoin("getnewaddress")]);

  const tx = await bitcoin("getrawtransaction", [txid, true]);
  const paid = tx.vout.find((out) => out.scriptPubKey.address === address);
  if (!paid) throw new Error(`${txid} does not pay ${address}`);

  return { txid, vout: paid.n, sats: Math.round(paid.value * 100000000) };
}

const api = async (path) => {
  const response = await fetch(`${BASE}${path}`);
  const body = await response.json();
  if (!response.ok) throw new Error(`${path}: ${body.error ?? response.status}`);
  return body;
};

/**
 * One row of a card, by the label that names it.
 *
 * A card holds several values of the same shape - an outpoint, a transaction id
 * - so taking the first match would read whichever happens to come first.
 */
function rowText(page, card, label) {
  return page.evaluate(
    `(() => {
       const labels = [...document.querySelectorAll(${JSON.stringify(`${card} dt`)})];
       const found = labels.find((dt) => dt.textContent.trim() === ${JSON.stringify(label)});
       return found?.nextElementSibling?.textContent.trim() ?? null;
     })()`,
  );
}

// --- the flows --------------------------------------------------------------

/**
 * The Lightning-first direction, up to the point where money moves.
 *
 * Stopped at the bond: paying it would set the maker funding a deposit, and this
 * maker is deliberately not ticking, because the other flow needs an invoice
 * that is never paid.
 */
async function lightningFirst(page) {
  await page.evaluate(`document.querySelector('[data-direction="ln2btc"]').click()`);
  await page.fill("#amount", "30000");
  await page.click("#create");
  await page.waitFor('a card for the new swap', "!!document.querySelector('#swaps article')");

  const card = "#swaps article";
  check(
    "the card's link is a lightning: URI named link",
    (await page.text(`${card} .pay-link`)) === "link",
  );
  check(
    "the link opens the bond invoice",
    (await page.evaluate(`document.querySelector('${card} .pay-link').getAttribute('href')`)).startsWith(
      "lightning:lnbcrt",
    ),
  );

  const copied = await page.evaluate(`document.querySelector('${card} .copy').dataset.copy`);
  check("the copy control carries the whole invoice", copied?.startsWith("lnbcrt") && copied.length > 200);

  check("the card draws a payment code", await page.evaluate(`!!document.querySelector('${card} .qr svg')`));

  // A real click, so the page has the user activation a clipboard write wants.
  await page.click(`${card} .copy`);
  await delay(400);
  check(
    "clicking copy reports that it copied",
    (await page.text(`${card} .copy`)) === "copied",
    await page.text(`${card} .copy`),
  );

  // The tabs, over the list this page holds.
  await page.evaluate(`[...document.querySelectorAll('#swaps .tabs button')].find((b) => b.textContent.includes('Closed'))?.click()`);
  await delay(300);
  check(
    "the Closed tab holds nothing",
    (await page.evaluate(`!document.querySelector('#swaps article')`)) === true &&
      (await page.text("#swaps p")).includes("Nothing closed"),
  );

  await page.evaluate(`[...document.querySelectorAll('#swaps .tabs button')].find((b) => b.textContent.includes('Active'))?.click()`);
  await delay(300);
  check(
    "the Active tab holds the new swap again",
    await page.evaluate(`!!document.querySelector('#swaps article')`),
  );
}

/**
 * The on-chain-first direction, all the way through a refund.
 *
 * This is the flow the page could not finish before: a maker that takes a
 * deposit and never pays the invoice leaves the taker with one move, and it has
 * to be reachable from the card rather than from a command line.
 */
async function onchainFirst(page) {
  await page.evaluate(`document.querySelector('[data-direction="btc2ln"]').click()`);
  await page.fill("#invoice", INVOICE);
  await page.click("#start");
  await page.waitFor("the offer card", "!!document.querySelector('#btc2ln-swaps article')");

  const card = "#btc2ln-swaps article";
  const shown = await page.text(`${card} .raw`);
  const offered = (await api("/btc2ln/swaps")).swaps.find((swap) => swap.htlc_address === shown);
  check(
    "the address on the card is the HTLC the maker built",
    Boolean(offered),
    shown,
  );
  if (!offered) return;

  check(
    "the copy control carries the bare address, not the URI",
    (await page.evaluate(`document.querySelector('${card} .copy').dataset.copy`)) === shown,
  );
  check("the card draws a payment code", await page.evaluate(`!!document.querySelector('${card} .qr svg')`));
  check(
    "reporting asks for a transaction id alone",
    (await page.text(`${card} label`)) === "The transaction id",
  );

  // A deposit, from a wallet the page does not control.
  const deposit = await fund(shown, Number(offered.deposit_sat));

  await page.fill(`${card} input[data-txid]`, deposit.txid);
  await page.click(`${card} button[data-act=report]`);
  await page.waitFor(
    "the maker to accept the deposit",
    `document.querySelector('#btc2ln-swaps article .badge')?.textContent === "funded"`,
  );

  const reported = await rowText(page, "#btc2ln-swaps article", "reported");
  check(
    "the card reports the outpoint the page worked out",
    reported === `${deposit.txid}:${deposit.vout}`,
    reported,
  );

  // Past the locktime, with the invoice still unpaid: the maker's side of the
  // swap is over, and the deposit is the taker's to take back.
  await mineTo(Number(offered.locktime));
  await page.waitFor(
    "the refund controls",
    "!!document.querySelector('#btc2ln-swaps button[data-act=refund]')",
    40_000,
  );
  check("the card offers the refund once the locktime has passed", true);

  await page.fill("#btc2ln-swaps input[data-refund]", REFUND_ADDRESS);
  const promised = Number(
    (await page.text("#btc2ln-swaps button[data-act=refund]")).replace(/[^0-9]/g, ""),
  );
  await page.click("#btc2ln-swaps button[data-act=refund]");
  await page.waitFor(
    "the refund to be broadcast",
    `document.querySelector('#btc2ln-swaps article .badge')?.textContent === "refunded"`,
  );

  const refunded = await rowText(page, "#btc2ln-swaps article", "refunded");
  check("the card names the refund transaction", /^[0-9a-f]{64}$/.test(refunded ?? ""), refunded);

  // What the page claims has to be what the chain did.
  const spent = await bitcoin("gettxout", [deposit.txid, deposit.vout]);
  check("the deposit is no longer unspent on chain", spent === null);

  const tx = await bitcoin("getrawtransaction", [refunded, true]);
  const paid = tx.vout.find((out) => out.scriptPubKey.address === REFUND_ADDRESS);
  check(
    "the refund paid the address the taker named",
    Boolean(paid),
    tx.vout.map((out) => out.scriptPubKey.address).join(","),
  );
  if (paid) {
    const sats = Math.round(paid.value * 100000000);
    check(
      "the refund is the deposit less a fee",
      sats < deposit.sats && sats > deposit.sats - 10_000,
      `${sats} of ${deposit.sats}`,
    );
    // The button said what the taker would get, so it has to be that.
    check(
      "the card promised the amount the chain paid",
      sats === promised,
      `promised ${promised}, paid ${sats}`,
    );
  }

  // A swap the list watched finish is held for a few seconds rather than
  // vanishing the moment it is done, so the card is expected to linger.
  await page.waitFor(
    "the refunded card to leave the active list",
    `[...document.querySelectorAll('#btc2ln-swaps .tabs button')].some((b) => b.textContent.startsWith('Active 0'))`,
    30_000,
  );
  check("the refunded card leaves the active list", true);
}

// --- the run ----------------------------------------------------------------

async function main() {
  for (const [name, value] of [
    ["--invoice", INVOICE],
    ["--refund-address", REFUND_ADDRESS],
  ]) {
    if (!value) throw new Error(`${name} is required`);
  }

  const page = await openPage(DEBUG_PORT, `${BASE}/`);
  if (HEADED) console.log(`browser: visible, devtools on port ${DEBUG_PORT}`);
  try {
    await page.waitFor(
      "the page to reach the coordinator",
      `document.getElementById("foot").textContent.startsWith("key ")`,
    );
    check("the page loads without a console error", page.pageErrors().length === 0, page.pageErrors()[0]);

    await lightningFirst(page);
    await onchainFirst(page);

    check("the page logged no error while it was driven", page.pageErrors().length === 0, page.pageErrors().join(" | "));
  } finally {
    await page.stop();
  }

  const failed = results.filter((result) => !result.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed at ${BASE}`);
  if (failed.length > 0) {
    console.log(`failed: ${failed.map((result) => result.name).join(", ")}`);
    process.exitCode = 1;
  }
}

main().catch((error) => {
  console.error(`FAIL: ${error.message}`);
  process.exitCode = 1;
});
