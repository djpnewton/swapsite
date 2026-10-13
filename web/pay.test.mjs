import assert from "node:assert/strict";
import test from "node:test";

import { bitcoinUri, lightningUri, payBlock } from "./pay.mjs";
import { encode } from "./qr.mjs";

test("a deposit becomes a BIP21 URI with the amount in BTC", () => {
  assert.equal(
    bitcoinUri("bcrt1qexample", 52000),
    "bitcoin:bcrt1qexample?amount=0.00052",
  );
  assert.equal(bitcoinUri("bcrt1qexample", 100000000), "bitcoin:bcrt1qexample?amount=1");
  assert.equal(bitcoinUri("bcrt1qexample", 1), "bitcoin:bcrt1qexample?amount=0.00000001");
  assert.equal(bitcoinUri("bcrt1qexample", 2100000000000000), "bitcoin:bcrt1qexample?amount=21000000");
});

test("an invoice becomes a URI a wallet opens", () => {
  assert.equal(lightningUri("lnbcrt1test"), "lightning:lnbcrt1test");
});

test("a payment block is a small link, a copy control and the code", () => {
  const uri = bitcoinUri("bcrt1qexample", 52000);
  const markup = payBlock(uri);
  const { modules, size } = encode(uri);

  assert.ok(markup.includes(`<a class="pay-link" href="${uri}">link</a>`));

  // With nothing else said, the copy control takes the URI itself.
  assert.ok(markup.includes(`data-copy="${uri}"`));

  // The URI is not printed anywhere: it is 300 characters for an invoice, and
  // the card already shows those above this block in full.
  assert.ok(!markup.includes(`>${uri}<`));

  // The code is of that URI and nothing else, or scanning it pays something
  // else: the modules are the encoder's, in the same order.
  const squares = [];
  for (let r = 0; r < size; r++) {
    for (let c = 0; c < size; c++) {
      if (modules[r * size + c]) squares.push(`M${c} ${r}h1v1h-1z`);
    }
  }

  assert.ok(markup.includes(`<path fill="#000000" d="${squares.join("")}"/>`));
});

test("a copy control can take a value the URI only wraps", () => {
  const address = "bcrt1qexample";
  const uri = bitcoinUri(address, 52000);

  // A wallet wants the URI; a form wants the bare address, and the query would
  // not survive being pasted into one.
  const shown = payBlock(uri, { raw: address, show: true });
  assert.ok(shown.includes(`<p class="raw">${address}</p>`));
  assert.ok(shown.includes(`data-copy="${address}"`));
  assert.ok(shown.includes(`href="${uri}"`));
  assert.ok(shown.indexOf(`<p class="raw">`) < shown.indexOf(`class="pay-link"`));

  // And it is not printed unless asked for, where the card already shows it.
  assert.ok(!payBlock(uri, { raw: address }).includes(`class="raw"`));
});
