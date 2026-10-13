// QR codes, drawn by this page, without a dependency.
//
// Byte mode only: a payment URI is bytes and nothing here needs the numeric or
// kanji modes. All 40 versions are supported, because a Lightning invoice with
// routing hints is a long string and an arbitrary ceiling would be a surprise
// rather than a limit.
//
// The two tables below are ISO/IEC 18004's own constants - the Reed-Solomon
// block structure and the alignment pattern positions - because those are data
// rather than arithmetic, and getting one digit wrong would be invisible.
// Everything else is computed here: the Galois field, the Reed-Solomon
// generators, the format and version BCH codes, the mask predicates and the
// penalty rules. The output is checked module for module against a reference
// encoder by `qr.test.mjs`.

/** Error correction levels: the two bits format information carries, and the table row. */
const LEVEL_BITS = { L: 0b01, M: 0b00, Q: 0b11, H: 0b10 };
const LEVEL_ROW = { L: 0, M: 1, Q: 2, H: 3 };

/**
 * The block structure: per version, per level (L, M, Q, H):
 * `[count, total codewords, data codewords]`, and a second group of three when
 * the version splits its data two ways.
 */
const RS_BLOCKS = [
  [[1, 26, 19], [1, 26, 16], [1, 26, 13], [1, 26, 9]],
  [[1, 44, 34], [1, 44, 28], [1, 44, 22], [1, 44, 16]],
  [[1, 70, 55], [1, 70, 44], [2, 35, 17], [2, 35, 13]],
  [[1, 100, 80], [2, 50, 32], [2, 50, 24], [4, 25, 9]],
  [[1, 134, 108], [2, 67, 43], [2, 33, 15, 2, 34, 16], [2, 33, 11, 2, 34, 12]],
  [[2, 86, 68], [4, 43, 27], [4, 43, 19], [4, 43, 15]],
  [[2, 98, 78], [4, 49, 31], [2, 32, 14, 4, 33, 15], [4, 39, 13, 1, 40, 14]],
  [[2, 121, 97], [2, 60, 38, 2, 61, 39], [4, 40, 18, 2, 41, 19], [4, 40, 14, 2, 41, 15]],
  [[2, 146, 116], [3, 58, 36, 2, 59, 37], [4, 36, 16, 4, 37, 17], [4, 36, 12, 4, 37, 13]],
  [[2, 86, 68, 2, 87, 69], [4, 69, 43, 1, 70, 44], [6, 43, 19, 2, 44, 20], [6, 43, 15, 2, 44, 16]],
  [[4, 101, 81], [1, 80, 50, 4, 81, 51], [4, 50, 22, 4, 51, 23], [3, 36, 12, 8, 37, 13]],
  [[2, 116, 92, 2, 117, 93], [6, 58, 36, 2, 59, 37], [4, 46, 20, 6, 47, 21], [7, 42, 14, 4, 43, 15]],
  [[4, 133, 107], [8, 59, 37, 1, 60, 38], [8, 44, 20, 4, 45, 21], [12, 33, 11, 4, 34, 12]],
  [[3, 145, 115, 1, 146, 116], [4, 64, 40, 5, 65, 41], [11, 36, 16, 5, 37, 17], [11, 36, 12, 5, 37, 13]],
  [[5, 109, 87, 1, 110, 88], [5, 65, 41, 5, 66, 42], [5, 54, 24, 7, 55, 25], [11, 36, 12, 7, 37, 13]],
  [[5, 122, 98, 1, 123, 99], [7, 73, 45, 3, 74, 46], [15, 43, 19, 2, 44, 20], [3, 45, 15, 13, 46, 16]],
  [[1, 135, 107, 5, 136, 108], [10, 74, 46, 1, 75, 47], [1, 50, 22, 15, 51, 23], [2, 42, 14, 17, 43, 15]],
  [[5, 150, 120, 1, 151, 121], [9, 69, 43, 4, 70, 44], [17, 50, 22, 1, 51, 23], [2, 42, 14, 19, 43, 15]],
  [[3, 141, 113, 4, 142, 114], [3, 70, 44, 11, 71, 45], [17, 47, 21, 4, 48, 22], [9, 39, 13, 16, 40, 14]],
  [[3, 135, 107, 5, 136, 108], [3, 67, 41, 13, 68, 42], [15, 54, 24, 5, 55, 25], [15, 43, 15, 10, 44, 16]],
  [[4, 144, 116, 4, 145, 117], [17, 68, 42], [17, 50, 22, 6, 51, 23], [19, 46, 16, 6, 47, 17]],
  [[2, 139, 111, 7, 140, 112], [17, 74, 46], [7, 54, 24, 16, 55, 25], [34, 37, 13]],
  [[4, 151, 121, 5, 152, 122], [4, 75, 47, 14, 76, 48], [11, 54, 24, 14, 55, 25], [16, 45, 15, 14, 46, 16]],
  [[6, 147, 117, 4, 148, 118], [6, 73, 45, 14, 74, 46], [11, 54, 24, 16, 55, 25], [30, 46, 16, 2, 47, 17]],
  [[8, 132, 106, 4, 133, 107], [8, 75, 47, 13, 76, 48], [7, 54, 24, 22, 55, 25], [22, 45, 15, 13, 46, 16]],
  [[10, 142, 114, 2, 143, 115], [19, 74, 46, 4, 75, 47], [28, 50, 22, 6, 51, 23], [33, 46, 16, 4, 47, 17]],
  [[8, 152, 122, 4, 153, 123], [22, 73, 45, 3, 74, 46], [8, 53, 23, 26, 54, 24], [12, 45, 15, 28, 46, 16]],
  [[3, 147, 117, 10, 148, 118], [3, 73, 45, 23, 74, 46], [4, 54, 24, 31, 55, 25], [11, 45, 15, 31, 46, 16]],
  [[7, 146, 116, 7, 147, 117], [21, 73, 45, 7, 74, 46], [1, 53, 23, 37, 54, 24], [19, 45, 15, 26, 46, 16]],
  [[5, 145, 115, 10, 146, 116], [19, 75, 47, 10, 76, 48], [15, 54, 24, 25, 55, 25], [23, 45, 15, 25, 46, 16]],
  [[13, 145, 115, 3, 146, 116], [2, 74, 46, 29, 75, 47], [42, 54, 24, 1, 55, 25], [23, 45, 15, 28, 46, 16]],
  [[17, 145, 115], [10, 74, 46, 23, 75, 47], [10, 54, 24, 35, 55, 25], [19, 45, 15, 35, 46, 16]],
  [[17, 145, 115, 1, 146, 116], [14, 74, 46, 21, 75, 47], [29, 54, 24, 19, 55, 25], [11, 45, 15, 46, 46, 16]],
  [[13, 145, 115, 6, 146, 116], [14, 74, 46, 23, 75, 47], [44, 54, 24, 7, 55, 25], [59, 46, 16, 1, 47, 17]],
  [[12, 151, 121, 7, 152, 122], [12, 75, 47, 26, 76, 48], [39, 54, 24, 14, 55, 25], [22, 45, 15, 41, 46, 16]],
  [[6, 151, 121, 14, 152, 122], [6, 75, 47, 34, 76, 48], [46, 54, 24, 10, 55, 25], [2, 45, 15, 64, 46, 16]],
  [[17, 152, 122, 4, 153, 123], [29, 74, 46, 14, 75, 47], [49, 54, 24, 10, 55, 25], [24, 45, 15, 46, 46, 16]],
  [[4, 152, 122, 18, 153, 123], [13, 74, 46, 32, 75, 47], [48, 54, 24, 14, 55, 25], [42, 45, 15, 32, 46, 16]],
  [[20, 147, 117, 4, 148, 118], [40, 75, 47, 7, 76, 48], [43, 54, 24, 22, 55, 25], [10, 45, 15, 67, 46, 16]],
  [[19, 148, 118, 6, 149, 119], [18, 75, 47, 31, 76, 48], [34, 54, 24, 34, 55, 25], [20, 45, 15, 61, 46, 16]],
];

