// Paging for lists

export const PAGE_SIZES = [10, 25, 50];
export const DEFAULT_PAGE_SIZE = PAGE_SIZES[0];

/** A fresh pager: first page, default size. */
export function newPager(size = DEFAULT_PAGE_SIZE) {
  return { page: 1, size };
}

/**
 * The rows to show, and what to say about them.
 *
 * The page is clamped rather than trusted: data moves under a list while it is
 * being paged, and a swap that changes tab must not leave an empty page on
 * screen.
 */
export function paginate(rows, state) {
  const size = PAGE_SIZES.includes(state.size) ? state.size : DEFAULT_PAGE_SIZE;
  const pages = Math.max(1, Math.ceil(rows.length / size));
  const page = Math.min(Math.max(1, state.page), pages);
  const from = rows.length === 0 ? 0 : (page - 1) * size + 1;
  const to = Math.min(page * size, rows.length);
  const start = from === 0 ? 0 : from - 1;

  return {
    page,
    size,
    pages,
    total: rows.length,
    from,
    to,
    rows: rows.slice(start, to),
  };
}

/** What a click or change on the pager means: `{move}`, `{size}` or null. */
export function pagerCommand(control) {
  if (!control?.dataset) return null;

  const move = control.dataset.pager;
  if (move === "prev") return { move: -1 };
  if (move === "next") return { move: 1 };

  // `data-size` is a marker with no value, so the chosen size is the control's
  // own value: present, even if empty, is what says this is a size control.
  if (control.dataset.size === undefined) return null;
  const size = Number(control.value);
  return Number.isFinite(size) && size > 0 ? { size } : null;
}

/** The pager itself, as markup. The caller puts it where the list ends. */
export function pagerHtml(view) {
  const sizes = PAGE_SIZES.map(
    (n) =>
      `<option value="${n}"${n === view.size ? " selected" : ""}>${n}</option>`,
  ).join("");

  return `<div class="pager">
      <span class="range">${view.from}\u2013${view.to} of ${view.total}</span>
      <button data-pager="prev"${view.page <= 1 ? " disabled" : ""}>\u2039 prev</button>
      <button data-pager="next"${view.page >= view.pages ? " disabled" : ""}>next \u203a</button>
      <label>per page <select data-size>${sizes}</select></label>
    </div>`;
}
