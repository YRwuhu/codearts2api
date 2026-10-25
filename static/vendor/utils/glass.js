/**
 * MIUIX CONSOLE — Liquid Glass Manager v2
 * 液体玻璃效果管理器（完整版）
 *
 * 基于 rdev/liquid-glass-react (⭐4800+) 的原理移植到 Vanilla JS：
 * 1. 4 种位移贴图模式：standard / polar / prominent / shader
 * 2. SVG filter 做 RGB 三通道分离色差 + 边缘遮罩
 * 3. Canvas 动态生成位移贴图（含 shader 模式的 SDF 生成器）
 * 4. backdrop-filter 做模糊和饱和度增强
 * 5. 鼠标跟踪 + 弹性位移 + 方向性缩放
 * 6. 双层动态边框（跟随鼠标角度的渐变高光）
 * 7. Over-light 模式
 * 8. Hover / Active 状态
 */

class MxGlass {
  static _initialized = false;
  static _svgFilter = null;
  static _mousePos = { x: 0, y: 0 };
  static _mouseOffset = { x: 0, y: 0 };
  static _trackedElements = new Set();
  static _animationFrame = null;

  // ── 预置位移贴图 ──

  /** Standard: 径向渐变（默认） */
  static _generateStandardMap(width, height) {
    const canvas = document.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext('2d');

    ctx.fillStyle = 'rgb(128, 128, 128)';
    ctx.fillRect(0, 0, width, height);

    const gradient = ctx.createRadialGradient(
      width / 2, height / 2, Math.min(width, height) * 0.1,
      width / 2, height / 2, Math.max(width, height) * 0.5
    );
    gradient.addColorStop(0, 'rgb(128, 128, 128)');
    gradient.addColorStop(0.5, 'rgb(128, 128, 128)');
    gradient.addColorStop(0.8, 'rgb(100, 128, 150)');
    gradient.addColorStop(1, 'rgb(80, 128, 170)');

    ctx.fillStyle = gradient;
    ctx.fillRect(0, 0, width, height);
    return canvas.toDataURL('image/jpeg', 0.9);
  }

  /** Polar: 极坐标位移 */
  static _generatePolarMap(width, height) {
    const canvas = document.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext('2d');
    const imageData = ctx.createImageData(width, height);
    const data = imageData.data;

    const cx = width / 2;
    const cy = height / 2;
    const maxR = Math.max(cx, cy);

    for (let y = 0; y < height; y++) {
      for (let x = 0; x < width; x++) {
        const dx = x - cx;
        const dy = y - cy;
        const r = Math.sqrt(dx * dx + dy * dy) / maxR;
        const angle = (Math.atan2(dy, dx) + Math.PI) / (2 * Math.PI);

        const i = (y * width + x) * 4;
        data[i]     = Math.floor(128 + (r - 0.5) * 80);       // R: 径向位移
        data[i + 1] = 128;                                      // G: 中性
        data[i + 2] = Math.floor(128 + (angle - 0.5) * 80);   // B: 角度位移
        data[i + 3] = 255;
      }
    }

    ctx.putImageData(imageData, 0, 0);
    return canvas.toDataURL('image/png');
  }

  /** Prominent: 强烈的中心凸起位移 */
  static _generateProminentMap(width, height) {
    const canvas = document.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext('2d');
    const imageData = ctx.createImageData(width, height);
    const data = imageData.data;

    const cx = width / 2;
    const cy = height / 2;

    for (let y = 0; y < height; y++) {
      for (let x = 0; x < width; x++) {
        const dx = (x - cx) / cx;
        const dy = (y - cy) / cy;
        const dist = Math.sqrt(dx * dx + dy * dy);

        // 中心凸起，边缘收缩
        const disp = dist < 0.6
          ? 0.3 * (1 - dist / 0.6)
          : -0.4 * ((dist - 0.6) / 0.4);

        const i = (y * width + x) * 4;
        data[i]     = Math.floor(128 + disp * 100);  // R
        data[i + 1] = 128;                            // G
        data[i + 2] = Math.floor(128 + disp * 100);  // B
        data[i + 3] = 255;
      }
    }

    ctx.putImageData(imageData, 0, 0);
    return canvas.toDataURL('image/png');
  }