/** Where the alignment patterns sit, per version. Version 1 has none. */
const ALIGNMENT = [
  [],
  [6, 18],
  [6, 22],
  [6, 26],
  [6, 30],
  [6, 34],
  [6, 22, 38],
  [6, 24, 42],
  [6, 26, 46],
  [6, 28, 50],
  [6, 30, 54],
  [6, 32, 58],
  [6, 34, 62],
  [6, 26, 46, 66],
  [6, 26, 48, 70],
  [6, 26, 50, 74],
  [6, 30, 54, 78],
  [6, 30, 56, 82],
  [6, 30, 58, 86],
  [6, 34, 62, 90],
  [6, 28, 50, 72, 94],
  [6, 26, 50, 74, 98],
  [6, 30, 54, 78, 102],
  [6, 28, 54, 80, 106],
  [6, 32, 58, 84, 110],
  [6, 30, 58, 86, 114],
  [6, 34, 62, 90, 118],
  [6, 26, 50, 74, 98, 122],
  [6, 30, 54, 78, 102, 126],
  [6, 26, 52, 78, 104, 130],
  [6, 30, 56, 82, 108, 134],
  [6, 34, 60, 86, 112, 138],
  [6, 30, 58, 86, 114, 142],
  [6, 34, 62, 90, 118, 146],
  [6, 30, 54, 78, 102, 126, 150],
  [6, 24, 50, 76, 102, 128, 154],
  [6, 28, 54, 80, 106, 132, 158],
  [6, 32, 58, 84, 110, 136, 162],
  [6, 26, 54, 82, 110, 138, 166],
  [6, 30, 58, 86, 114, 142, 170],
];

