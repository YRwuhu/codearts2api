/**
 * MIUIX CONSOLE — Modal / Dialog Manager
 */
class MxModal {
  /**
   * Open a modal by id
   * @param {string} id overlay element id
   */
  static open(id) {
    const overlay = document.getElementById(id);
    if (!overlay) return;
    overlay.classList.add('open');
    document.body.style.overflow = 'hidden';

    // Close on overlay click
    overlay._clickHandler = e => {
      if (e.target === overlay) MxModal.close(id);
    };
    overlay.addEventListener('click', overlay._clickHandler);

    // Close on Escape
    overlay._escHandler = e => {
      if (e.key === 'Escape') MxModal.close(id);
    };
    document.addEventListener('keydown', overlay._escHandler);

    // Trap focus
    const modal = overlay.querySelector('.mx-modal');
    if (modal) {
      const focusable = modal.querySelectorAll('button,input,select,textarea,[tabindex]:not([tabindex="-1"])');
      if (focusable.length) focusable[0].focus();
    }
  }

  /**
   * Close a modal by id
   * @param {string} id overlay element id
   */
  static close(id) {
    const overlay = document.getElementById(id);
    if (!overlay) return;
    overlay.classList.remove('open');
    document.body.style.overflow = '';
    if (overlay._clickHandler) overlay.removeEventListener('click', overlay._clickHandler);
    if (overlay._escHandler) document.removeEventListener('keydown', overlay._escHandler);
  }

  /**
   * Show a confirmation dialog (creates dynamically)
   * @param {object} opts
   * @param {string} opts.title
   * @param {string} opts.message
   * @param {string} [opts.type='info'] info|success|warning|danger
   * @param {string} [opts.confirmText='确认']
   * @param {string} [opts.cancelText='取消']
   * @returns {Promise<boolean>}
   */
  static confirm(opts = {}) {
    return new Promise(resolve => {
      const { title = '确认', message = '', type = 'info', confirmText = '确认', cancelText = '取消' } = opts;
      const id = 'mx-confirm-' + Date.now();

      const icons = {
        info: '<svg width="24" height="24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>',
        success: '<svg width="24" height="24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/></svg>',
        warning: '<svg width="24" height="24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>',
        danger: '<svg width="24" height="24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>',
      };

      const overlay = document.createElement('div');
      overlay.id = id;
      overlay.className = 'mx-modal-overlay';
      overlay.innerHTML = `
        <div class="mx-modal mx-modal-sm">
          <div class="mx-modal-body" style="padding:24px 20px">
            <div class="mx-modal-confirm-icon ${type}">${icons[type] || icons.info}</div>
            <div class="mx-modal-confirm-title">${title}</div>
            <div class="mx-modal-confirm-text">${message}</div>
          </div>
          <div class="mx-modal-foot mx-modal-confirm-foot">
            <button class="mx-btn mx-btn-ghost" data-mx-cancel>${cancelText}</button>
            <button class="mx-btn ${type === 'danger' ? 'mx-btn-danger' : 'mx-btn-primary'}" data-mx-confirm>${confirmText}</button>
          </div>
        </div>`;

      document.body.appendChild(overlay);
      requestAnimationFrame(() => MxModal.open(id));

      overlay.querySelector('[data-mx-cancel]').onclick = () => { MxModal.close(id); overlay.remove(); resolve(false); };
      overlay.querySelector('[data-mx-confirm]').onclick = () => { MxModal.close(id); overlay.remove(); resolve(true); };
    });
  }
}

if (typeof module !== 'undefined') module.exports = MxModal;