  /** Shader: 基于 SDF 的精确位移贴图 */
  static _generateShaderMap(width, height) {
    const canvas = document.createElement('canvas');
    canvas.width = width;
    canvas.height = height;
    const ctx = canvas.getContext('2d');
    const imageData = ctx.createImageData(width, height);
    const data = imageData.data;

    function smoothStep(a, b, t) {
      t = Math.max(0, Math.min(1, (t - a) / (b - a)));
      return t * t * (3 - 2 * t);
    }

    function roundedRectSDF(x, y, w, h, r) {
      const qx = Math.abs(x) - w + r;
      const qy = Math.abs(y) - h + r;
      return Math.min(Math.max(qx, qy), 0) +
        Math.sqrt(Math.pow(Math.max(qx, 0), 2) + Math.pow(Math.max(qy, 0), 2)) - r;
    }

    for (let py = 0; py < height; py++) {
      for (let px = 0; px < width; px++) {
        const uvX = px / width;
        const uvY = py / height;
        const ix = uvX - 0.5;
        const iy = uvY - 0.5;

        const distanceToEdge = roundedRectSDF(ix, iy, 0.3, 0.2, 0.6);
        const displacement = smoothStep(0.8, 0, distanceToEdge - 0.15);
        const scaled = smoothStep(0, 1, displacement);

        const finalX = ix * scaled + 0.5;
        const finalY = iy * scaled + 0.5;

        const dx = finalX * width - px;
        const dy = finalY * height - py;

        const i = (py * width + px) * 4;
        data[i]     = Math.floor(128 + dx * 0.5);
        data[i + 1] = 128;
        data[i + 2] = Math.floor(128 + dy * 0.5);
        data[i + 3] = 255;
      }
    }

    ctx.putImageData(imageData, 0, 0);
    return canvas.toDataURL('image/png');
  }

  /** 根据模式获取位移贴图 */
  static _getDisplacementMap(mode, width, height) {
    switch (mode) {
      case 'polar':     return MxGlass._generatePolarMap(width, height);
      case 'prominent': return MxGlass._generateProminentMap(width, height);
      case 'shader':    return MxGlass._generateShaderMap(width, height);
      case 'standard':
      default:          return MxGlass._generateStandardMap(width, height);
    }
  }

  // ── CSS 变量读取 ──

  static _getCSSVar(name, fallback) {
    const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    return v ? parseFloat(v) : fallback;
  }

  static getDisplacement()   { return MxGlass._getCSSVar('--mx-glass-displacement', 25); }
  static getBlur()           { return MxGlass._getCSSVar('--mx-glass-blur', 12); }
  static getSaturation()     { return MxGlass._getCSSVar('--mx-glass-saturation', 180); }
  static getAberration()     { return MxGlass._getCSSVar('--mx-glass-aberration', 2); }
  static getElasticity()     { return MxGlass._getCSSVar('--mx-glass-elasticity', 0.15); }
  static getCornerRadius()   { return MxGlass._getCSSVar('--mx-glass-corner-radius', 16); }
  static getActivationZone() { return MxGlass._getCSSVar('--mx-glass-activation-zone', 200); }

  // ── 初始化 ──