// --- the field -------------------------------------------------------------

const EXP = new Uint8Array(512);
const LOG = new Uint8Array(256);

for (let i = 0, x = 1; i < 255; i++) {
  EXP[i] = x;
  LOG[x] = i;
  x = (x << 1) ^ (x & 0x80 ? 0x11d : 0);
}
for (let i = 255; i < 512; i++) EXP[i] = EXP[i - 255];

const multiply = (a, b) => (a === 0 || b === 0 ? 0 : EXP[LOG[a] + LOG[b]]);

/** The generator polynomial for n error correction codewords, highest first. */
function generator(n) {
  let poly = [1];

  for (let i = 0; i < n; i++) {
    const next = new Array(poly.length + 1).fill(0);
    for (let j = 0; j < poly.length; j++) {
      next[j] ^= poly[j];
      next[j + 1] ^= multiply(poly[j], EXP[i]);
    }
    poly = next;
  }

  return poly;
}

/** The n error correction codewords for one block of data. */
function correction(data, n) {
  const gen = generator(n);
  const rest = new Uint8Array(data.length + n);
  rest.set(data);

  for (let i = 0; i < data.length; i++) {
    const lead = rest[i];
    if (!lead) continue;
    for (let j = 0; j < gen.length; j++) rest[i + j] ^= multiply(gen[j], lead);
  }

  return Array.from(rest.slice(data.length));
}

// --- the codewords ---------------------------------------------------------

class Bits {
  constructor() {
    this.bytes = [];
    this.length = 0;
  }

  put(value, bits) {
    for (let i = bits - 1; i >= 0; i--) {
      if (this.length % 8 === 0) this.bytes.push(0);
      if ((value >> i) & 1) this.bytes[this.bytes.length - 1] |= 0x80 >> this.length % 8;
      this.length++;
    }
  }
}

/** The blocks a version and level split the payload into: [count, size] pairs. */
function groups(version, level) {
  const entry = RS_BLOCKS[version - 1][LEVEL_ROW[level]];
  const out = [];

  for (let i = 0; i < entry.length; i += 3) out.push([entry[i], entry[i + 2]]);

  return out;
}

