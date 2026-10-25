/**
 * MIUIX CONSOLE — Smooth Corner
 * Ported from Miuix SmoothCornerPath.kt → CSS clip-path
 *
 * Miuix 的 SmoothRoundedCornerShape 不用普通 border-radius，
 * 而是用贝塞尔曲线实现从直线到圆弧的连续曲率过渡。
 * 这里用 CSS clip-path: path() 实现同样的效果。
 */

class MxSmoothCorner {
  /**
   * 生成 smooth corner 的 SVG path
   * @param {number} w 宽度
   * @param {number} h 高度
   * @param {number} r 圆角半径
   * @returns {string} SVG path d 属性
   */
  static path(w, h, r) {
    r = Math.min(r, w / 2, h / 2);
    if (r <= 0) return `M0,0 H${w} V${h} H0 Z`;

    // Miuix 使用 ~0.55 的系数来平滑过渡
    // 比标准 0.5523 更柔和
    const k = 0.55;
    const c = r * k;

    return [
      `M${r},0`,
      `H${w - r}`,
      `C${w - r + c},0 ${w},${r - c} ${w},${r}`,
      `V${h - r}`,
      `C${w},${h - r + c} ${w - r + c},${h} ${w - r},${h}`,
      `H${r}`,
      `C${r - c},${h} 0,${h - r + c} 0,${h - r}`,
      `V${r}`,
      `C0,${r - c} ${r - c},0 ${r},0`,
      'Z'
    ].join(' ');
  }

  /**
   * 应用 smooth corner 到元素
   * @param {HTMLElement} el
   * @param {number} [r] 圆角半径，默认读 CSS --mx-r
   */
  static apply(el, r) {
    const rect = el.getBoundingClientRect();
    const radius = r || parseFloat(getComputedStyle(el).getPropertyValue('--mx-r')) || 16;
    const w = rect.width;
    const h = rect.height;
    if (w === 0 || h === 0) return;

    const d = MxSmoothCorner.path(w, h, radius);
    el.style.clipPath = `path('${d}')`;
  }

  /**
   * 自动监听所有 .mx-smooth 元素
   */
  static init() {
    const apply = () => {
      document.querySelectorAll('.mx-smooth').forEach(el => {
        MxSmoothCorner.apply(el);
      });
    };

    // 初始应用
    if (document.readyState === 'loading') {
      document.addEventListener('DOMContentLoaded', apply);
    } else {
      apply();
    }

    // Resize 时重新计算
    const ro = new ResizeObserver(() => apply());
    document.querySelectorAll('.mx-smooth').forEach(el => ro.observe(el));

    // MutationObserver 自动处理新增元素
    const mo = new MutationObserver(mutations => {
      mutations.forEach(m => {
        m.addedNodes.forEach(node => {
          if (node.nodeType === 1) {
            if (node.classList?.contains('mx-smooth')) {
              MxSmoothCorner.apply(node);
              ro.observe(node);
            }
            node.querySelectorAll?.('.mx-smooth').forEach(el => {
              MxSmoothCorner.apply(el);
              ro.observe(el);
            });
          }
        });
      });
    });
    mo.observe(document.body, { childList: true, subtree: true });
  }
}

// 导出
if (typeof module !== 'undefined') module.exports = MxSmoothCorner;
