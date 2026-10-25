/**
 * MIUIX CONSOLE — Chart Builder
 * 快速构建柱状图
 */
class MxChart {
  /**
   * 渲染柱状图
   * @param {HTMLElement} container
   * @param {number[]} data
   * @param {object} [opts]
   * @param {number} [opts.barDelay=25] 每条延迟 ms
   * @param {number} [opts.startDelay=300] 起始延迟 ms
   */
  static render(container, data, opts = {}) {
    const { barDelay = 25, startDelay = 300 } = opts;
    const max = Math.max(...data);
    const height = container.clientHeight || 160;

    container.innerHTML = data.map((v, i) => {
      const h = Math.round((v / max) * (height - 20));
      const alt = i % 3 === 0 ? ' alt' : '';
      return `<div class="mx-chart-bar-col">
        <div class="mx-chart-bar mx-anim-bar-grow${alt}"
             style="height:${h}px;animation-delay:${i * barDelay + startDelay}ms"></div>
      </div>`;
    }).join('');
  }

  /**
   * 重新播放动画
   * @param {HTMLElement} container
   */
  static replay(container) {
    container.querySelectorAll('.mx-chart-bar').forEach((bar, i) => {
      bar.style.animation = 'none';
      bar.offsetHeight; // reflow
      bar.style.animation = `mx-bar-grow var(--mx-d4) var(--mx-ease) ${i * 20}ms both`;
    });
  }
}

if (typeof module !== 'undefined') module.exports = MxChart;
