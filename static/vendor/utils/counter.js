/**
 * MIUIX CONSOLE — Counter Animation
 * 数字滚动动画
 */
class MxCounter {
  /**
   * 滚动数字到目标值
   * @param {HTMLElement} el 目标元素
   * @param {number} to 目标值
   * @param {object} [opts] 选项
   * @param {number} [opts.duration=1200] 动画时长 ms
   * @param {number} [opts.decimals=0] 小数位数
   * @param {string} [opts.suffix=''] 后缀 (如 'ms', '%')
   */
  static animate(el, to, opts = {}) {
    const { duration = 1200, decimals = 0, suffix = '' } = opts;
    const t0 = performance.now();

    const tick = (now) => {
      const p = Math.min((now - t0) / duration, 1);
      // Ease out cubic
      const ease = 1 - Math.pow(1 - p, 3);
      const v = to * ease;

      if (decimals > 0) {
        el.textContent = v.toFixed(decimals);
      } else if (to > 10000) {
        el.textContent = Math.floor(v).toLocaleString();
      } else {
        el.textContent = Math.floor(v);
      }

      if (p < 1) requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  }

  /**
   * 自动初始化所有 [data-mx-count] 元素
   */
  static init() {
    document.querySelectorAll('[data-mx-count]').forEach(el => {
      const to = parseFloat(el.dataset.mxCount);
      const decimals = parseInt(el.dataset.mxDecimals || '0');
      const suffix = el.dataset.mxSuffix || '';

      MxCounter.animate(el, to, { decimals, suffix });

      if (suffix) {
        const span = document.createElement('span');
        span.style.cssText = 'font-size:0.55em;font-weight:600;color:var(--mx-t2)';
        span.textContent = suffix;
        el.parentElement.appendChild(span);
      }
    });
  }
}

if (typeof module !== 'undefined') module.exports = MxCounter;
