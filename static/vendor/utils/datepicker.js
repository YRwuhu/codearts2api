/**
 * MIUIX CONSOLE — Date Picker Manager
 */
class MxDatePicker {
  /**
   * Initialize all .mx-datepicker elements
   */
  static init() {
    // Toggle dropdown
    document.addEventListener('click', e => {
      const input = e.target.closest('.mx-datepicker-input');
      if (input) {
        e.stopPropagation();
        const picker = input.closest('.mx-datepicker');
        const dropdown = picker.querySelector('.mx-datepicker-dropdown');
        const isOpen = dropdown.classList.contains('open');

        // Close all
        document.querySelectorAll('.mx-datepicker-dropdown.open').forEach(d => d.classList.remove('open'));

        if (!isOpen) {
          dropdown.classList.add('open');
          input.classList.add('open');
          MxDatePicker._renderMonth(picker);
        }
        return;
      }

      // Close if clicking outside
      if (!e.target.closest('.mx-datepicker')) {
        document.querySelectorAll('.mx-datepicker-dropdown.open').forEach(d => d.classList.remove('open'));
        document.querySelectorAll('.mx-datepicker-input.open').forEach(i => i.classList.remove('open'));
      }
    });

    // Nav buttons
    document.addEventListener('click', e => {
      const navBtn = e.target.closest('.mx-datepicker-nav button');
      if (!navBtn) return;
      const picker = navBtn.closest('.mx-datepicker');
      const dir = parseInt(navBtn.dataset.dir);
      const state = MxDatePicker._getState(picker);
      state.month += dir;
      if (state.month > 11) { state.month = 0; state.year++; }
      if (state.month < 0) { state.month = 11; state.year--; }
      MxDatePicker._renderMonth(picker);
    });

    // Day click
    document.addEventListener('click', e => {
      const day = e.target.closest('.mx-datepicker-day');
      if (!day || day.classList.contains('disabled')) return;
      const picker = day.closest('.mx-datepicker');
      const state = MxDatePicker._getState(picker);

      if (day.classList.contains('other-month')) {
        const m = parseInt(day.dataset.month);
        const y = parseInt(day.dataset.year);
        state.month = m;
        state.year = y;
      }

      const d = parseInt(day.dataset.day);
      const selected = new Date(state.year, state.month, d);
      state.selected = selected;

      MxDatePicker._renderMonth(picker);

      // Update input display
      const input = picker.querySelector('.mx-datepicker-input span:not(.placeholder)');
      const placeholder = picker.querySelector('.mx-datepicker-input .placeholder');
      if (input) {
        input.textContent = MxDatePicker._format(selected);
        input.classList.remove('placeholder');
      }
      if (placeholder) placeholder.style.display = 'none';

      // Dispatch
      picker.dispatchEvent(new CustomEvent('mx-date-change', {
        detail: { date: selected, formatted: MxDatePicker._format(selected) }
      }));

      // Close (unless range mode)
      if (picker.dataset.range !== 'true') {
        setTimeout(() => {
          picker.querySelector('.mx-datepicker-dropdown')?.classList.remove('open');
          picker.querySelector('.mx-datepicker-input')?.classList.remove('open');
        }, 150);
      }
    });
  }

  static _getState(picker) {
    if (!picker._state) {
      const now = new Date();
      picker._state = { year: now.getFullYear(), month: now.getMonth(), selected: null };
    }
    return picker._state;
  }

  static _renderMonth(picker) {
    const state = MxDatePicker._getState(picker);
    const { year, month, selected } = state;

    // Update title
    const title = picker.querySelector('.mx-datepicker-title');
    if (title) title.textContent = `${year}年 ${month + 1}月`;

    // Render days
    const daysEl = picker.querySelector('.mx-datepicker-days');
    if (!daysEl) return;

    const firstDay = new Date(year, month, 1).getDay();
    const daysInMonth = new Date(year, month + 1, 0).getDate();
    const daysInPrev = new Date(year, month, 0).getDate();
    const today = new Date();

    let html = '';

    // Previous month days
    for (let i = firstDay - 1; i >= 0; i--) {
      const d = daysInPrev - i;
      const pm = month - 1 < 0 ? 11 : month - 1;
      const py = month - 1 < 0 ? year - 1 : year;
      html += `<div class="mx-datepicker-day other-month" data-day="${d}" data-month="${pm}" data-year="${py}">${d}</div>`;
    }

    // Current month days
    for (let d = 1; d <= daysInMonth; d++) {
      const isToday = d === today.getDate() && month === today.getMonth() && year === today.getFullYear();
      const isSelected = selected && d === selected.getDate() && month === selected.getMonth() && year === selected.getFullYear();
      const cls = ['mx-datepicker-day'];
      if (isToday) cls.push('today');
      if (isSelected) cls.push('selected');
      html += `<div class="${cls.join(' ')}" data-day="${d}">${d}</div>`;
    }

    // Next month days
    const totalCells = firstDay + daysInMonth;
    const remaining = totalCells % 7 === 0 ? 0 : 7 - (totalCells % 7);
    for (let d = 1; d <= remaining; d++) {
      const nm = month + 1 > 11 ? 0 : month + 1;
      const ny = month + 1 > 11 ? year + 1 : year;
      html += `<div class="mx-datepicker-day other-month" data-day="${d}" data-month="${nm}" data-year="${ny}">${d}</div>`;
    }

    daysEl.innerHTML = html;
  }

  static _format(date) {
    const y = date.getFullYear();
    const m = String(date.getMonth() + 1).padStart(2, '0');
    const d = String(date.getDate()).padStart(2, '0');
    return `${y}-${m}-${d}`;
  }

  /**
   * Get selected date
   * @param {HTMLElement} picker
   * @returns {Date|null}
   */
  static getValue(picker) {
    return MxDatePicker._getState(picker).selected;
  }

  /**
   * Set date programmatically
   * @param {HTMLElement} picker
   * @param {Date} date
   */
  static setValue(picker, date) {
    const state = MxDatePicker._getState(picker);
    state.selected = date;
    state.year = date.getFullYear();
    state.month = date.getMonth();
    MxDatePicker._renderMonth(picker);
    const input = picker.querySelector('.mx-datepicker-input span');
    if (input) {
      input.textContent = MxDatePicker._format(date);
      input.classList.remove('placeholder');
    }
  }
}

if (typeof module !== 'undefined') module.exports = MxDatePicker;
