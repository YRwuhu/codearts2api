/**
 * MIUIX CONSOLE — Select Manager
 */
class MxSelect {
  /**
   * Initialize all .mx-select elements
   */
  static init() {
    document.addEventListener('click', e => {
      // Close all open selects first
      document.querySelectorAll('.mx-select-trigger.open').forEach(trigger => {
        const select = trigger.closest('.mx-select');
        if (!select.contains(e.target)) {
          trigger.classList.remove('open');
          trigger.nextElementSibling?.classList.remove('open');
        }
      });
    });

    // Delegate click on triggers
    document.addEventListener('click', e => {
      const trigger = e.target.closest('.mx-select-trigger');
      if (!trigger || trigger.closest('.mx-select.disabled')) return;

      e.stopPropagation();
      const dropdown = trigger.nextElementSibling;
      if (!dropdown) return;

      const isOpen = trigger.classList.contains('open');
      // Close all others
      document.querySelectorAll('.mx-select-trigger.open').forEach(t => {
        t.classList.remove('open');
        t.nextElementSibling?.classList.remove('open');
      });

      if (!isOpen) {
        trigger.classList.add('open');
        dropdown.classList.add('open');
        const search = dropdown.querySelector('.mx-select-search input');
        if (search) search.focus();
      }
    });

    // Delegate click on options
    document.addEventListener('click', e => {
      const option = e.target.closest('.mx-select-option');
      if (!option || option.classList.contains('disabled')) return;

      const dropdown = option.closest('.mx-select-dropdown');
      const select = dropdown?.closest('.mx-select');
      if (!select) return;

      const isMulti = select.dataset.multi === 'true';

      if (isMulti) {
        option.classList.toggle('selected');
        MxSelect._updateMultiValue(select);
      } else {
        dropdown.querySelectorAll('.mx-select-option').forEach(o => o.classList.remove('selected'));
        option.classList.add('selected');
        MxSelect._updateSingleValue(select, option);

        // Close dropdown
        select.querySelector('.mx-select-trigger').classList.remove('open');
        dropdown.classList.remove('open');
      }

      // Dispatch change event
      select.dispatchEvent(new CustomEvent('mx-select-change', {
        detail: { value: MxSelect.getValue(select) }
      }));
    });

    // Search filter
    document.addEventListener('input', e => {
      const search = e.target.closest('.mx-select-search input');
      if (!search) return;
      const dropdown = search.closest('.mx-select-dropdown');
      const query = search.value.toLowerCase();
      dropdown.querySelectorAll('.mx-select-option').forEach(opt => {
        const text = opt.textContent.toLowerCase();
        opt.style.display = text.includes(query) ? '' : 'none';
      });
    });
  }

  static _updateSingleValue(select, option) {
    const trigger = select.querySelector('.mx-select-trigger');
    const valueEl = trigger.querySelector('.mx-select-value');
    const placeholder = trigger.querySelector('.mx-select-placeholder');
    if (valueEl) valueEl.textContent = option.textContent.trim();
    if (placeholder) placeholder.style.display = 'none';
    if (valueEl) valueEl.style.display = '';
    trigger.dataset.value = option.dataset.value || option.textContent.trim();
  }

  static _updateMultiValue(select) {
    const trigger = select.querySelector('.mx-select-trigger');
    const selected = select.querySelectorAll('.mx-select-option.selected');
    const tagsEl = trigger.querySelector('.mx-select-tags');
    const placeholder = trigger.querySelector('.mx-select-placeholder');

    if (tagsEl) {
      tagsEl.innerHTML = '';
      selected.forEach(opt => {
        const tag = document.createElement('span');
        tag.className = 'mx-select-tag';
        tag.innerHTML = `${opt.textContent.trim()}<span class="mx-select-tag-remove" data-value="${opt.dataset.value || opt.textContent.trim()}">×</span>`;
        tagsEl.appendChild(tag);
      });
      placeholder.style.display = selected.length ? 'none' : '';
    }
  }

  /**
   * Get current value
   * @param {HTMLElement} select
   * @returns {string|string[]}
   */
  static getValue(select) {
    const isMulti = select.dataset.multi === 'true';
    if (isMulti) {
      return Array.from(select.querySelectorAll('.mx-select-option.selected'))
        .map(o => o.dataset.value || o.textContent.trim());
    }
    const selected = select.querySelector('.mx-select-option.selected');
    return selected ? (selected.dataset.value || selected.textContent.trim()) : '';
  }

  /**
   * Set value programmatically
   * @param {HTMLElement} select
   * @param {string|string[]} value
   */
  static setValue(select, value) {
    const isMulti = select.dataset.multi === 'true';
    const values = Array.isArray(value) ? value : [value];
    select.querySelectorAll('.mx-select-option').forEach(opt => {
      const v = opt.dataset.value || opt.textContent.trim();
      if (isMulti) {
        opt.classList.toggle('selected', values.includes(v));
      } else {
        opt.classList.toggle('selected', v === value);
      }
    });
    if (isMulti) MxSelect._updateMultiValue(select);
    else {
      const selected = select.querySelector('.mx-select-option.selected');
      if (selected) MxSelect._updateSingleValue(select, selected);
    }
  }
}

// Remove tag click handler
document.addEventListener('click', e => {
  const remove = e.target.closest('.mx-select-tag-remove');
  if (!remove) return;
  const select = remove.closest('.mx-select');
  const value = remove.dataset.value;
  if (select && value) {
    select.querySelectorAll('.mx-select-option').forEach(opt => {
      if ((opt.dataset.value || opt.textContent.trim()) === value) {
        opt.classList.remove('selected');
      }
    });
    MxSelect._updateMultiValue(select);
  }
});

if (typeof module !== 'undefined') module.exports = MxSelect;