  static init() {
    if (MxGlass._initialized) return;

    const mode = MxGlass._getMode();
    const dispMapUrl = MxGlass._getDisplacementMap(mode, 256, 256);

    const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
    svg.setAttribute('style', 'position:absolute;width:0;height:0');
    svg.setAttribute('aria-hidden', 'true');

    const disp = MxGlass.getDisplacement();
    const aberration = MxGlass.getAberration();

    // 根据模式决定 scale 方向（shader 模式正向，其余反向）
    const scaleDir = mode === 'shader' ? 1 : -1;
    const greenOffset = scaleDir - aberration * 0.05;
    const blueOffset = scaleDir - aberration * 0.1;
    const blurStd = Math.max(0.1, 0.5 - aberration * 0.1);

    // 边缘遮罩阈值：aberration 越大，色差区域越宽
    const edgeMaskMid = Math.max(30, 80 - aberration * 2);

    svg.innerHTML = `
      <defs>
        <filter id="mx-glass-filter" x="-35%" y="-35%" width="170%" height="170%" color-interpolation-filters="sRGB">
          <!-- 位移贴图 -->
          <feImage id="mx-glass-feimage" x="0" y="0" width="100%" height="100%"
                   result="DISPLACEMENT_MAP" href="${dispMapUrl}"
                   preserveAspectRatio="xMidYMid slice"/>

          <!-- 边缘遮罩：中心透明，边缘不透明 -->
          <radialGradient id="mx-glass-edge-mask" cx="50%" cy="50%" r="50%">
            <stop offset="0%" stop-color="black" stop-opacity="0"/>
            <stop offset="${edgeMaskMid}%" stop-color="black" stop-opacity="0"/>
            <stop offset="100%" stop-color="white" stop-opacity="1"/>
          </radialGradient>

          <!-- 从位移贴图生成边缘强度 -->
          <feColorMatrix in="DISPLACEMENT_MAP" type="matrix"
            values="0.3 0.3 0.3 0 0
                    0.3 0.3 0.3 0 0
                    0.3 0.3 0.3 0 0
                    0 0 0 1 0"
            result="EDGE_INTENSITY"/>
          <feComponentTransfer in="EDGE_INTENSITY" result="EDGE_MASK">
            <feFuncA type="discrete" tableValues="0 ${aberration * 0.05} 1"/>
          </feComponentTransfer>

          <!-- 中心原始图像（无位移） -->
          <feOffset in="SourceGraphic" dx="0" dy="0" result="CENTER_ORIGINAL"/>

          <!-- Red 通道位移 -->
          <feDisplacementMap in="SourceGraphic" in2="DISPLACEMENT_MAP"
            scale="${disp * scaleDir}" xChannelSelector="R" yChannelSelector="B"
            result="RED_DISPLACED"/>
          <feColorMatrix in="RED_DISPLACED" type="matrix"
            values="1 0 0 0 0  0 0 0 0 0  0 0 0 0 0  0 0 0 1 0"
            result="RED_CHANNEL"/>

          <!-- Green 通道位移（偏移量不同） -->
          <feDisplacementMap in="SourceGraphic" in2="DISPLACEMENT_MAP"
            scale="${disp * greenOffset}" xChannelSelector="R" yChannelSelector="B"
            result="GREEN_DISPLACED"/>
          <feColorMatrix in="GREEN_DISPLACED" type="matrix"
            values="0 0 0 0 0  0 1 0 0 0  0 0 0 0 0  0 0 0 1 0"
            result="GREEN_CHANNEL"/>

          <!-- Blue 通道位移（偏移量最大） -->
          <feDisplacementMap in="SourceGraphic" in2="DISPLACEMENT_MAP"
            scale="${disp * blueOffset}" xChannelSelector="R" yChannelSelector="B"
            result="BLUE_DISPLACED"/>
          <feColorMatrix in="BLUE_DISPLACED" type="matrix"
            values="0 0 0 0 0  0 0 0 0 0  0 0 1 0 0  0 0 0 1 0"
            result="BLUE_CHANNEL"/>

          <!-- 合并 RGB 三通道（screen 混合） -->
          <feBlend in="GREEN_CHANNEL" in2="BLUE_CHANNEL" mode="screen" result="GB_COMBINED"/>
          <feBlend in="RED_CHANNEL" in2="GB_COMBINED" mode="screen" result="RGB_COMBINED"/>

          <!-- 柔化色差 -->
          <feGaussianBlur in="RGB_COMBINED" stdDeviation="${blurStd}" result="ABERRATED_BLURRED"/>

          <!-- 边缘色差遮罩：仅边缘有色差 -->
          <feComposite in="ABERRATED_BLURRED" in2="EDGE_MASK" operator="in" result="EDGE_ABERRATION"/>

          <!-- 反转遮罩：中心区域 -->
          <feComponentTransfer in="EDGE_MASK" result="INVERTED_MASK">
            <feFuncA type="table" tableValues="1 0"/>
          </feComponentTransfer>
          <feComposite in="CENTER_ORIGINAL" in2="INVERTED_MASK" operator="in" result="CENTER_CLEAN"/>

          <!-- 合并：边缘色差 + 清晰中心 -->
          <feComposite in="EDGE_ABERRATION" in2="CENTER_CLEAN" operator="over"/>
        </filter>
      </defs>
    `;

    document.body.appendChild(svg);
    MxGlass._svgFilter = svg;
    MxGlass._initialized = true;

    // 启动鼠标跟踪
    MxGlass._startMouseTracking();
  }

