/**
 * MIUIX CONSOLE — Theme Manager
 * 主题切换 + 持久化
 */
class MxTheme {
  static KEY = 'mx-theme';

  /**
   * 获取当前主题
   * @returns {'dark'|'light'}
   */
  static get() {
    return document.documentElement.dataset.theme || 'dark';
  }

  /**
   * 设置主题
   * @param {'dark'|'light'} theme
   */
  static set(theme) {
    document.documentElement.dataset.theme = theme;
    try { localStorage.setItem(MxTheme.KEY, theme); } catch(e) {}

    // 更新所有主题切换按钮
    document.querySelectorAll('.mx-theme-toggle').forEach(btn => {
      const moon = btn.querySelector('.mx-moon');
      const sun = btn.querySelector('.mx-sun');
      if (moon) moon.style.display = theme === 'dark' ? '' : 'none';
      if (sun) sun.style.display = theme === 'light' ? '' : 'none';
    });

    // 派发事件
    window.dispatchEvent(new CustomEvent('mx-theme-change', { detail: { theme } }));
  }

  /**
   * 切换主题
   */
  static toggle() {
    MxTheme.set(MxTheme.get() === 'dark' ? 'light' : 'dark');
  }

  /**
   * 初始化（从 localStorage 恢复）
   */
  static init() {
    let saved;
    try { saved = localStorage.getItem(MxTheme.KEY); } catch(e) {}
    if (saved) {
      MxTheme.set(saved);
    } else {
      // 跟随系统
      const prefersDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
      MxTheme.set(prefersDark ? 'dark' : 'light');
    }

    // 监听系统主题变化
    window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', e => {
      if (!localStorage.getItem(MxTheme.KEY)) {
        MxTheme.set(e.matches ? 'dark' : 'light');
      }
    });
  }
}

/**
 * MIUIX CONSOLE — Density Manager
 */
class MxDensity {
  static KEY = 'mx-density';

  static get() {
    return document.documentElement.dataset.density || 'normal';
  }

  static set(density) {
    document.documentElement.dataset.density = density;
    try { localStorage.setItem(MxDensity.KEY, density); } catch(e) {}

    // 更新 pill group 按钮
    document.querySelectorAll('.mx-density-group').forEach(grp => {
      grp.querySelectorAll('button').forEach(btn => {
        btn.classList.toggle('active', btn.dataset.density === density);
      });
    });

    window.dispatchEvent(new CustomEvent('mx-density-change', { detail: { density } }));
  }

  static init() {
    let saved;
    try { saved = localStorage.getItem(MxDensity.KEY); } catch(e) {}
    if (saved) MxDensity.set(saved);
  }
}

if (typeof module !== 'undefined') module.exports = { MxTheme, MxDensity };