/** How many error correction codewords each block carries. */
function correctionSize(version, level) {
  const entry = RS_BLOCKS[version - 1][LEVEL_ROW[level]];
  return entry[1] - entry[2];
}

/** How many data codewords the version and level hold. */
function capacity(version, level) {
  return groups(version, level).reduce((sum, [count, size]) => sum + count * size, 0);
}

/** The smallest version a payload fits in, or null when none does. */
function fit(bytes, level) {
  for (let version = 1; version <= 40; version++) {
    // The header is a mode indicator and the character count, which widens once
    // there are more than 255 characters to describe.
    const header = 4 + (version < 10 ? 8 : 16);
    if (header + bytes.length * 8 <= capacity(version, level) * 8) return version;
  }

  return null;
}

/** The interleaved codewords for a payload: data first, then correction. */
function codewords(bytes, version, level) {
  const bits = new Bits();
  bits.put(0b0100, 4);
  bits.put(bytes.length, version < 10 ? 8 : 16);
  for (const byte of bytes) bits.put(byte, 8);

  // Terminator, then zeroes to a codeword boundary, then the two pad bytes the
  // standard alternates to fill the rest.
  const room = capacity(version, level) * 8 - bits.length;
  bits.put(0, Math.min(4, room));
  while (bits.length % 8 !== 0) bits.put(0, 1);

  const data = bits.bytes.slice();
  for (let i = 0; data.length < capacity(version, level); i++) {
    data.push(i % 2 === 0 ? 0xec : 0x11);
  }

  const size = correctionSize(version, level);
  const blocks = [];
  let at = 0;

  for (const [count, dataSize] of groups(version, level)) {
    for (let i = 0; i < count; i++) {
      const chunk = data.slice(at, at + dataSize);
      at += dataSize;
      blocks.push({ data: chunk, correction: correction(chunk, size) });
    }
  }

  const out = [];
  const widest = Math.max(...blocks.map((block) => block.data.length));
  for (let i = 0; i < widest; i++) {
    for (const block of blocks) if (i < block.data.length) out.push(block.data[i]);
  }
  for (let i = 0; i < size; i++) for (const block of blocks) out.push(block.correction[i]);

  return out;
}

// --- the symbol ------------------------------------------------------------

/** The eight masks, as the standard numbers them. */
const MASKS = [
  (r, c) => (r + c) % 2 === 0,
  (r) => r % 2 === 0,
  (r, c) => c % 3 === 0,
  (r, c) => (r + c) % 3 === 0,
  (r, c) => (Math.floor(r / 2) + Math.floor(c / 3)) % 2 === 0,
  (r, c) => ((r * c) % 2) + ((r * c) % 3) === 0,
  (r, c) => (((r * c) % 2) + ((r * c) % 3)) % 2 === 0,
  (r, c) => (((r + c) % 2) + ((r * c) % 3)) % 2 === 0,
];

/** The 15 bits of format information: level, mask, and a BCH check. */
function formatBits(level, mask) {
  const data = (LEVEL_BITS[level] << 3) | mask;
  let rest = data;
  for (let i = 0; i < 10; i++) rest = (rest << 1) ^ ((rest >> 9) * 0x537);

  return ((data << 10) | (rest & 0x3ff)) ^ 0x5412;
}

/** The 18 bits of version information, for versions that carry it. */
function versionBits(version) {
  let rest = version;
  for (let i = 0; i < 12; i++) rest = (rest << 1) ^ ((rest >> 11) * 0x1f25);

  return (version << 12) | (rest & 0xfff);
}

/**
 * One symbol, unmasked data and all.
 *
 * The format information is written twice: once as a placeholder, to reserve the
 * modules the data must step around, and once for real once the mask is known.
 */
