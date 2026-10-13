// The list both directions show: tabs over swaps, and the rule for letting one
// go.
//
// Shared rather than written twice, because the two lists are meant to read
// alike, and the interesting part - when a swap stops being shown - is the same
// question in both. The attribute is a parameter because each list has its own
// click handler scoped to its own container, and a shared name would let one
// list's tabs drive the other's.

/** The tabs, in the order they are shown. */
export const TABS = [
  ["active", "Active"],
  ["closed", "Closed"],
  ["all", "All"],
];

/**
 * How long a swap the list watched finish stays put.
 *
 * Long enough to read the line that says it worked: the card is the only place
 * that says so, and the poll that removes it comes three seconds after.
 */
export const HOLD_MS = 15_000;

/** The tab bar, as markup. */
export function tabBar(tab, counts, attribute = "data-tab") {
  const buttons = TABS.map(
    ([key, label]) =>
      `<button ${attribute}="${key}"${key === tab ? ' class="on"' : ""}>` +
      `${label} <span class="count">${counts[key]}</span></button>`,
  ).join("");

  return `<div class="tabs">${buttons}</div>`;
}

/**
 * One list's state: the chosen tab, and which swaps it has let go.
 *
 * `isDone` answers "is this swap over with", and `now` is injectable so the
 * hold can be tested without waiting for it.
 */
export function makeList(isDone, holdMs = HOLD_MS) {
  // Ids seen while they were still not over, when each was first seen over.
  const seenOpen = new Set();
  const holding = new Map();
  let tab = "active";

  /**
   * Whether the list has let a swap go yet.
   *
   * Later than `isDone`, deliberately, so a swap the list watched finish keeps
   * its card long enough to say what happened. A swap that was already over
   * when the page arrived goes at once: nothing was watched, and a reload is
   * not a moment to explain.
   */
  function gone(id, now = Date.now()) {
    if (!isDone(id)) {
      seenOpen.add(id);
      holding.delete(id);
      return false;
    }

    if (!seenOpen.has(id)) return true;

    const since = holding.get(id) ?? now;
    holding.set(id, since);
    return now - since >= holdMs;
  }

  return {
    get tab() {
      return tab;
    },

    choose(next) {
      tab = next;
    },

    gone,

    /** How many swaps each tab holds. */
    counts(ids, now) {
      return {
        active: ids.filter((id) => !gone(id, now)).length,
        closed: ids.filter((id) => gone(id, now)).length,
        all: ids.length,
      };
    },

    /** The swaps in the chosen tab, in the order they were given. */
    pick(ids, now) {
      return ids.filter((id) => tab === "all" || (tab === "closed") === gone(id, now));
    },
  };
}
