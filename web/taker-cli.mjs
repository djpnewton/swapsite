#!/usr/bin/env node
//
// The taker's operations, one per invocation, so a shell script can drive them
// the same way it drives `swapx`. Every piece of cryptography happens in the
// wasm module; this file only marshals arguments and talks to the relay.
//
//   SWAPSITE_WASM=... SWAPSITE_RELAY=http://127.0.0.1:3100 \
//     node web/taker-cli.mjs keypair
import { argv, env, exit } from "node:process";

import { relay } from "./relay.mjs";
import { loadTaker } from "./taker-node.mjs";

const WASM =
  env.SWAPSITE_WASM ?? "target/wasm32-unknown-unknown/release/swapsite_wasm.wasm";
const RELAY = env.SWAPSITE_RELAY ?? "http://127.0.0.1:3000";

/** `--a=1` and `--a 1` both into `{a: "1"}`, so a shell can use either. */
function parseFlags(args) {
  const flags = {};
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (!argument.startsWith("--")) {
      continue;
    }
    const [key, ...rest] = argument.slice(2).split("=");
    if (rest.length > 0) {
      flags[key] = rest.join("=");
    } else {
      flags[key] = args[index + 1];
      index += 1;
    }
  }
  return flags;
}

function emit(pairs) {
  for (const [key, value] of Object.entries(pairs)) {
    console.log(`${key}=${value}`);
  }
}

function need(flags, name) {
  const value = flags[name];
  if (value === undefined || value === "") {
    throw new Error(`missing --${name}`);
  }
  return value;
}

const taker = await loadTaker(WASM);
const chain = relay(RELAY);
const flags = parseFlags(argv.slice(3));

/** The public description of the swap, as the maker published it. */
function swap() {
  return {
    payment_hash: need(flags, "payment-hash"),
    locktime: Number(need(flags, "locktime")),
    claim_pubkey: need(flags, "claim-pubkey"),
    refund_pubkey: need(flags, "refund-pubkey"),
  };
}

try {
  switch (argv[2]) {
    case "keypair": {
      const { secret, pubkey } = taker.keypair();
      emit({ secret, pubkey });
      break;
    }

    case "height": {
      emit({ height: await chain.height() });
      break;
    }

    case "verify-deposit": {
      // The taker rebuilds the address from public values and then checks that
      // the deposit really pays it. A relay cannot fake this: the bytes have to
      // match the script.
      const expected = taker.htlcAddress(swap());

      const txid = need(flags, "txid");
      const tx = await chain.transaction(txid);
      if (!tx) {
        throw new Error(`the relay does not know ${txid}`);
      }

      const found = taker.findOutput({ tx_hex: tx.hex, address: expected.address });
      emit({
        address: expected.address,
        redeem_script: expected.redeem_script,
        vout: found.vout,
        sats: found.sats,
        confirmations: tx.confirmations,
      });
      break;
    }

    case "check-join": {
      const join = taker.checkJoin({ ...swap(), preimage: need(flags, "preimage") });
      emit({ joined: true, address: join.address, payment_hash: join.payment_hash });
      break;
    }

    case "assess": {
      // The deadline is the HTLC's own locktime, and the height comes from the
      // chain rather than from the caller, so the two cannot disagree. Same for
      // the deposit: the taker asks the relay whether it is still there instead
      // of taking anyone's word for it.
      const preimage = flags.preimage;
      let deposit_unspent = true;
      if (flags.outpoint) {
        deposit_unspent = (await chain.unspent(flags.outpoint)).unspent;
      }

      const recovery = taker.assess({
        state: need(flags, "state"),
        has_preimage: preimage !== undefined && preimage !== "",
        deposit_unspent,
        deadline: Number(need(flags, "locktime")),
        height: Number(flags.height ?? (await chain.height())),
      });
      emit({
        action: recovery.action.kind,
        blocks_left: recovery.action.blocks_left ?? "",
        blocks_since: recovery.action.blocks_since ?? "",
        deadline: recovery.deadline,
        height: recovery.height,
        deposit_unspent,
        urgent: recovery.urgent,
      });
      break;
    }

    case "claim": {
      // No `claim_pubkey` here: the claimant is the owner of `claim-key`, and
      // the wasm module derives the pubkey from it.
      const claimed = taker.claim({
        claim_key: need(flags, "claim-key"),
        payment_hash: need(flags, "payment-hash"),
        locktime: Number(need(flags, "locktime")),
        refund_pubkey: need(flags, "refund-pubkey"),
        preimage: need(flags, "preimage"),
        outpoint: need(flags, "outpoint"),
        value: Number(need(flags, "value")),
        to: need(flags, "to"),
        fee: Number(flags.fee ?? 1000),
      });
      emit({ txid: claimed.txid, tx_hex: claimed.tx_hex });
      break;
    }

    case "broadcast": {
      emit({ txid: await chain.broadcast(need(flags, "tx-hex")) });
      break;
    }

    default:
      throw new Error(
        "usage: taker-cli.mjs <keypair|height|verify-deposit|check-join|assess|claim|broadcast>",
      );
  }
} catch (error) {
  console.error(`taker: ${error.message}`);
  exit(1);
}
