/**
 * MIUIX CONSOLE — Ripple Effect
 * Miuix press feedback
 */
class MxRipple {
  static init() {
    document.addEventListener('pointerdown', e => {
      const el = e.target.closest('[data-ripple]');
      if (!el) return;
      const rect = el.getBoundingClientRect();
      const r = document.createElement('span');
      const size = Math.max(rect.width, rect.height) * 2;
      r.style.cssText = `
        position:absolute;border-radius:50%;
        background:currentColor;opacity:0.08;
        width:${size}px;height:${size}px;
        left:${e.clientX - rect.left - size/2}px;
        top:${e.clientY - rect.top - size/2}px;
        transform:scale(0);animation:mx-ripple 0.6s ease-out;
        pointer-events:none;
      `;
      el.style.position = el.style.position || 'relative';
      el.style.overflow = 'hidden';
      el.appendChild(r);
      setTimeout(() => r.remove(), 600);
    });
  }
}

if (typeof module !== 'undefined') module.exports = MxRipple;