  // ── 鼠标跟踪 ──

  static _startMouseTracking() {
    if (MxGlass._mouseTrackingActive) return;
    MxGlass._mouseTrackingActive = true;

    document.addEventListener('mousemove', (e) => {
      MxGlass._mousePos = { x: e.clientX, y: e.clientY };
      MxGlass._updateTrackedElements();
    }, { passive: true });
  }

  static _updateTrackedElements() {
    if (MxGlass._animationFrame) cancelAnimationFrame(MxGlass._animationFrame);
    MxGlass._animationFrame = requestAnimationFrame(() => {
      MxGlass._trackedElements.forEach(el => {
        MxGlass._applyMouseEffect(el);
      });
    });
  }

  static _applyMouseEffect(el) {
    const rect = el.getBoundingClientRect();
    const centerX = rect.left + rect.width / 2;
    const centerY = rect.top + rect.height / 2;
    const elasticity = MxGlass.getElasticity();
    const activationZone = MxGlass.getActivationZone();

    // 鼠标偏移（百分比）
    const offsetX = ((MxGlass._mousePos.x - centerX) / rect.width) * 100;
    const offsetY = ((MxGlass._mousePos.y - centerY) / rect.height) * 100;

    // 边缘距离
    const edgeDistX = Math.max(0, Math.abs(MxGlass._mousePos.x - centerX) - rect.width / 2);
    const edgeDistY = Math.max(0, Math.abs(MxGlass._mousePos.y - centerY) - rect.height / 2);
    const edgeDist = Math.sqrt(edgeDistX * edgeDistX + edgeDistY * edgeDistY);

    // 淡入因子
    const fadeIn = edgeDist > activationZone ? 0 : 1 - edgeDist / activationZone;

    // 弹性位移
    const tx = (MxGlass._mousePos.x - centerX) * elasticity * 0.1 * fadeIn;
    const ty = (MxGlass._mousePos.y - centerY) * elasticity * 0.1 * fadeIn;

    // 方向性缩放
    const centerDist = Math.sqrt(
      Math.pow(MxGlass._mousePos.x - centerX, 2) +
      Math.pow(MxGlass._mousePos.y - centerY, 2)
    );
    const normX = centerDist > 0 ? (MxGlass._mousePos.x - centerX) / centerDist : 0;
    const normY = centerDist > 0 ? (MxGlass._mousePos.y - centerY) / centerDist : 0;
    const stretchIntensity = Math.min(centerDist / 300, 1) * elasticity * fadeIn;

    const scaleX = 1 + Math.abs(normX) * stretchIntensity * 0.3 - Math.abs(normY) * stretchIntensity * 0.15;
    const scaleY = 1 + Math.abs(normY) * stretchIntensity * 0.3 - Math.abs(normX) * stretchIntensity * 0.15;

    el.style.transform = `translate(${tx}px, ${ty}px) scaleX(${Math.max(0.8, scaleX).toFixed(4)}) scaleY(${Math.max(0.8, scaleY).toFixed(4)})`;

    // 更新边框渐变角度
    const gradientAngle = 135 + offsetX * 1.2;
    const borderSpots = [
      Math.max(10, 33 + offsetY * 0.3),
      Math.min(90, 66 + offsetY * 0.4)
    ];

    // 更新双层边框
    const borderLayers = el.querySelectorAll('.mx-glass-border-layer');
    borderLayers.forEach((layer, i) => {
      const intensity1 = 0.12 + Math.abs(offsetX) * 0.008;
      const intensity2 = 0.4 + Math.abs(offsetX) * 0.012;
      const intensity1b = 0.32 + Math.abs(offsetX) * 0.008;
      const intensity2b = 0.6 + Math.abs(offsetX) * 0.012;

      if (i === 0) {
        layer.style.background = `linear-gradient(${gradientAngle}deg, rgba(255,255,255,0) 0%, rgba(255,255,255,${intensity1}) ${borderSpots[0]}%, rgba(255,255,255,${intensity2}) ${borderSpots[1]}%, rgba(255,255,255,0) 100%)`;
      } else if (i === 1) {
        layer.style.background = `linear-gradient(${gradientAngle}deg, rgba(255,255,255,0) 0%, rgba(255,255,255,${intensity1b}) ${borderSpots[0]}%, rgba(255,255,255,${intensity2b}) ${borderSpots[1]}%, rgba(255,255,255,0) 100%)`;
      }
    });

    // 更新 CSS 变量供其他元素使用
    el.style.setProperty('--mx-glass-mouse-x', offsetX.toFixed(2));
    el.style.setProperty('--mx-glass-mouse-y', offsetY.toFixed(2));
  }