function build(version, level, words, mask) {
  const size = version * 4 + 17;
  const modules = new Uint8Array(size * size);
  const fixed = new Uint8Array(size * size);

  const inside = (r, c) => r >= 0 && c >= 0 && r < size && c < size;
  const put = (r, c, dark) => {
    modules[r * size + c] = dark ? 1 : 0;
    fixed[r * size + c] = 1;
  };
  const bit = (value, at) => (value >> at) & 1;

  // The three finder patterns, each with the separator that surrounds it: the
  // pattern itself is the seven-by-seven core, and the ring around it is light.
  const finder = (top, left) => {
    for (let dr = -1; dr <= 7; dr++) {
      for (let dc = -1; dc <= 7; dc++) {
        if (!inside(top + dr, left + dc)) continue;

        const core = dr >= 0 && dr <= 6 && dc >= 0 && dc <= 6;
        const border = core && (dr === 0 || dr === 6 || dc === 0 || dc === 6);
        const centre = dr >= 2 && dr <= 4 && dc >= 2 && dc <= 4;
        put(top + dr, left + dc, border || centre);
      }
    }
  };
  finder(0, 0);
  finder(0, size - 7);
  finder(size - 7, 0);

  // The timing patterns, dark on even coordinates.
  for (let i = 8; i < size - 8; i++) {
    put(6, i, i % 2 === 0);
    put(i, 6, i % 2 === 0);
  }

  // Alignment patterns, except the three that would sit on a finder.
  const centres = ALIGNMENT[version - 1];
  const last = centres[centres.length - 1];
  for (const r of centres) {
    for (const c of centres) {
      if ((r === 6 && c === 6) || (r === 6 && c === last) || (r === last && c === 6)) continue;
      for (let dr = -2; dr <= 2; dr++) {
        for (let dc = -2; dc <= 2; dc++) {
          put(r + dr, c + dc, Math.max(Math.abs(dr), Math.abs(dc)) !== 1);
        }
      }
    }
  }

  // Version information, above the top-right and left of the bottom-left finder.
  if (version >= 7) {
    const info = versionBits(version);
    for (let i = 0; i < 18; i++) {
      const dark = bit(info, i);
      put(size - 11 + (i % 3), Math.floor(i / 3), dark);
      put(Math.floor(i / 3), size - 11 + (i % 3), dark);
    }
  }

  // The dark module, which is always dark.
  put(size - 8, 8, true);

  // The format information, placed twice, as a placeholder for now. Its fifteen
  // bits go out least significant first: up beside the top-left finder and along
  // the top, then the copies that let a scanner read the symbol whichever way up
  // it finds it.
  const reserve = (format) => {
    for (let i = 0; i < 15; i++) {
      const dark = (format >> i) & 1;

      if (i < 6) put(i, 8, dark);
      else if (i < 8) put(i + 1, 8, dark);
      else put(size - 15 + i, 8, dark);

      if (i < 8) put(8, size - 1 - i, dark);
      else if (i === 8) put(8, 7, dark);
      else put(8, 14 - i, dark);
    }
  };
  reserve(0);

  // The data, in the two-module zigzag the standard walks: right to left, up
  // then down, stepping over the sixth column and anything already placed.
  let at = 0;
  for (let right = size - 1; right >= 1; right -= 2) {
    if (right === 6) right = 5;
    for (let step = 0; step < size; step++) {
      for (let j = 0; j < 2; j++) {
        const c = right - j;
        const up = ((right + 1) & 2) === 0;
        const r = up ? size - 1 - step : step;
        const index = r * size + c;
        if (fixed[index]) continue;

        modules[index] = at < words.length * 8 ? (words[at >> 3] >> (7 - (at & 7))) & 1 : 0;
        at++;
      }
    }
  }

  // The mask, over the data modules only, and then the format information that
  // says which mask it was.
  const mask_ = MASKS[mask];
  for (let r = 0; r < size; r++) {
    for (let c = 0; c < size; c++) {
      const index = r * size + c;
      if (!fixed[index] && mask_(r, c)) modules[index] ^= 1;
    }
  }
  reserve(formatBits(level, mask));

  return modules;
}

