import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";

import { encode, svg } from "./qr.mjs";

// A real deposit URI and a real regtest invoice: the two things this page draws.
const URI =
  "bitcoin:bcrt1q8kse420hutprrwwlcuhwlsrem57qdrwha58e5gspqu4cuzuzyj3q9m6d7r?amount=0.00023";
const INVOICE =
  "lnbcrt210u1p4v67mkpp5jyf83fjzpw8gkf7tgzaxmkgt57y8se3unzuercta3ryy06tqkh7qdq6vf6xxvnvdcsxxctjvss8getnwscqzzsxqyz5vqsp5hpxz3tsls6ukqlkqg9mfe6w03dlez0jytg32luzrnnzrf796636q9p4gqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqpqysgq2kyqqpm04kascwqj4ql9danxztw7wuuddnzzawfe0rzg4keky45kpy0kw7f7r7mdqww2fjsq7865rlltdcyseh53jdh8lr4";

/**
 * text, version, level, mask, and the symbol a reference encoder builds for it.
 *
 * The version, level and mask are all pinned, so a difference is a difference in
 * the symbol rather than in which mask the standard's scoring prefers. Every
 * version, level and mask was swept against the reference while this was written,
 * including payloads that fill the version exactly; these are the cases worth
 * keeping to catch a regression offline.
 */
const CASES = [
  ["abc123", 1, "L", 0, "ac3a19299d4b0601"],
  ["abc123", 1, "L", 1, "0b24211f777b2255"],
  ["abc123", 1, "L", 2, "70db50e69c65b09f"],
  ["abc123", 1, "L", 3, "3b4a396a40a8bf9c"],
  ["abc123", 1, "L", 4, "ccf131fb87726992"],
  ["abc123", 1, "L", 5, "376f8a30a160d375"],
  ["abc123", 1, "L", 6, "215c0730d344b56a"],
  ["abc123", 1, "L", 7, "96c34c76979a2eb7"],
  ["abc123", 3, "M", 0, "01e0ffd1a8a3f794"],
  ["abc123", 3, "M", 1, "a3a352b1f660b6d0"],
  ["abc123", 3, "M", 2, "3e18e6bccc73993a"],
  ["abc123", 3, "M", 3, "92d765999cfbeddb"],
  ["abc123", 3, "M", 4, "9df2d7daa278dbee"],
  ["abc123", 3, "M", 5, "d609e66acd32bdd7"],
  ["abc123", 3, "M", 6, "230b784b8b0232b9"],
  ["abc123", 3, "M", 7, "89c11a4823d0020b"],
  ["abc123", 10, "L", 0, "ba91cec99e080f5a"],
  ["abc123", 10, "M", 0, "6dd2d38ab3cca0ff"],
  ["abc123", 10, "Q", 0, "ce6ec32489335da9"],
  ["abc123", 10, "H", 0, "cfd97031c236fb2c"],
  ["abc123", 40, "M", 0, "5aa956b627d65804"],
  [URI, 6, "M", 0, "7ea464580b333f3b"],
  [INVOICE, 14, "M", 3, "59be0c0f5c456c99"],
  [INVOICE, 15, "L", 5, "f427c7f1ff170e4f"],
];

/** The symbol as text, which is what the hashes above are of. */
function asText({ modules, size }) {
  const rows = [];

  for (let r = 0; r < size; r++) {
    let row = "";
    for (let c = 0; c < size; c++) row += modules[r * size + c] ? "#" : ".";
    rows.push(row);
  }

  return rows.join("\n");
}

const digest = (text) => createHash("sha256").update(text).digest("hex").slice(0, 16);

test("every pinned version, level and mask matches a reference encoder", () => {
  for (const [text, version, level, mask, expected] of CASES) {
    const symbol = encode(text, { version, level, mask });

    assert.equal(symbol.size, version * 4 + 17);
    assert.equal(
      digest(asText(symbol)),
      expected,
      `${version}${level} mask ${mask} for ${text.slice(0, 16)}`,
    );
  }
});

test("a symbol has the patterns a scanner looks for", () => {
  const { modules, size } = encode("abc123", { level: "M" });
  const at = (r, c) => modules[r * size + c];

  for (const [top, left] of [
    [0, 0],
    [0, size - 7],
    [size - 7, 0],
  ]) {
    for (let dr = 0; dr < 7; dr++) {
      for (let dc = 0; dc < 7; dc++) {
        const border = dr === 0 || dr === 6 || dc === 0 || dc === 6;
        const centre = dr >= 2 && dr <= 4 && dc >= 2 && dc <= 4;
        assert.equal(
          at(top + dr, left + dc),
          border || centre ? 1 : 0,
          `finder at ${top},${left} module ${dr},${dc}`,
        );
      }
    }
  }

  // The timing patterns alternate, and the dark module is dark.
  for (let i = 8; i < size - 8; i++) {
    assert.equal(at(6, i), i % 2 === 0 ? 1 : 0);
    assert.equal(at(i, 6), i % 2 === 0 ? 1 : 0);
  }
  assert.equal(at(size - 8, 8), 1);
});

test("the two copies of the format information agree", () => {
  const { modules, size } = encode("abc123", { level: "Q" });
  const at = (r, c) => modules[r * size + c];

  // The first copy runs up the left of the top-left finder, the second along the
  // bottom-left corner; a scanner uses whichever it can read.
  const first = [];
  for (let i = 0; i < 15; i++) {
    if (i < 6) first.push(at(i, 8));
    else if (i < 8) first.push(at(i + 1, 8));
    else first.push(at(size - 15 + i, 8));
  }

  const second = [];
  for (let i = 0; i < 15; i++) {
    if (i < 8) second.push(at(8, size - 1 - i));
    else if (i === 8) second.push(at(8, 7));
    else second.push(at(8, 14 - i));
  }

  assert.deepEqual(second, first);
});

test("the smallest version that fits is chosen, and the mask is the cheapest", () => {
  assert.equal(encode("abc123", { level: "L" }).version, 1);

  // The standard scores the finished symbol; for this payload and level, mask 2
  // scores lowest of the eight, and the format information is part of the count.
  assert.equal(encode("abc123", { level: "L" }).mask, 2);

  // A bigger payload needs a bigger version, and a stronger level needs one at
  // least as big as a weaker one.
  const small = encode("abc123", { level: "M" }).version;
  const big = encode("x".repeat(400), { level: "M" }).version;

  assert.ok(big > small);
  assert.ok(encode("x".repeat(400), { level: "H" }).version >= big);
});

test("a payload nothing can hold is refused rather than truncated", () => {
  assert.throws(() => encode("x".repeat(3000), { level: "H" }), /too long/);
  assert.throws(() => encode("abc123", { level: "Z" }), /unknown error correction level/);
});

test("the svg is a square drawing of the same symbol", () => {
  const markup = svg(URI, { module: 4, margin: 3, level: "M" });
  const { size } = encode(URI, { level: "M" });
  const expected = (size + 6) * 4;

  assert.match(markup, new RegExp(`width="${expected}" height="${expected}"`));
  assert.match(markup, /<path fill="#000000"/);
  assert.match(markup, new RegExp(`viewBox="-3 -3 ${size + 6} ${size + 6}"`));
});