  // ── 元素注册 ──

  static track(el) {
    if (!MxGlass._initialized) MxGlass.init();
    MxGlass._trackedElements.add(el);
    MxGlass._createBorderLayers(el);
    MxGlass._applyMouseEffect(el);
  }

  static untrack(el) {
    MxGlass._trackedElements.delete(el);
  }

  static _createBorderLayers(el) {
    if (el.querySelector('.mx-glass-border-layer')) return;

    for (let i = 0; i < 2; i++) {
      const layer = document.createElement('span');
      layer.className = 'mx-glass-border-layer';
      layer.setAttribute('aria-hidden', 'true');
      layer.style.cssText = `
        position: absolute; inset: 0; pointer-events: none;
        border-radius: inherit; padding: 1.5px;
        -webkit-mask: linear-gradient(#000 0 0) content-box, linear-gradient(#000 0 0);
        -webkit-mask-composite: xor;
        mask: linear-gradient(#000 0 0) content-box, linear-gradient(#000 0 0);
        mask-composite: exclude;
        box-shadow: 0 0 0 0.5px rgba(255,255,255,0.5) inset, 0 1px 3px rgba(255,255,255,0.25) inset, 0 1px 4px rgba(0,0,0,0.35);
        mix-blend-mode: ${i === 0 ? 'screen' : 'overlay'};
        opacity: ${i === 0 ? 0.2 : 1};
        z-index: 2;
      `;
      el.appendChild(layer);
    }
  }

  // ── 模式管理 ──

  static _getMode() {
    return document.documentElement.dataset.glassMode || 'standard';
  }

  static setMode(mode) {
    const valid = ['standard', 'polar', 'prominent', 'shader'];
    if (!valid.includes(mode)) {
      console.warn(`MxGlass: invalid mode "${mode}". Valid: ${valid.join(', ')}`);
      return;
    }
    document.documentElement.dataset.glassMode = mode;
    // 重新生成 filter
    MxGlass._rebuildFilter();
  }

  static getMode() {
    return MxGlass._getMode();
  }

  // ── 参数设置 ──

  static setDisplacement(value) {
    document.documentElement.style.setProperty('--mx-glass-displacement', value);
    MxGlass._rebuildFilter();
  }

