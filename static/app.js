/* CodeArts2API 控制台前端逻辑
   数据来源：/api/status、/api/accounts、/api/refresh、/api/test
   组件 API 来自 miuix-console（MxToast / MxModal / MxCounter / MxTheme） */

(function () {
  'use strict';

  const state = {
    status: null,
    accounts: [],
    models: [],
    route: 'overview',
    editing: null,   // 正在编辑的账号 id；null 表示新增
    selectedModel: '',
  };

  const $ = (sel) => document.querySelector(sel);
  const $$ = (sel) => Array.from(document.querySelectorAll(sel));

  // ── 工具 ────────────────────────────────────────────────
  function escapeHtml(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function fmtNumber(n) {
    if (typeof n !== 'number' || !isFinite(n)) return '—';
    if (n >= 1_000_000) return (n / 1_000_000).toFixed(1).replace(/\.0$/, '') + 'M';
    if (n >= 1_000) return (n / 1_000).toFixed(1).replace(/\.0$/, '') + 'K';
    return String(n);
  }

  async function api(path, options) {
    const opts = Object.assign({ headers: { 'Content-Type': 'application/json' } }, options);
    const resp = await fetch(path, opts);
    const text = await resp.text();
    let data = null;
    try { data = text ? JSON.parse(text) : null; } catch (_) { data = { raw: text }; }
    if (!resp.ok) {
      const msg = (data && (data.error || data.message)) || ('HTTP ' + resp.status);
      throw new Error(typeof msg === 'string' ? msg : JSON.stringify(msg));
    }
    return data;
  }

  function toast(kind, msg) {
    if (window.MxToast && typeof MxToast[kind] === 'function') {
      MxToast[kind](msg);
    } else {
      console.log('[' + kind + ']', msg);
    }
  }

  // ── 路由 ────────────────────────────────────────────────
  const ROUTE_LABELS = {
    overview: '概览',
    accounts: '账号',
    models: '模型',
    connect: '客户端接入',
  };

  function setRoute(route) {
    if (!ROUTE_LABELS[route]) route = 'overview';
    state.route = route;
    $$('.route').forEach((el) => {
      el.hidden = el.id !== 'route-' + route;
    });
    $$('.mx-nav-item').forEach((el) => {
      el.classList.toggle('active', el.dataset.route === route);
    });
    $$('.mx-btm-item').forEach((el) => {
      el.classList.toggle('active', el.dataset.route === route);
    });
    const crumb = $('#crumb');
    if (crumb) crumb.textContent = ROUTE_LABELS[route];
    // 桌面端顶栏的"刷新模型"只在概览/模型页有意义
    const refreshBtn = $('#refreshBtn');
    if (refreshBtn) refreshBtn.style.visibility = (route === 'connect') ? 'hidden' : 'visible';
    if (history.replaceState) history.replaceState(null, '', '#' + route);
  }

  function currentRouteFromHash() {
    const h = (location.hash || '').replace(/^#/, '');
    return ROUTE_LABELS[h] ? h : 'overview';
  }

  // ── 渲染 ────────────────────────────────────────────────
  function renderStatus(data) {
    state.status = data;
    state.accounts = data.accounts || [];
    state.models = data.models || [];

    const summary = data.summary || {};
    const config = data.config || {};

    const setStat = (id, value, counter) => {
      const el = $(id);
      if (!el) return;
      if (counter && window.MxCounter && typeof MxCounter.animate === 'function') {
        try { MxCounter.animate(el, Number(value) || 0); return; } catch (_) { /* fall through */ }
      }
      el.textContent = value;
    };

    setStat('#statAccounts', summary.account_count || 0, true);
    setStat('#statModels', summary.model_count || 0, true);
    setStat('#statEnabled', summary.enabled_count || 0, true);
    const listenEl = $('#statListen');
    if (listenEl) listenEl.textContent = summary.listen || '—';

    const badge = $('#poolBadge');
    if (badge) {
      const enabled = summary.enabled_count || 0;
      badge.textContent = enabled > 1 ? ('多账号 · ' + enabled + ' 个启用') : '单账号';
      badge.className = 'mx-badge ' + (enabled > 1 ? 'mx-badge-success' : '');
    }

    // 概览页账号表
    const tbody = $('#overviewAccounts');
    if (tbody) {
      if (!state.accounts.length) {
        tbody.innerHTML = '<tr><td colspan="4" class="empty-cell">暂无账号</td></tr>';
      } else {
        tbody.innerHTML = state.accounts.map((a) => `
          <tr>
            <td data-label="账号" class="mono">${escapeHtml(a.id)}</td>
            <td data-label="AK" class="mono">${escapeHtml(a.ak)}</td>
            <td data-label="状态">${a.enabled
              ? '<span class="mx-badge mx-badge-success">启用</span>'
              : '<span class="mx-badge">停用</span>'}</td>
            <td data-label="模型数">${fmtNumber(a.model_count || 0)}</td>
          </tr>`).join('');
      }
    }
    const hint = $('#accountsHint');
    if (hint) hint.textContent = (state.accounts.length || 0) + ' 个账号';

    // 账号管理页
    renderAccountRows();
    const fileHint = $('#accountsFileHint');
    if (fileHint) fileHint.textContent = '配置文件：' + (config.accounts_file || '—');

    // 模型页
    renderModelRows();

    // 接入页
    const base = $('#connBase');
    if (base) base.textContent = 'http://' + (summary.listen || '127.0.0.1:8787') + '/v1';
    const connModel = $('#connModel');
    if (connModel) {
      const preferred = ['deepseek-v4.1-flash', 'glm-5.3', 'kimi-k3'];
      const found = preferred.find((m) => state.models.some((x) => x.id === m));
      connModel.textContent = found || (state.models[0] && state.models[0].id) || '—';
    }
  }

  function renderAccountRows() {
    const tbody = $('#accountRows');
    if (!tbody) return;
    if (!state.accounts.length) {
      tbody.innerHTML = '<tr><td colspan="5" class="empty-cell">还没有账号，点击右上角"添加账号"</td></tr>';
      return;
    }
    tbody.innerHTML = state.accounts.map((a) => `
      <tr>
        <td data-label="账号" class="mono">${escapeHtml(a.id)}</td>
        <td data-label="AK" class="mono">${escapeHtml(a.ak)}</td>
        <td data-label="模型数">${fmtNumber(a.model_count || 0)}</td>
        <td data-label="启用">
          <label class="mx-switch">
            <input type="checkbox" ${a.enabled ? 'checked' : ''} data-toggle="${escapeHtml(a.id)}">
            <span class="mx-switch-track"><span class="mx-switch-thumb"></span></span>
          </label>
        </td>
        <td data-label="操作">
          <div class="row-actions">
            <button class="mx-btn mx-btn-sm mx-btn-secondary" data-edit="${escapeHtml(a.id)}" data-ripple>编辑</button>
            <button class="mx-btn mx-btn-sm mx-btn-danger" data-del="${escapeHtml(a.id)}" data-ripple>删除</button>
          </div>
        </td>
      </tr>`).join('');
  }

  function modelOwnerNames(modelId) {
    const owners = [];
    state.accounts.forEach((a) => {
      if (Array.isArray(a.models) && a.models.includes(modelId)) owners.push(a.id);
    });
    return owners;
  }

  function renderModelRows() {
    const tbody = $('#modelRows');
    if (!tbody) return;
    const filter = ($('#modelFilter') && $('#modelFilter').value || '').trim().toLowerCase();
    const list = state.models.filter((m) => !filter || m.id.toLowerCase().includes(filter));

    const countEl = $('#modelCount');
    if (countEl) countEl.textContent = list.length + ' / ' + state.models.length;

    if (!list.length) {
      tbody.innerHTML = '<tr><td colspan="6" class="empty-cell">没有匹配的模型</td></tr>';
      return;
    }

    tbody.innerHTML = list.map((m) => {
      const owners = modelOwnerNames(m.id);
      const channel = m.channel === 'benefit'
        ? '<span class="mx-badge mx-badge-warning">免费</span>'
        : '<span class="mx-badge mx-badge-success">套餐</span>';
      return `
        <tr>
          <td data-label="模型" class="mono">${escapeHtml(m.id)}</td>
          <td data-label="通道">${channel}</td>
          <td data-label="上下文">${m.context_window ? fmtNumber(m.context_window) : '—'}</td>
          <td data-label="最大输出">${m.max_tokens ? fmtNumber(m.max_tokens) : '—'}</td>
          <td data-label="可用账号">${owners.length ? owners.map(escapeHtml).join(', ') : '—'}</td>
          <td data-label="操作">
            <button class="mx-btn mx-btn-sm mx-btn-secondary" data-test-model="${escapeHtml(m.id)}" data-ripple>测试</button>
          </td>
        </tr>`;
    }).join('');
  }

  // ── 数据加载 ────────────────────────────────────────────
  async function loadStatus(showSpinner) {
    try {
      const data = await api('/api/status');
      renderStatus(data);
      if (showSpinner) toast('success', '状态已刷新');
    } catch (e) {
      toast('error', '加载失败：' + e.message);
      const badge = $('#poolBadge');
      if (badge) { badge.textContent = '离线'; badge.className = 'mx-badge mx-badge-error'; }
    }
  }

  async function refreshModels() {
    const btn = $('#refreshBtn');
    if (btn) { btn.disabled = true; btn.textContent = '同步中…'; }
    try {
      const data = await api('/api/refresh', { method: 'POST' });
      const failed = (data.accounts || []).filter((a) => !a.ok);
      if (failed.length) {
        toast('warning', failed.length + ' 个账号同步失败：' + failed.map((a) => a.account).join(', '));
      } else {
        toast('success', '已同步 ' + (data.accounts || []).length + ' 个账号');
      }
      await loadStatus(false);
    } catch (e) {
      toast('error', '同步失败：' + e.message);
    } finally {
      if (btn) { btn.disabled = false; btn.textContent = '刷新模型'; }
    }
  }

  // ── 账号操作 ────────────────────────────────────────────
  function openAccountModal(accountId) {
    state.editing = accountId || null;
    const modal = $('#accountModal');
    const title = $('#accountModalTitle');
    const fId = $('#fId');
    const fAk = $('#fAk');
    const fSk = $('#fSk');
    const hint = $('#editHint');

    if (accountId) {
      const account = state.accounts.find((a) => a.id === accountId);
      title.textContent = '编辑账号';
      fId.value = accountId;
      fId.disabled = true;
      fAk.value = '';
      fAk.placeholder = '留空则不修改（当前 ' + (account ? account.ak : '') + '）';
      fSk.value = '';
      hint.hidden = false;
    } else {
      title.textContent = '添加账号';
      fId.value = '';
      fId.disabled = false;
      fAk.value = '';
      fAk.placeholder = '华为云 Access Key';
      fSk.value = '';
      hint.hidden = true;
    }
    if (window.MxModal && typeof MxModal.open === 'function') {
      MxModal.open('accountModal');
    } else {
      modal.classList.add('open');
    }
  }

  function closeAccountModal() {
    if (window.MxModal && typeof MxModal.close === 'function') {
      MxModal.close('accountModal');
    } else {
      $('#accountModal').classList.remove('open');
    }
  }

  async function saveAccount() {
    const id = $('#fId').value.trim();
    const ak = $('#fAk').value.trim();
    const sk = $('#fSk').value.trim();
    const btn = $('#saveAccountBtn');

    if (btn) { btn.disabled = true; }
    try {
      if (state.editing) {
        const payload = {};
        if (ak) payload.ak = ak;
        if (sk) payload.sk = sk;
        if (!Object.keys(payload).length) {
          toast('info', '没有需要保存的变更');
          return;
        }
        await api('/api/accounts/' + encodeURIComponent(state.editing), {
          method: 'PATCH',
          body: JSON.stringify(payload),
        });
        toast('success', '账号已更新');
      } else {
        if (!ak || !sk) { toast('error', 'AK 和 SK 都必须填写'); return; }
        await api('/api/accounts', {
          method: 'POST',
          body: JSON.stringify({ id, ak, sk }),
        });
        toast('success', '账号已添加并同步模型');
      }
      closeAccountModal();
      await loadStatus(false);
    } catch (e) {
      toast('error', e.message);
    } finally {
      if (btn) { btn.disabled = false; }
    }
  }

  async function toggleAccount(id, enabled) {
    try {
      await api('/api/accounts/' + encodeURIComponent(id), {
        method: 'PATCH',
        body: JSON.stringify({ enabled }),
      });
      toast('success', enabled ? '账号已启用' : '账号已停用');
      await loadStatus(false);
    } catch (e) {
      toast('error', e.message);
      await loadStatus(false);
    }
  }

  async function deleteAccount(id) {
    let ok = true;
    if (window.MxModal && typeof MxModal.confirm === 'function') {
      try {
        ok = await MxModal.confirm({
          title: '删除账号',
          message: '确定删除账号 "' + id + '"？该操作会写回 accounts.json。',
          type: 'danger',
        });
      } catch (_) { ok = false; }
    } else {
      ok = window.confirm('确定删除账号 "' + id + '"？');
    }
    if (!ok) return;
    try {
      await api('/api/accounts/' + encodeURIComponent(id), { method: 'DELETE' });
      toast('success', '账号已删除');
      await loadStatus(false);
    } catch (e) {
      toast('error', e.message);
    }
  }

  // ── 请求测试 ────────────────────────────────────────────
  function selectTestModel(modelId) {
    state.selectedModel = modelId;
    const badge = $('#testModelBadge');
    if (badge) badge.textContent = modelId;
    const card = $('#testCard');
    if (card && card.scrollIntoView) card.scrollIntoView({ behavior: 'smooth', block: 'center' });
    const prompt = $('#testPrompt');
    if (prompt) prompt.focus();
  }

  async function runTest() {
    const model = state.selectedModel;
    if (!model) { toast('warning', '请先在模型列表点"测试"选择模型'); return; }
    const prompt = ($('#testPrompt') && $('#testPrompt').value) || '只回复两个字：成功';
    const btn = $('#runTestBtn');
    const meta = $('#testMeta');
    const out = $('#testOutput');
    const pre = $('#testPre');

    if (btn) { btn.disabled = true; btn.textContent = '请求中…'; }
    if (meta) meta.textContent = '正在请求 ' + model + '…';
    try {
      const data = await api('/api/test', {
        method: 'POST',
        body: JSON.stringify({ model, prompt, max_tokens: 512 }),
      });
      if (out) out.hidden = false;
      if (pre) pre.textContent = JSON.stringify(data, null, 2);
      if (data.ok) {
        if (meta) meta.textContent = '成功 · ' + data.elapsed_ms + 'ms';
        toast('success', '模型响应正常');
      } else {
        if (meta) meta.textContent = '失败 · ' + (data.status ? 'HTTP ' + data.status : '网络错误');
        toast('error', '请求失败，详见响应');
      }
    } catch (e) {
      if (out) out.hidden = false;
      if (pre) pre.textContent = '错误：' + e.message;
      if (meta) meta.textContent = '失败';
      toast('error', e.message);
    } finally {
      if (btn) { btn.disabled = false; btn.textContent = '发送测试'; }
    }
  }

  // ── 事件绑定 ────────────────────────────────────────────
  function bindEvents() {
    $$('.mx-nav-item, .mx-btm-item').forEach((el) => {
      el.addEventListener('click', () => {
        setRoute(el.dataset.route);
      });
    });

    const refreshBtn = $('#refreshBtn');
    if (refreshBtn) refreshBtn.addEventListener('click', refreshModels);

    const addBtn = $('#addAccountBtn');
    if (addBtn) addBtn.addEventListener('click', () => openAccountModal(null));

    const saveBtn = $('#saveAccountBtn');
    if (saveBtn) saveBtn.addEventListener('click', saveAccount);

    $$('[data-close-modal]').forEach((el) => {
      el.addEventListener('click', closeAccountModal);
    });

    // 表格内的事件委托：编辑 / 删除 / 启停 / 测试
    document.addEventListener('click', (ev) => {
      const editBtn = ev.target.closest('[data-edit]');
      if (editBtn) { openAccountModal(editBtn.dataset.edit); return; }

      const delBtn = ev.target.closest('[data-del]');
      if (delBtn) { deleteAccount(delBtn.dataset.del); return; }

      const testBtn = ev.target.closest('[data-test-model]');
      if (testBtn) { selectTestModel(testBtn.dataset.testModel); return; }
    });

    document.addEventListener('change', (ev) => {
      const toggle = ev.target.closest('[data-toggle]');
      if (toggle) toggleAccount(toggle.dataset.toggle, toggle.checked);
    });

    const filter = $('#modelFilter');
    if (filter) filter.addEventListener('input', renderModelRows);

    const runBtn = $('#runTestBtn');
    if (runBtn) runBtn.addEventListener('click', runTest);

    const themeBtn = document.querySelector('[data-theme-toggle]');
    if (themeBtn) {
      themeBtn.addEventListener('click', () => {
        if (window.MxTheme && typeof MxTheme.toggle === 'function') {
          MxTheme.toggle();
        } else {
          const html = document.documentElement;
          html.dataset.theme = html.dataset.theme === 'dark' ? 'light' : 'dark';
        }
      });
    }

    window.addEventListener('hashchange', () => {
      setRoute(currentRouteFromHash());
    });
  }

  // ── 启动 ────────────────────────────────────────────────
  async function boot() {
    bindEvents();
    setRoute(currentRouteFromHash());
    await loadStatus(false);
    // 每 20 秒静默刷新一次，账号状态与模型变化能及时反映
    setInterval(() => loadStatus(false), 20000);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();
