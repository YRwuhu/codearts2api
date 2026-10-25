/**
 * MIUIX CONSOLE — Tabs Manager
 */
class MxTabs {
  /**
   * Initialize all .mx-tabs groups
   */
  static init() {
    document.addEventListener('click', e => {
      const tab = e.target.closest('.mx-tab');
      if (!tab || tab.classList.contains('disabled')) return;

      const tabs = tab.closest('.mx-tabs');
      if (!tabs) return;

      // Deactivate all tabs in this group
      tabs.querySelectorAll('.mx-tab').forEach(t => t.classList.remove('active'));
      tab.classList.add('active');

      // Show corresponding panel
      const panelId = tab.dataset.panel;
      if (panelId) {
        const parent = tabs.parentElement;
        parent.querySelectorAll('.mx-tab-panel').forEach(p => p.classList.remove('active'));
        const panel = document.getElementById(panelId);
        if (panel) panel.classList.add('active');
      }

      // Dispatch event
      tabs.dispatchEvent(new CustomEvent('mx-tab-change', {
        detail: { index: Array.from(tabs.querySelectorAll('.mx-tab')).indexOf(tab), panelId }
      }));
    });
  }

  /**
   * Activate a tab by index
   * @param {HTMLElement} tabsEl
   * @param {number} index
   */
  static activate(tabsEl, index) {
    const tabs = tabsEl.querySelectorAll('.mx-tab');
    if (tabs[index]) tabs[index].click();
  }
}

if (typeof module !== 'undefined') module.exports = MxTabs;