  static setBlur(value) {
    document.documentElement.style.setProperty('--mx-glass-blur', value + 'px');
  }

  static setSaturation(value) {
    document.documentElement.style.setProperty('--mx-glass-saturation', value + '%');
  }

  static setAberration(value) {
    document.documentElement.style.setProperty('--mx-glass-aberration', value);
    MxGlass._rebuildFilter();
  }

  static setElasticity(value) {
    document.documentElement.style.setProperty('--mx-glass-elasticity', value);
  }

  static setCornerRadius(value) {
    document.documentElement.style.setProperty('--mx-glass-corner-radius', value + 'px');
  }

  static setActivationZone(value) {
    document.documentElement.style.setProperty('--mx-glass-activation-zone', value);
  }

  /** 批量设置 */
  static configure(options = {}) {
    if (options.mode !== undefined)        MxGlass.setMode(options.mode);
    if (options.displacement !== undefined) MxGlass.setDisplacement(options.displacement);
    if (options.blur !== undefined)         MxGlass.setBlur(options.blur);
    if (options.saturation !== undefined)   MxGlass.setSaturation(options.saturation);
    if (options.aberration !== undefined)   MxGlass.setAberration(options.aberration);
    if (options.elasticity !== undefined)   MxGlass.setElasticity(options.elasticity);
    if (options.cornerRadius !== undefined) MxGlass.setCornerRadius(options.cornerRadius);
    if (options.activationZone !== undefined) MxGlass.setActivationZone(options.activationZone);
    if (options.overLight !== undefined)    MxGlass.setOverLight(options.overLight);
  }

  // ── Over-light 模式 ──

  static setOverLight(enabled) {
    if (enabled) {
      document.documentElement.dataset.glassOverLight = 'true';
    } else {
      delete document.documentElement.dataset.glassOverLight;
    }
  }

  // ── 重建 SVG Filter ──

