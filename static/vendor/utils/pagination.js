/**
 * MIUIX CONSOLE — Pagination Manager
 */
class MxPagination {
  /**
   * Render pagination into a container
   * @param {HTMLElement} container
   * @param {object} opts
   * @param {number} opts.current current page (1-based)
   * @param {number} opts.total total pages
   * @param {number} [opts.siblings=1] pages around current
   * @param {Function} [opts.onChange] callback(newPage)
   */
  static render(container, opts = {}) {
    const { current = 1, total = 1, siblings = 1, onChange } = opts;

    const range = MxPagination._getPageRange(current, total, siblings);
    let html = '';

    // Prev
    html += `<button class="mx-page-prev ${current <= 1 ? 'disabled' : ''}" data-page="${current - 1}">
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><polyline points="15 18 9 12 15 6"/></svg>
      <span>上一页</span>
    </button>`;

    // Pages
    for (const p of range) {
      if (p === '...') {
        html += '<span class="mx-page-ellipsis">…</span>';
      } else {
        html += `<button class="mx-page-btn ${p === current ? 'active' : ''}" data-page="${p}">${p}</button>`;
      }
    }

    // Next
    html += `<button class="mx-page-next ${current >= total ? 'disabled' : ''}" data-page="${current + 1}">
      <span>下一页</span>
      <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><polyline points="9 18 15 12 9 6"/></svg>
    </button>`;

    // Info
    html += `<span class="mx-page-info">${current} / ${total}</span>`;

    container.innerHTML = html;
    container.className = 'mx-pagination';

    // Click handler
    container.onclick = e => {
      const btn = e.target.closest('[data-page]');
      if (!btn || btn.classList.contains('disabled')) return;
      const page = parseInt(btn.dataset.page);
      if (page >= 1 && page <= total && page !== current && onChange) {
        onChange(page);
      }
    };
  }

  static _getPageRange(current, total, siblings) {
    if (total <= 7) return Array.from({ length: total }, (_, i) => i + 1);

    const range = [];
    const left = Math.max(2, current - siblings);
    const right = Math.min(total - 1, current + siblings);

    range.push(1);
    if (left > 2) range.push('...');
    for (let i = left; i <= right; i++) range.push(i);
    if (right < total - 1) range.push('...');
    range.push(total);

    return range;
  }
}

if (typeof module !== 'undefined') module.exports = MxPagination;
