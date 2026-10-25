# Miuix Console

> 基于 [Miuix](https://github.com/compose-miuix-ui/miuix) 设计语言的 Web 控制台 UI 框架。
> 将 Compose Multiplatform 的 Miuix 组件库的设计 DNA 移植到纯 CSS + Vanilla JS，零依赖。

[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)
[![CSS](https://img.shields.io/badge/CSS-pure-1572B6.svg)]()
[![JS](https://img.shields.io/badge/JS-Vanilla-F7DF1E.svg)]()

### 🌐 在线体验

| 页面 | 链接 | 说明 |
|------|------|------|
| **组件 Demo** | [bmbxwbh.github.io/miuix-console/demo/index.html](https://bmbxwbh.github.io/miuix-console/demo/index.html) | 全部 28 个组件 + 完整控制台界面 |
| **Liquid Glass** | [bmbxwbh.github.io/miuix-console/demo/glass.html](https://bmbxwbh.github.io/miuix-console/demo/glass.html) | 液体玻璃效果 Playground + 参数调节面板 |
| **项目主页** | [bmbxwbh.github.io/miuix-console](https://bmbxwbh.github.io/miuix-console/) | 落地页 + 设计理念展示 |

---

## 目录

- [在线体验](#-在线体验)
- [设计理念](#设计理念)
- [快速开始](#快速开始)
- [⚠️ App Shell 布局警告](#️-app-shell-布局警告)
- [项目结构](#项目结构)
- [设计令牌](#设计令牌)
- [组件总览](#组件总览)
- [液体玻璃系统](#液体玻璃系统)
- [响应式系统](#响应式系统)
- [动效系统](#动效系统)
- [主题系统](#主题系统)
- [工具库](#工具库)
- [浏览器兼容](#浏览器兼容)

---

## 设计理念

Miuix Console 不是另一个 UI 框架。它是 [Miuix](https://github.com/compose-miuix-ui/miuix)（小米 HyperOS 的 Compose Multiplatform 组件库）在 Web 端的忠实移植。

### 核心原则

| 原则 | 说明 |
|------|------|
| **Smooth Corner** | 不用普通 `border-radius`，而是连续曲率的贝塞尔曲线过渡（系数 k=0.55），这是 Miuix 最具辨识度的视觉特征 |
| **多层 Surface** | 5 级 surface 色阶，逐层递进，形成自然的层级关系 |
| **柔和阴影** | 所有阴影都是多层半透明叠加，不用硬投影 |
| **弹簧动效** | `cubic-bezier(0.34, 1.56, 0.64, 1)` 弹簧曲线，仅用于 Switch/BottomNav/Sidebar 三个物理反馈场景 |
| **克制动效** | 其余交互统一使用 `cubic-bezier(0.16, 1, 0.3, 1)` 减速缓动，不滥用弹性 |
| **深色优先** | Dark mode 是默认主题，Light mode 做等价适配 |
| **毛玻璃** | 顶栏和底部导航使用 `backdrop-filter: blur(24px)` |
| **干净克制** | 不追求炫技，追求安静的高级感。动效服务于交互反馈，不服务于表演 |

### 设计语言对齐

```
Miuix (Compose Multiplatform)     Miuix Console (Web)
─────────────────────────────     ──────────────────────
Card.kt                    →      components/card.css
Button.kt                  →      components/button.css
Switch.kt                  →      components/switch.css
TextField.kt               →      components/input.css
TopAppBar.kt               →      components/navigation.css
Scaffold.kt                →      components/layout.css
SmoothRoundedCornerShape   →      utils/smooth-corner.js
ThemeController            →      utils/theme.js
Colors.kt                  →      core/tokens.css
```

### 色彩系统

从 Miuix `Colors.kt` 源码 1:1 移植：

| 用途 | Light | Dark |
|------|-------|------|
| Primary | `#3482FF` | `#277AF7` |
| Surface | `#F7F7F7` | `#0C0C0C` |
| Background | `#FFFFFF` | `#161616` |
| Text | `#000000` | `#F2F2F2` |

---

## 快速开始

### 一键引入

```html
<!DOCTYPE html>
<html lang="zh-CN" data-theme="dark">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <link href="https://cdn.jsdelivr.net/npm/misans@4.1/lib/Normal/MiSans-Regular.min.css" rel="stylesheet">
  <link href="https://cdn.jsdelivr.net/npm/misans@4.1/lib/Normal/MiSans-Medium.min.css" rel="stylesheet">
  <link href="https://cdn.jsdelivr.net/npm/misans@4.1/lib/Normal/MiSans-Semibold.min.css" rel="stylesheet">
  <link href="https://cdn.jsdelivr.net/npm/misans@4.1/lib/Normal/MiSans-Bold.min.css" rel="stylesheet">
</head>
<body>
  <div class="mx-card">
    <div class="mx-card-body">
      <button class="mx-btn mx-btn-primary" data-ripple>Hello Miuix</button>
    </div>
  </div>
  <script src="miuix-console/miuix-console.js"></script>
</body>
</html>
```

### 按需引入

```html
<!-- Core (必须) -->
<link rel="stylesheet" href="miuix-console/core/tokens.css">
<link rel="stylesheet" href="miuix-console/core/base.css">
<link rel="stylesheet" href="miuix-console/core/animations.css">

<!-- Components (按需) -->
<link rel="stylesheet" href="miuix-console/components/card.css">
<link rel="stylesheet" href="miuix-console/components/button.css">
<!-- ... -->

<!-- Utils (按需) -->
<script src="miuix-console/utils/smooth-corner.js"></script>
<script src="miuix-console/utils/ripple.js"></script>
<!-- ... -->
```

---

## ⚠️ App Shell 布局警告

使用 App Shell 布局时，**最常见的致命错误**是将 `<main>` 嵌套在 `.mx-desktop-shell` 内部，导致**主内容区域完全空白**。

```html
<!-- ✅ 正确：<main> 是 .mx-app 的直接子元素 -->
<div class="mx-app">
  <header class="mx-topbar">...</header>
  <aside class="mx-sidebar">...</aside>
  <div class="mx-desktop-shell">
    <header class="mx-desktop-topbar">...</header>
  </div>
  <main class="mx-desktop-main mx-body">...</main>  <!-- ✅ 在这里 -->
  <nav class="mx-bottom-nav">...</nav>
</div>

<!-- ❌ 错误：<main> 嵌套在 .mx-desktop-shell 内，页面空白！ -->
<div class="mx-app">
  <div class="mx-desktop-shell">
    <header class="mx-desktop-topbar">...</header>
    <main>...</main>  <!-- ❌ 内容不可见！ -->
  </div>
</div>
```

`.mx-desktop-shell` **只允许包含** `.mx-desktop-topbar`，不能放 `<main>` 或其他内容。

---

## 项目结构

```
miuix-console/
├── core/                        # 核心设计系统
│   ├── tokens.css               # 设计令牌：色彩、圆角、阴影、动效、字体
│   ├── base.css                 # 全局重置、滚动条、选区、暗色内发光、reduced-motion
│   ├── animations.css           # @keyframe + 动画工具类 + stagger 延迟 + 骨架屏
│   └── glass.css                # 液体玻璃效果系统（可选）
│
├── components/                  # 28 个 UI 组件，每个独立 CSS 文件
│   ├── card.css                 # 卡片容器
│   ├── button.css               # 按钮（4 变体 × 3 尺寸 + pill/icon）
│   ├── switch.css               # 开关（弹簧回弹）
│   ├── badge.css                # 徽章（5 语义色 + pulse 动画）
│   ├── tag.css                  # 标签 / Chip（多色/outline/可关闭/输入标签）
│   ├── input.css                # 输入框 / 文本域（iOS 防缩放）
│   ├── select.css               # 下拉选择器（单选/多选/搜索过滤）
│   ├── checkbox.css             # 复选框 & 单选框
│   ├── slider.css               # 滑块 / 范围选择
│   ├── datepicker.css           # 日期选择器
│   ├── upload.css               # 文件上传（拖拽/文件列表/进度）
│   ├── table.css                # 数据表格（手机端卡片式）
│   ├── stat.css                 # 统计卡片
│   ├── tabs.css                 # 选项卡（下划线/胶囊/卡片变体）
│   ├── pagination.css           # 分页器
│   ├── accordion.css            # 折叠面板（默认/卡片/无边框）
│   ├── tree.css                 # 树形控件
│   ├── navigation.css           # 顶栏 / 侧边栏 / 底部导航 / 面包屑 / 胶囊组
│   ├── activity.css             # 活动列表 / 柱状图
│   ├── layout.css               # App Shell / 网格 / flex / 全设备响应式
│   ├── modal.css                # 弹窗 / 对话框 / 确认框
│   ├── drawer.css               # 抽屉 / 侧边面板
│   ├── toast.css                # 轻提示 / Snackbar
│   ├── tooltip.css              # 文字提示（4 方向）
│   ├── popover.css              # 弹出层 / 下拉菜单
│   ├── progress.css             # 进度条 / 加载指示器 / 环形进度
│   ├── empty.css                # 空状态占位
│   └── avatar.css               # 头像 / 头像组 / 在线状态
│
├── utils/                       # 13 个 JS 工具
│   ├── smooth-corner.js         # Miuix SmoothRoundedCornerShape → CSS clip-path
│   ├── ripple.js                # 点击涟漪（事件委托）
│   ├── counter.js               # 数字滚动动画（ease-out cubic）
│   ├── theme.js                 # 主题切换 + 密度管理 + localStorage 持久化
│   ├── chart.js                 # 柱状图渲染器
│   ├── modal.js                 # 弹窗管理 + confirm() Promise API
│   ├── toast.js                 # Toast 管理（自动消失/进度条/操作按钮）
│   ├── select.js                # 下拉选择（单选/多选/搜索过滤）
│   ├── tabs.js                  # 选项卡管理
│   ├── pagination.js            # 分页器渲染
│   ├── accordion.js             # 折叠面板管理
│   ├── datepicker.js            # 日期选择器管理
│   ├── upload.js                # 文件上传（拖拽/过滤/文件列表）
│   └── glass.js                 # 液体玻璃管理器
│
├── demo/                        # 演示页面
│   ├── index.html               # 完整 demo（含所有组件展示）
│   └── glass.html               # Liquid Glass Playground（参数调节 + 模式对比）
│
├── miuix-console.js             # 一键入口（自动加载所有 CSS + JS + 初始化）
└── README.md
```

---

## 设计令牌

所有视觉参数集中在 `core/tokens.css`，通过 CSS 自定义属性全局生效。

### 色彩

```css
--mx-primary: #3482FF;           /* Light */
--mx-primary: #277AF7;           /* Dark */
--mx-green: #34D399;             /* 成功 */
--mx-amber: #FBBF24;             /* 警告 */
--mx-red: #E94634;               /* 错误 (Light) */
--mx-red: #F12522;               /* 错误 (Dark) */
--mx-blue: #60A5FA;              /* 信息 */
```

### Surface 色阶（5 级）

```css
/* Light */
--mx-bg: #FFFFFF;                /* 页面背景 */
--mx-s1: #F7F7F7;                /* 导航栏、侧边栏 */
--mx-s2: #FFFFFF;                /* 卡片 */
--mx-s3: #E8E8E8;                /* 输入框、浮层 */
--mx-s4: #E8E8E8;                /* 下拉菜单 */

/* Dark */
--mx-bg: #161616;
--mx-s1: #0C0C0C;
--mx-s2: #1C1C1C;
--mx-s3: #242424;
--mx-s4: #2C2C2C;
```

### 动效

```css
--mx-ease: cubic-bezier(0.16, 1, 0.3, 1);       /* 标准缓动 */
--mx-spring: cubic-bezier(0.34, 1.56, 0.64, 1);  /* 弹簧（仅物理反馈） */
--mx-d1: 100ms;   /* 即时反馈（按下） */
--mx-d2: 200ms;   /* 快速过渡（hover in） */
--mx-d3: 320ms;   /* 从容出场（hover out / 弹窗） */
--mx-d4: 500ms;   /* 慢速编排（图表） */
```

### 圆角

```css
--mx-r: 16px;                    /* 标准 */
--mx-r-sm: 12px;                 /* 按钮、输入框 */
--mx-r-xs: 8px;                  /* 极小 */
--mx-r-pill: 9999px;             /* 胶囊 */
```

### 字号层级

```css
--mx-text-xs: 11px;
--mx-text-sm: 12px;
--mx-text-base: 13px;
--mx-text-md: 14px;
--mx-text-lg: 15px;
--mx-text-xl: 16px;
--mx-text-2xl: 20px;
--mx-text-3xl: 24px;
```

---

## 组件总览

### 基础

| 组件 | CSS | JS | 说明 |
|------|-----|-----|------|
| Card | `card.css` | — | 卡片容器、clickable、accent 变体 |
| Button | `button.css` | — | 4 变体 × 3 尺寸 + pill + icon |
| Switch | `switch.css` | — | 弹簧回弹动效 |
| Badge | `badge.css` | — | 5 语义色 + pulse + count |
| Tag | `tag.css` | — | 多色/outline/dot/可关闭/输入标签 |

### 表单

| 组件 | CSS | JS | 说明 |
|------|-----|-----|------|
| Input | `input.css` | — | 输入框 + 文本域 + 图标 + 错误态 |
| Select | `select.css` | `select.js` | 单选/多选/搜索过滤/分组 |
| Checkbox | `checkbox.css` | — | 复选框 + 单选框 |
| Slider | `slider.css` | — | 滑块 + 范围选择 |
| Date Picker | `datepicker.css` | `datepicker.js` | 月历 + 选择 + 范围 |
| Upload | `upload.css` | `upload.js` | 拖拽上传 + 文件列表 + 进度 |

### 数据展示

| 组件 | CSS | JS | 说明 |
|------|-----|-----|------|
| Table | `table.css` | — | 手机端卡片式（需 `data-label`） |
| Stat | `stat.css` | — | 统计卡 + accent + 计数器 |
| Tabs | `tabs.css` | `tabs.js` | 下划线/胶囊/卡片 3 种变体 |
| Pagination | `pagination.css` | `pagination.js` | 完整/简洁模式 |
| Accordion | `accordion.css` | `accordion.js` | 默认/卡片/无边框 |
| Tree | `tree.css` | — | 展开/收起 + 叶子节点 |
| Activity | `activity.css` | — | 活动列表 + 时间线 |
| Chart | `activity.css` | `chart.js` | 柱状图渲染器 |
| Empty | `empty.css` | — | 空状态占位 |

### 导航 & 布局

| 组件 | CSS | JS | 说明 |
|------|-----|-----|------|
| TopBar | `navigation.css` | — | 毛玻璃顶栏 |
| Sidebar | `navigation.css` | — | 侧边栏 + active 指示条 |
| BottomNav | `navigation.css` | — | 移动端底部导航 |
| Breadcrumb | `navigation.css` | — | 面包屑 |
| Pill Group | `navigation.css` | — | 胶囊切换组 |
| Avatar | `avatar.css` | — | 5 尺寸 + 头像组 + 在线状态 |
| Layout | `layout.css` | — | App Shell / 网格 / flex |

### 反馈

| 组件 | CSS | JS | 说明 |
|------|-----|-----|------|
| Modal | `modal.css` | `modal.js` | 弹窗 + confirm() Promise API |
| Drawer | `drawer.css` | — | 左右抽屉 + 3 尺寸 |
| Toast | `toast.css` | `toast.js` | 自动消失 + 进度条 + 操作按钮 |
| Tooltip | `tooltip.css` | — | 4 方向 CSS-only |
| Popover | `popover.css` | — | 弹出层 + item/divider |
| Progress | `progress.css` | — | 线性/条纹/不确定/旋转/环形 |

---

## 液体玻璃系统

可选的视觉增强层，通过 `data-glass="liquid"` 属性启用，**不修改任何原有样式**。

基于 [rdev/liquid-glass-react](https://github.com/rdev/liquid-glass-react)（⭐4800+）的原理移植到 Vanilla JS。

### 快速启用

```html
<html data-theme="dark" data-glass="liquid">
```

```js
MxGlass.enable();
MxGlass.disable();
MxGlass.toggle();
```

### 4 种折射模式

| 模式 | 说明 | 场景 |
|------|------|------|
| `standard` | 标准径向渐变位移 | 通用场景（默认） |
| `polar` | 极坐标位移，中心旋转扭曲 | 强调中心内容 |
| `prominent` | 中心凸起 + 边缘收缩 | 立体感最强 |
| `shader` | SDF 精确计算位移 | Apple 原版效果 |

```js
MxGlass.setMode('shader');
```

### 核心特性

| 特性 | 说明 |
|------|------|
| **折射位移** | SVG `feDisplacementMap` 真实折射，4 种位移贴图模式 |
| **色差色散** | RGB 三通道独立位移，仅边缘产生色差，中心保持清晰 |
| **鼠标跟踪** | 玻璃元素跟随鼠标弹性移动 + 方向性缩放 |
| **动态边框** | 双层渐变边框，跟随鼠标角度变化（screen + overlay 混合） |
| **Over-Light** | 亮色背景自适应，自动切换深色玻璃 |
| **Hover/Active** | 额外的径向渐变高光层 |

### 完整 API

```js
// 启用 / 禁用
MxGlass.enable()
MxGlass.disable()
MxGlass.toggle()
MxGlass.isEnabled()         // → boolean

// 模式切换
MxGlass.setMode('standard') // standard | polar | prominent | shader
MxGlass.getMode()           // → string

// 单项设置
MxGlass.setDisplacement(25)  // 位移量 0-100
MxGlass.setBlur(12)          // 模糊 px
MxGlass.setSaturation(180)   // 饱和度 %
MxGlass.setAberration(2)     // 色差强度 0-10
MxGlass.setElasticity(0.15)  // 弹性 0-0.5
MxGlass.setCornerRadius(16)  // 圆角 px
MxGlass.setActivationZone(200) // 鼠标激活区域 px
MxGlass.setOverLight(true)   // 亮色背景模式

// 批量配置
MxGlass.configure({
  mode: 'shader',
  displacement: 50,
  blur: 16,
  aberration: 3,
  elasticity: 0.2,
})

// 元素级鼠标跟踪
MxGlass.track(element)    // 注册跟踪
MxGlass.untrack(element)  // 取消跟踪

// 快照 / 重置
MxGlass.getSnapshot()     // → { enabled, mode, displacement, ... }
MxGlass.reset()           // 恢复所有默认值
```

### CSS 变量

```css
--mx-glass-blur: 12px;
--mx-glass-saturation: 180%;
--mx-glass-displacement: 25;
--mx-glass-aberration: 2;
--mx-glass-elasticity: 0.15;
--mx-glass-corner-radius: 16px;
--mx-glass-activation-zone: 200;
--mx-glass-bg-dark: rgba(0, 0, 0, 0.35);
--mx-glass-bg-light: rgba(255, 255, 255, 0.25);
--mx-glass-border-dark: rgba(255, 255, 255, 0.08);
--mx-glass-border-light: rgba(255, 255, 255, 0.2);
```

### 覆盖组件

| 组件 | 说明 |
|------|------|
| TopBar | 透明 + blur + 折射 |
| Sidebar | 同上 |
| BottomNav | 同上 |
| Modal | 弹窗本体玻璃化 |
| Drawer | 同上 |
| Popover | 同上 |
| Select Dropdown | 同上 |
| Card (opt-in) | 需添加 `.mx-card-glass` 类 |

### 在线体验

👉 [Liquid Glass Playground](demo/glass.html) — 完整的参数调节面板 + 4 种模式对比 + 鼠标跟踪演示

---

## 响应式系统

三层断点 + 触控检测 + 横屏处理。

| 断点 | 设备 | 适配 |
|------|------|------|
| `< 768px` | 手机 | 触控优先、全屏弹窗、卡片式表格、单列网格 |
| `768-1023px` | 平板 | 居中弹窗、可滚动 tabs |
| `≥ 1024px` | 桌面 | 侧边栏布局 |
| `≥ 1440px` | 大屏 | 加宽侧边栏 |
| `hover:none` | 纯触控 | 禁用 hover、加大触控区 ≥ 44px |
| `max-height:500px` | 横屏 | 隐藏底部导航 |

### 触控适配

手机端自动加大可交互区域：

- Button: 40px、Input: 44px、Switch: 48×28
- Checkbox/Radio: 22px、Slider thumb: 24px
- Nav item / Accordion header: min-height 44px
- Popover / Select / DatePicker: 底部弹出 sheet
- Table: 卡片式（需 `data-label` 属性配合）

---

## 动效系统

### 曲线分配

| 曲线 | 用途 | 场景 |
|------|------|------|
| `spring` | 弹簧回弹 | Switch thumb、BottomNav 圆点、Sidebar 指示条 |
| `ease` | 减速缓动 | 按钮、卡片、输入框、accordion、pill 切换 |

### 时间节奏

| 变量 | 时长 | 场景 |
|------|------|------|
| `--mx-d1` | 100ms | 按下反馈 |
| `--mx-d2` | 200ms | hover in |
| `--mx-d3` | 320ms | hover out / 弹窗 |
| `--mx-d4` | 500ms | 图表生长 |

### 进快出慢

Card / Stat / Table 的 hover 效果使用不对称时间：进入 200ms，离开 320ms，让消散更从容。

### 无障碍

`prefers-reduced-motion: reduce` 时所有动画/过渡归零。

---

## 主题系统

### Dark Mode（默认）

- 背景 `#161616`，Surface 5 级递增
- 文字 `#F2F2F2`
- 浮层带 `inset 0 1px 0 rgba(255,255,255,0.04)` 内发光
- 顶栏/底部导航毛玻璃

### Light Mode

- 背景 `#FFFFFF`，Surface 白色系
- 文字 `#000000`

### 密度

```js
MxDensity.set('compact');   // 圆角缩小 2px
MxDensity.set('normal');    // 标准
```

### 自定义

覆盖 CSS 变量即可：

```css
:root {
  --mx-primary: #E91E63;
  --mx-r: 20px;
}
```

---

## 工具库

| 类 | 方法 | 说明 |
|------|------|------|
| `MxSmoothCorner` | `.init()` | Smooth Corner → clip-path |
| `MxRipple` | `.init()` | 涟漪效果 |
| `MxCounter` | `.animate(el, to)` | 数字滚动 |
| `MxTheme` | `.toggle()` / `.set()` / `.get()` | 主题切换 |
| `MxDensity` | `.set()` / `.get()` | 密度管理 |
| `MxChart` | `.render()` / `.replay()` | 柱状图 |
| `MxModal` | `.open()` / `.close()` / `.confirm()` | 弹窗 |
| `MxToast` | `.show()` / `.success()` / `.error()` | 轻提示 |
| `MxSelect` | `.init()` / `.getValue()` / `.setValue()` | 下拉选择 |
| `MxTabs` | `.init()` / `.activate()` | 选项卡 |
| `MxPagination` | `.render(container, opts)` | 分页器 |
| `MxAccordion` | `.init()` / `.open()` / `.close()` | 折叠面板 |
| `MxDatePicker` | `.init()` / `.getValue()` / `.setValue()` | 日期选择 |
| `MxUpload` | `.init()` | 文件上传 |
| `MxGlass` | `.enable()` / `.disable()` / `.setBlur()` | 液体玻璃 |

---

## 浏览器兼容

| 浏览器 | 版本 | 说明 |
|--------|------|------|
| Chrome | 80+ | 完整支持 |
| Firefox | 80+ | 完整支持 |
| Safari | 14+ | 需要 `-webkit-backdrop-filter`，液体玻璃 SVG filter 部分支持 |
| Edge | 80+ | 完整支持 |
| iOS Safari | 14+ | 安全区域适配 |
| Android Chrome | 80+ | 完整支持 |

**关键 API 依赖：**
- CSS Custom Properties
- `backdrop-filter`
- `clip-path: path()`
- `ResizeObserver` / `MutationObserver`
- `CustomEvent`
- SVG `feDisplacementMap`（液体玻璃）

---

## License

Apache License 2.0