/** The standard's penalty score: lower is a better looking symbol. */
function penalty(modules, size) {
  const at = (r, c) => modules[r * size + c];
  let score = 0;

  // Runs of five or more, in rows and columns.
  const runs = (get) => {
    for (let i = 0; i < size; i++) {
      let run = 1;
      for (let j = 1; j < size; j++) {
        if (get(i, j) === get(i, j - 1)) {
          run++;
        } else {
          if (run >= 5) score += 3 + (run - 5);
          run = 1;
        }
      }
      if (run >= 5) score += 3 + (run - 5);
    }
  };
  runs(at);
  runs((i, j) => at(j, i));

  // Blocks of four modules of one colour.
  for (let r = 0; r < size - 1; r++) {
    for (let c = 0; c < size - 1; c++) {
      const first = at(r, c);
      if (first === at(r, c + 1) && first === at(r + 1, c) && first === at(r + 1, c + 1)) {
        score += 3;
      }
    }
  }

  // Anything that looks like a finder pattern, which is what a scanner hunts
  // for and what a symbol should not sprout by accident.
  const LIKE_FINDER = [
    [1, 0, 1, 1, 1, 0, 1, 0, 0, 0, 0],
    [0, 0, 0, 0, 1, 0, 1, 1, 1, 0, 1],
  ];
  const looks = (get) => {
    for (let i = 0; i < size; i++) {
      for (let j = 0; j + 11 <= size; j++) {
        for (const pattern of LIKE_FINDER) {
          let match = true;
          for (let k = 0; k < 11 && match; k++) match = get(i, j + k) === pattern[k];
          if (match) score += 40;
        }
      }
    }
  };
  looks(at);
  looks((i, j) => at(j, i));

  // How far the dark proportion is from half.
  let dark = 0;
  for (const module of modules) dark += module;
  const percent = (dark * 100) / (size * size);
  score += Math.floor(Math.abs(percent - 50) / 5) * 10;

  return score;
}

/**
 * A QR code for this text.
 *
 * The mask is chosen the way the standard says, by the lowest penalty, unless
 * one is named - which is only useful for comparing against another encoder.
 */
export function encode(text, { level = "M", version = null, mask = null } = {}) {
  if (!(level in LEVEL_ROW)) throw new Error(`unknown error correction level ${level}`);

  const bytes = Array.from(new TextEncoder().encode(text));
  const chosen = version ?? fit(bytes, level);
  if (!chosen) throw new Error("this is too long for a QR code");

  const words = codewords(bytes, chosen, level);
  const size = chosen * 4 + 17;

  if (mask !== null) return { version: chosen, size, mask, modules: build(chosen, level, words, mask) };

  let best = null;
  for (let candidate = 0; candidate < 8; candidate++) {
    const modules = build(chosen, level, words, candidate);
    const score = penalty(modules, size);
    if (!best || score < best.score) best = { score, candidate, modules };
  }

  return { version: chosen, size, mask: best.candidate, modules: best.modules };
}

/**
 * The same code as an SVG, dark on light so a scanner has the contrast it needs
 * whatever the page is doing.
 *
 * The margin defaults to the four modules the standard asks for: a scanner that
 * cannot see where the symbol stops will not find it at all.
 */
export function svg(text, { module = 3, margin = 4, dark = "#000000", light = "#ffffff", ...options } = {}) {
  const { modules, size } = encode(text, options);
  const squares = [];

  for (let r = 0; r < size; r++) {
    for (let c = 0; c < size; c++) {
      if (modules[r * size + c]) squares.push(`M${c} ${r}h1v1h-1z`);
    }
  }

  const width = (size + margin * 2) * module;
  const box = -margin;

  return (
    `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${width}" ` +
    `viewBox="${box} ${box} ${size + margin * 2} ${size + margin * 2}" ` +
    `shape-rendering="crispEdges" role="img" aria-label="QR code">` +
    `<rect x="${box}" y="${box}" width="${size + margin * 2}" height="${size + margin * 2}" fill="${light}"/>` +
    `<path fill="${dark}" d="${squares.join("")}"/>` +
    `</svg>`
  );
}