  static _rebuildFilter() {
    if (!MxGlass._initialized) return;

    const mode = MxGlass._getMode();
    const dispMapUrl = MxGlass._getDisplacementMap(mode, 256, 256);
    const disp = MxGlass.getDisplacement();
    const aberration = MxGlass.getAberration();
    const scaleDir = mode === 'shader' ? 1 : -1;
    const greenOffset = scaleDir - aberration * 0.05;
    const blueOffset = scaleDir - aberration * 0.1;
    const blurStd = Math.max(0.1, 0.5 - aberration * 0.1);
    const edgeMaskMid = Math.max(30, 80 - aberration * 2);

    const feImage = MxGlass._svgFilter.querySelector('#mx-glass-feimage');
    if (feImage) feImage.setAttribute('href', dispMapUrl);

    const filter = MxGlass._svgFilter.querySelector('#mx-glass-filter');
    if (filter) {
      filter.innerHTML = `
        <feImage id="mx-glass-feimage" x="0" y="0" width="100%" height="100%"
                 result="DISPLACEMENT_MAP" href="${dispMapUrl}"
                 preserveAspectRatio="xMidYMid slice"/>
        <radialGradient id="mx-glass-edge-mask" cx="50%" cy="50%" r="50%">
          <stop offset="0%" stop-color="black" stop-opacity="0"/>
          <stop offset="${edgeMaskMid}%" stop-color="black" stop-opacity="0"/>
          <stop offset="100%" stop-color="white" stop-opacity="1"/>
        </radialGradient>
        <feColorMatrix in="DISPLACEMENT_MAP" type="matrix"
          values="0.3 0.3 0.3 0 0  0.3 0.3 0.3 0 0  0.3 0.3 0.3 0 0  0 0 0 1 0"
          result="EDGE_INTENSITY"/>
        <feComponentTransfer in="EDGE_INTENSITY" result="EDGE_MASK">
          <feFuncA type="discrete" tableValues="0 ${aberration * 0.05} 1"/>
        </feComponentTransfer>
        <feOffset in="SourceGraphic" dx="0" dy="0" result="CENTER_ORIGINAL"/>
        <feDisplacementMap in="SourceGraphic" in2="DISPLACEMENT_MAP"
          scale="${disp * scaleDir}" xChannelSelector="R" yChannelSelector="B"
          result="RED_DISPLACED"/>
        <feColorMatrix in="RED_DISPLACED" type="matrix"
          values="1 0 0 0 0  0 0 0 0 0  0 0 0 0 0  0 0 0 1 0" result="RED_CHANNEL"/>
        <feDisplacementMap in="SourceGraphic" in2="DISPLACEMENT_MAP"
          scale="${disp * greenOffset}" xChannelSelector="R" yChannelSelector="B"
          result="GREEN_DISPLACED"/>
        <feColorMatrix in="GREEN_DISPLACED" type="matrix"
          values="0 0 0 0 0  0 1 0 0 0  0 0 0 0 0  0 0 0 1 0" result="GREEN_CHANNEL"/>
        <feDisplacementMap in="SourceGraphic" in2="DISPLACEMENT_MAP"
          scale="${disp * blueOffset}" xChannelSelector="R" yChannelSelector="B"
          result="BLUE_DISPLACED"/>
        <feColorMatrix in="BLUE_DISPLACED" type="matrix"
          values="0 0 0 0 0  0 0 0 0 0  0 0 1 0 0  0 0 0 1 0" result="BLUE_CHANNEL"/>
        <feBlend in="GREEN_CHANNEL" in2="BLUE_CHANNEL" mode="screen" result="GB_COMBINED"/>
        <feBlend in="RED_CHANNEL" in2="GB_COMBINED" mode="screen" result="RGB_COMBINED"/>
        <feGaussianBlur in="RGB_COMBINED" stdDeviation="${blurStd}" result="ABERRATED_BLURRED"/>
        <feComposite in="ABERRATED_BLURRED" in2="EDGE_MASK" operator="in" result="EDGE_ABERRATION"/>
        <feComponentTransfer in="EDGE_MASK" result="INVERTED_MASK">
          <feFuncA type="table" tableValues="1 0"/>
        </feComponentTransfer>
        <feComposite in="CENTER_ORIGINAL" in2="INVERTED_MASK" operator="in" result="CENTER_CLEAN"/>
        <feComposite in="EDGE_ABERRATION" in2="CENTER_CLEAN" operator="over"/>
      `;
    }
  }

  // ── 启用 / 禁用 ──

  static enable() {
    document.documentElement.dataset.glass = 'liquid';
    if (!MxGlass._initialized) MxGlass.init();
  }

  static disable() {
    delete document.documentElement.dataset.glass;
  }

  static toggle() {
    if (document.documentElement.dataset.glass === 'liquid') {
      MxGlass.disable();
    } else {
      MxGlass.enable();
    }
  }

  static isEnabled() {
    return document.documentElement.dataset.glass === 'liquid';
  }

  // ── 便捷 API ──

  /** 获取当前所有配置（快照） */
  static getSnapshot() {
    return {
      enabled: MxGlass.isEnabled(),
      mode: MxGlass.getMode(),
      displacement: MxGlass.getDisplacement(),
      blur: MxGlass.getBlur(),
      saturation: MxGlass.getSaturation(),
      aberration: MxGlass.getAberration(),
      elasticity: MxGlass.getElasticity(),
      cornerRadius: MxGlass.getCornerRadius(),
      activationZone: MxGlass.getActivationZone(),
      overLight: document.documentElement.dataset.glassOverLight === 'true',
    };
  }

  /** 重置为默认值 */
  static reset() {
    MxGlass.configure({
      mode: 'standard',
      displacement: 25,
      blur: 12,
      saturation: 180,
      aberration: 2,
      elasticity: 0.15,
      cornerRadius: 16,
      activationZone: 200,
      overLight: false,
    });
  }
}

if (typeof module !== 'undefined') module.exports = MxGlass;
