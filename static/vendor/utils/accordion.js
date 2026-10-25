/**
 * MIUIX CONSOLE — Accordion Manager
 */
class MxAccordion {
  /**
   * Initialize all accordions
   */
  static init() {
    document.addEventListener('click', e => {
      const header = e.target.closest('.mx-accordion-header');
      if (!header || header.classList.contains('disabled')) return;

      const item = header.closest('.mx-accordion-item');
      const accordion = header.closest('.mx-accordion');
      if (!item || !accordion) return;

      const isExclusive = accordion.dataset.exclusive !== 'false';

      if (isExclusive) {
        // Close others in same accordion
        accordion.querySelectorAll('.mx-accordion-item.open').forEach(i => {
          if (i !== item) i.classList.remove('open');
        });
      }

      item.classList.toggle('open');

      // Dispatch event
      accordion.dispatchEvent(new CustomEvent('mx-accordion-change', {
        detail: { index: Array.from(accordion.children).indexOf(item), open: item.classList.contains('open') }
      }));
    });
  }

  /**
   * Open an accordion item by index
   * @param {HTMLElement} accordion
   * @param {number} index
   */
  static open(accordion, index) {
    const items = accordion.querySelectorAll('.mx-accordion-item');
    if (items[index] && !items[index].classList.contains('open')) {
      items[index].querySelector('.mx-accordion-header')?.click();
    }
  }

  /**
   * Close an accordion item by index
   * @param {HTMLElement} accordion
   * @param {number} index
   */
  static close(accordion, index) {
    const items = accordion.querySelectorAll('.mx-accordion-item');
    if (items[index] && items[index].classList.contains('open')) {
      items[index].querySelector('.mx-accordion-header')?.click();
    }
  }
}

if (typeof module !== 'undefined') module.exports = MxAccordion;
