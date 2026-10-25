/**
 * MIUIX CONSOLE — Framework Entry Point
 * 引入此文件自动初始化所有组件
 *
 * <script src="miuix-console.js"></script>
 */

(function() {
  'use strict';

  // 动态加载 CSS（如果未手动引入）
  function loadCSS(href) {
    if (document.querySelector(`link[href="${href}"]`)) return;
    const link = document.createElement('link');
    link.rel = 'stylesheet';
    link.href = href;
    document.head.appendChild(link);
  }

  // 获取当前脚本路径
  const scripts = document.querySelectorAll('script[src*="miuix-console"]');
  const currentScript = scripts[scripts.length - 1];
  const basePath = currentScript ? currentScript.src.replace(/\/[^/]*$/, '/') : './';

  // 加载所有 CSS
  const cssFiles = [
    // Core
    'core/tokens.css',
    'core/base.css',
    'core/animations.css',
    // Components — original
    'components/card.css',
    'components/button.css',
    'components/switch.css',
    'components/badge.css',
    'components/input.css',
    'components/navigation.css',
    'components/table.css',
    'components/stat.css',
    'components/activity.css',
    'components/layout.css',
    // Components — new
    'components/modal.css',
    'components/toast.css',
    'components/tooltip.css',
    'components/popover.css',
    'components/select.css',
    'components/checkbox.css',
    'components/tabs.css',
    'components/pagination.css',
    'components/progress.css',
    'components/upload.css',
    'components/accordion.css',
    'components/empty.css',
    'components/avatar.css',
    'components/drawer.css',
    'components/slider.css',
    'components/tag.css',
    'components/datepicker.css',
    'components/tree.css',
    // Glass effect
    'core/glass.css',
  ];
  cssFiles.forEach(f => loadCSS(basePath + f));

  // DOM Ready 后初始化
  function init() {
    // 加载工具脚本
    const utils = [
      // Original
      'utils/smooth-corner.js',
      'utils/ripple.js',
      'utils/counter.js',
      'utils/theme.js',
      'utils/chart.js',
      // New
      'utils/modal.js',
      'utils/toast.js',
      'utils/select.js',
      'utils/tabs.js',
      'utils/pagination.js',
      'utils/accordion.js',
      'utils/datepicker.js',
      'utils/upload.js',
      'utils/glass.js',
    ];

    let loaded = 0;
    utils.forEach(f => {
      const script = document.createElement('script');
      script.src = basePath + f;
      script.onload = () => {
        loaded++;
        if (loaded === utils.length) {
          // 所有工具加载完毕，初始化
          if (typeof MxSmoothCorner !== 'undefined') MxSmoothCorner.init();
          if (typeof MxRipple !== 'undefined') MxRipple.init();
          if (typeof MxCounter !== 'undefined') MxCounter.init();
          if (typeof MxTheme !== 'undefined') MxTheme.init();
          if (typeof MxDensity !== 'undefined') MxDensity.init();
          if (typeof MxSelect !== 'undefined') MxSelect.init();
          if (typeof MxTabs !== 'undefined') MxTabs.init();
          if (typeof MxAccordion !== 'undefined') MxAccordion.init();
          if (typeof MxDatePicker !== 'undefined') MxDatePicker.init();
          if (typeof MxUpload !== 'undefined') MxUpload.init();
          if (typeof MxGlass !== 'undefined') MxGlass.init();

          // 派发就绪事件
          window.dispatchEvent(new CustomEvent('mx-ready'));
          console.log('%c✦ Miuix Console Framework loaded', 'color:#4C8BF5;font-weight:bold');
        }
      };
      document.head.appendChild(script);
    });
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();
