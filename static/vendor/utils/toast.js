/**
 * MIUIX CONSOLE — Toast Manager
 * 轻量反馈 — 底部弹出 + 自动消失
 */
class MxToast {
  static _container = null;
  static _id = 0;

  static _getContainer() {
    if (!MxToast._container || !document.body.contains(MxToast._container)) {
      MxToast._container = document.createElement('div');
      MxToast._container.className = 'mx-toast-container';
      document.body.appendChild(MxToast._container);
    }
    return MxToast._container;
  }

  /**
   * Show a toast
   * @param {object} opts
   * @param {string} opts.message
   * @param {string} [opts.type='info'] success|warning|error|info
   * @param {number} [opts.duration=3000] ms, 0 = sticky
   * @param {string} [opts.action] action button text
   * @param {Function} [opts.onAction] action callback
   * @returns {string} toast id
   */
  static show(opts = {}) {
    const { message = '', type = 'info', duration = 3000, action, onAction } = opts;
    const container = MxToast._getContainer();
    const id = 'mx-toast-' + (++MxToast._id);

    const icons = {
      success: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/></svg>',
      warning: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>',
      error: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>',
      info: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><circle cx="12" cy="12" r="10"/><line x1="12" y1="16" x2="12" y2="12"/><line x1="12" y1="8" x2="12.01" y2="8"/></svg>',
    };

    const toast = document.createElement('div');
    toast.id = id;
    toast.className = `mx-toast mx-toast-${type}`;
    toast.style.position = 'relative';
    toast.innerHTML = `
      <span class="mx-toast-icon">${icons[type] || icons.info}</span>
      <span style="flex:1">${message}</span>
      ${action ? `<button class="mx-toast-action">${action}</button>` : ''}
      <button class="mx-toast-close" data-mx-dismiss>
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
      </button>
      ${duration > 0 ? '<div class="mx-toast-progress" style="width:100%"></div>' : ''}`;

    container.appendChild(toast);
    requestAnimationFrame(() => toast.classList.add('show'));

    // Action button
    if (action && onAction) {
      toast.querySelector('.mx-toast-action').onclick = () => {
        onAction();
        MxToast.dismiss(id);
      };
    }

    // Close button
    toast.querySelector('[data-mx-dismiss]').onclick = () => MxToast.dismiss(id);

    // Auto dismiss
    if (duration > 0) {
      const progress = toast.querySelector('.mx-toast-progress');
      if (progress) {
        requestAnimationFrame(() => {
          progress.style.transitionDuration = duration + 'ms';
          progress.style.width = '0%';
        });
      }
      setTimeout(() => MxToast.dismiss(id), duration);
    }

    return id;
  }

  /**
   * Dismiss a toast by id
   * @param {string} id
   */
  static dismiss(id) {
    const toast = document.getElementById(id);
    if (!toast) return;
    toast.classList.remove('show');
    toast.classList.add('hide');
    setTimeout(() => toast.remove(), 300);
  }

  /** Shorthand: success */
  static success(message, opts = {}) { return MxToast.show({ ...opts, message, type: 'success' }); }
  /** Shorthand: warning */
  static warning(message, opts = {}) { return MxToast.show({ ...opts, message, type: 'warning' }); }
  /** Shorthand: error */
  static error(message, opts = {}) { return MxToast.show({ ...opts, message, type: 'error' }); }
  /** Shorthand: info */
  static info(message, opts = {}) { return MxToast.show({ ...opts, message, type: 'info' }); }
}

if (typeof module !== 'undefined') module.exports = MxToast;
