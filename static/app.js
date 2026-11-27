/* CodeArts2API 控制台前端逻辑
   数据来源：/api/status、/api/accounts、/api/refresh、/api/test
   组件 API 来自 miuix-console（MxToast / MxModal / MxCounter / MxTheme） */

(function () {
  'use strict';

  const isAndroid = document.documentElement.dataset.platform === 'android';

  const state = {
    status: null,
    accounts: [],
    models: [],
    route: 'overview',
    editing: null,   // 正在编辑的账号 id；null 表示新增
    selectedModel: '',
    usageHours: 24,
    quota: null,
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
    quota: '额度',
    usage: '用量',
    checkin: '签到',
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
    if (route === 'quota') loadQuota(false);
    if (route === 'usage') loadUsage();
    if (route === 'checkin') loadCheckin();
    if (route === 'connect' && isAndroid) loadMobileConnection();
    // 桌面端顶栏的"刷新模型"只在概览/模型页有意义
    const refreshBtn = $('#refreshBtn');
    if (refreshBtn) refreshBtn.style.visibility = (route === 'connect') ? 'hidden' : 'visible';
    if (history.replaceState) history.replaceState(null, '', '#' + route);
  }

  function currentRouteFromHash() {
    const h = (location.hash || '').replace(/^#/, '');
    return ROUTE_LABELS[h] ? h : 'overview';
  }

  async function loadMobileConnection() {
    const key = $('#connKey');
    key.textContent = '加载中…';
    try {
      const config = await api('/api/mobile');
      $('#connBase').textContent = config.base_url;
      key.textContent = config.api_key;
      $('#mobileDataDir').textContent = config.data_dir;
      $('#mobileDataRow').hidden = false;
      $('#connKeyHint').textContent = '请复制此密钥；代理会校验 Authorization 或 x-api-key。';
      $('#connNetworkHint').textContent = config.allow_lan
        ? '同一局域网的客户端请把 127.0.0.1 换成手机的局域网 IP。管理面板仅供本机访问。'
        : '代理仅在本机监听。若需在局域网访问，在 CodeArts/.env 设置 CODEARTS_PROXY_HOST=0.0.0.0 后重启应用。';
    } catch (e) {
      key.textContent = '读取失败';
      toast('error', e.message);
    }
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
    const concEl = $('#statConc');
    if (concEl) {
      concEl.textContent = (summary.concurrency_limit || 3) + ' 并发 · ' +
        (summary.pinned_sessions || 0) + ' 钉号';
    }

    const badge = $('#poolBadge');
    if (badge) {
      const enabled = summary.enabled_count || 0;
      if (enabled === 0) {
        badge.textContent = '待登录';
        badge.className = 'mx-badge mx-badge-error';
      } else {
        badge.textContent = enabled > 1 ? ('多账号 · ' + enabled + ' 个启用') : '单账号';
        badge.className = 'mx-badge ' + (enabled > 1 ? 'mx-badge-success' : '');
      }
    }

    // 概览页账号池卡片
    renderPoolCards(data);
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
    if (base) base.textContent = (isAndroid ? location.origin :
      'http://' + (summary.listen || '127.0.0.1:8787')) + '/v1';
    const connModel = $('#connModel');
    if (connModel) {
      const preferred = ['deepseek-v4.1-flash', 'glm-5.3', 'kimi-k3'];
      const found = preferred.find((m) => state.models.some((x) => x.id === m));
      connModel.textContent = found || (state.models[0] && state.models[0].id) || '—';
    }
  }

  // ── 账号池与轮换 ────────────────────────────────────────
  const STRATEGY_TEXT = {
    balanced: '负载均衡',
    credit: '余额优先',
    roundrobin: '严格轮转',
  };

  function fmtIdle(ms) {
    if (ms == null) return '本次未用过';
    if (ms < 1000) return '刚刚用过';
    const s = Math.round(ms / 1000);
    if (s < 60) return s + ' 秒前';
    const m = Math.round(s / 60);
    if (m < 60) return m + ' 分钟前';
    return Math.round(m / 60) + ' 小时前';
  }

  function renderRotationPolicy(rotation, config) {
    const strategy = (rotation && rotation.strategy) || (config && config.rotation) || 'balanced';
    const label = (rotation && rotation.strategy_label) || STRATEGY_TEXT[strategy] || strategy;
    const badge = $('#rotationStrategy');
    if (badge) {
      badge.textContent = label;
      badge.className = 'mx-badge mx-badge-success';
    }
    const pageBadge = $('#rotationStrategyBadge');
    if (pageBadge) {
      pageBadge.textContent = '当前：' + label;
      pageBadge.className = 'mx-badge mx-badge-success';
    }
    const note = $('#rotationNote');
    const minCredit = (rotation && rotation.min_credit) || (config && config.min_credit) || 0;
    const summary = strategy === 'credit'
      ? '按剩余 credit 排序，余额多的先接新会话。'
        + (minCredit > 0 ? ' 低于 ' + minCredit + ' credit 的账号只在其它账号都不可用时才接流量。' : '')
      : strategy === 'roundrobin'
        ? '只按「最久没被选中」轮转，请求数严格平均分配，不看负载。'
        : '在途少的账号先接新会话；负载相同时按最久未用轮转，空闲时自动均摊。'
          + (minCredit > 0 ? ' 低于 ' + minCredit + ' credit 的账号会被降级为兜底。' : '');
    if (note) note.textContent = summary;
    // 概览页用同一段说明的短句版本
    const overviewHint = $('#rotationHint');
    if (overviewHint) {
      const enabled = (rotation && rotation.accounts || []).filter((r) => {
        const acc = state.accounts.find((a) => a.id === r.id);
        return acc && acc.enabled;
      });
      const ready = enabled.filter((r) => !r.cooling && !r.daily_exhausted).length;
      overviewHint.textContent = '新会话按「' + label + '」选号，'
        + ready + '/' + enabled.length + ' 个账号当前可接流量；冷却或额度耗尽的排在队尾兜底。';
    }
    $$('#rotationPicker .mx-btn').forEach((btn) => {
      btn.classList.toggle('active', btn.dataset.strategy === strategy);
    });
  }

  function renderPoolCards(data) {
    const box = $('#poolCards');
    if (!box) return;
    const rotation = (data && data.rotation) || {};
    renderRotationPolicy(rotation, (data && data.config) || {});

    const rotationById = new Map((rotation.accounts || []).map((r) => [r.id, r]));
    const health = (data && data.health) || {};
    const list = (state.accounts || []).slice().sort((a, b) => {
      const ra = rotationById.has(a.id) ? rotationById.get(a.id).rank : a.enabled ? 999 : 10000;
      const rb = rotationById.has(b.id) ? rotationById.get(b.id).rank : b.enabled ? 999 : 10000;
      return ra - rb;
    });

    if (!list.length) {
      box.innerHTML = '<p class="mx-helper">还没有账号 —— 去「账号」页点「登录华为云」完成首次登录。</p>';
      return;
    }

    box.innerHTML = list.map((a) => {
      const r = rotationById.get(a.id) || {};
      const h = health[a.id] || {};
      const inFlight = r.in_flight != null ? r.in_flight : (h.in_flight || 0);
      const limit = r.limit || h.limit || 1;
      const pct = Math.min(100, Math.round((inFlight / Math.max(1, limit)) * 100));

      // 卡片优先级：冷却/额度耗尽 > 负载高 > 正常；排序第一的号高亮
      let cls = 'pool-card';
      const notes = [];
      if (r.cooling || h.available === false) {
        cls += ' danger';
        notes.push('账号冷却中' + (h.cooldown_remaining ? '（剩 ' + h.cooldown_remaining + 's）' : '') + '，排在队尾');
      }
      if (r.daily_exhausted) {
        cls += ' warn';
        notes.push('今日额度已用尽，仅在其它账号不可用时接管');
      }
      if (r.model_cooling) {
        cls += ' warn';
        notes.push('当前模型在该账号上冷却，其它模型不受影响');
      }
      if ((r.consecutive_failures || 0) > 0) {
        notes.push('连续失败 ' + r.consecutive_failures + ' 次');
      }
      if (!a.enabled) {
        cls += ' warn';
        notes.push('已在配置中停用，不参与选号');
      }
      if (cls === 'pool-card' && r.rank === 0 && a.enabled) cls += ' primary';

      const creditText = r.credit_remain != null
        ? (fmtNumber(Math.round(r.credit_remain)) + (r.credit_ratio != null
          ? ' · ' + Math.round(r.credit_ratio * 100) + '%'
          : ''))
        : '未刷新';

      return `
      <div class="${cls}">
        <div class="pool-card-head">
          <span class="pool-name" title="${escapeHtml(a.id)}">${escapeHtml(a.id)}</span>
          <span class="pool-rank">${a.enabled && r.rank != null ? '优先级 #' + (r.rank + 1) : '不参与'}</span>
        </div>
        <div class="pool-meta">
          <div><b>${inFlight}/${limit}</b>在途</div>
          <div><b>${creditText}</b>剩余 credit</div>
          <div><b>${fmtNumber(a.model_count || 0)}</b>模型</div>
          <div><b>${escapeHtml(fmtIdle(r.idle_ms))}</b>上次选号</div>
        </div>
        <div class="pool-load${pct >= 90 ? ' danger' : pct >= 60 ? ' warn' : ''}"><span style="width:${pct}%"></span></div>
        ${notes.length ? '<div class="pool-note">' + notes.map(escapeHtml).join('<br>') + '</div>' : ''}
      </div>`;
    }).join('');
  }

  function renderAccountRows() {
    const tbody = $('#accountRows');
    if (!tbody) return;
    if (!state.accounts.length) {
      tbody.innerHTML = '<tr><td colspan="6" class="empty-cell">还没有账号，点击上方「登录华为云」扫码登录即可</td></tr>';
      return;
    }
    const health = (state.status && state.status.health) || {};
    const oauthById = new Map(((state.status && state.status.oauth_accounts) || [])
      .map((item) => [item.user_id, item]));
    tbody.innerHTML = state.accounts.map((a) => {
      const h = health[a.id] || {};
      const isOauth = a.oauth || (!a.ak || a.ak === '****');
      const identity = oauthById.get(a.id);
      let credBadge = isOauth
        ? '<span class="mx-badge mx-badge-success">OAuth</span>'
        : '<span class="mx-badge">AK/SK</span>';
      if (isOauth && !identity) {
        credBadge = '<span class="mx-badge mx-badge-error" title="请重新登录华为云">OAuth 凭证缺失</span>';
      } else if (isOauth && identity.remaining_secs <= 0) {
        credBadge = '<span class="mx-badge mx-badge-error" title="若自动续期失败，请重新登录华为云">OAuth 已过期</span>';
      } else if (isOauth && identity.expiring_soon) {
        credBadge = '<span class="mx-badge mx-badge-warning">OAuth 即将续期</span>';
      }
      let healthBadge;
      if (h.available === false) {
        healthBadge = '<span class="mx-badge mx-badge-error">冷却 ' + (h.cooldown_remaining || 0) + 's</span>';
      } else if ((h.in_flight || 0) > 0) {
        healthBadge = '<span class="mx-badge mx-badge-warning">' + h.in_flight + '/' + (h.limit || 3) + '</span>';
      } else if ((h.total_failures || 0) > 0) {
        healthBadge = '<span class="mx-badge mx-badge-warning">失败 ' + h.total_failures + '</span>';
      } else {
        healthBadge = '<span class="mx-badge mx-badge-success">正常</span>';
      }
      return `
      <tr>
        <td data-label="账号" class="mono">${escapeHtml(a.id)}</td>
        <td data-label="凭证">${credBadge}</td>
        <td data-label="模型数">${fmtNumber(a.model_count || 0)}</td>
        <td data-label="健康">${healthBadge}</td>
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
      </tr>`;
    }).join('');
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

  // ── 额度 ────────────────────────────────────────────────
  function fmtCredit(n) {
    if (typeof n !== 'number' || !isFinite(n)) return '—';
    return n.toLocaleString('zh-CN', { maximumFractionDigits: 2 });
  }

  async function loadQuota(forceRefresh) {
    try {
      if (forceRefresh) {
        const btn = $('#refreshQuotaBtn');
        if (btn) { btn.disabled = true; btn.textContent = '刷新中…'; }
        await api('/api/quota/refresh', { method: 'POST' });
        if (btn) { btn.disabled = false; btn.textContent = '刷新额度'; }
        toast('success', '额度已刷新');
      }
      const data = await api('/api/quota');
      state.quota = data;
      renderQuota(data);
    } catch (e) {
      toast('error', '额度加载失败：' + e.message);
    }
  }

  function renderQuota(data) {
    const accounts = data.accounts || [];
    const okAccounts = accounts.filter((a) => a.ok);

    const total = okAccounts.reduce((sum, a) => sum + (a.total_amount || 0), 0);
    const remain = okAccounts.reduce((sum, a) => sum + (a.total_remain || 0), 0);
    const used = okAccounts.reduce((sum, a) => sum + (a.total_used || 0), 0);
    const tokens = okAccounts.reduce((sum, a) => sum + (a.used_tokens || 0), 0);

    const setText = (sel, text) => { const el = $(sel); if (el) el.textContent = text; };
    setText('#qTotal', okAccounts.length ? fmtCredit(total) : '—');
    setText('#qRemain', okAccounts.length ? fmtCredit(remain) : '—');
    setText('#qUsed', okAccounts.length ? fmtCredit(used) : '—');
    setText('#qTokens', okAccounts.length ? fmtNumber(tokens) : '—');

    const box = $('#quotaAccounts');
    if (box) {
      if (!accounts.length) {
        box.innerHTML = '<p class="helper-block">还没有账号。</p>';
      } else {
        box.innerHTML = accounts.map((a) => {
          if (!a.ok) {
            return '<div class="quota-account">' +
              '<div class="quota-head"><span class="quota-name">' + escapeHtml(a.id) + '</span>' +
              '<span class="mx-badge mx-badge-error">获取失败</span></div>' +
              '<p class="helper-block" style="margin-top:8px">' + escapeHtml(a.error || '未知错误') + '</p>' +
              '</div>';
          }
          const pct = a.total_amount > 0 ? (a.total_remain / a.total_amount) * 100 : 0;
          const cls = pct < 10 ? 'danger' : (pct < 30 ? 'warn' : '');
          const pkgs = (a.packages || []).map((p) => {
            const ppct = p.amount > 0 ? (p.remain / p.amount) * 100 : 0;
            return '<div class="pkg-row">' +
              '<span class="pkg-name">' + escapeHtml(p.name) + '</span>' +
              '<span>' + fmtCredit(p.remain) + ' / ' + fmtCredit(p.amount) + '</span>' +
              '<span>' + ppct.toFixed(0) + '%</span></div>';
          }).join('');
          return '<div class="quota-account">' +
            '<div class="quota-head">' +
            '<span class="quota-name">' + escapeHtml(a.id) + '</span>' +
            '<span class="quota-nums">剩余 <strong>' + fmtCredit(a.total_remain) + '</strong> / ' +
            fmtCredit(a.total_amount) + ' · 已用 ' + fmtCredit(a.total_used) +
            ' · ' + fmtNumber(a.used_tokens) + ' tokens</span>' +
            '</div>' +
            '<div class="credit-bar ' + cls + '"><span style="width:' +
            Math.max(0, Math.min(100, pct)).toFixed(1) + '%"></span></div>' +
            (pkgs ? '<div class="quota-pkgs">' + pkgs + '</div>' : '') +
            '</div>';
        }).join('');
      }
    }

    const linksBox = $('#quotaLinks');
    if (linksBox) {
      linksBox.innerHTML = (data.links || []).map((l) =>
        '<div class="link-row">' +
          '<div class="link-main">' +
            '<a class="link-title" href="' + escapeHtml(l.url) + '" target="_blank" rel="noopener noreferrer">' +
              escapeHtml(l.name) + '</a>' +
            '<span class="link-desc">' + escapeHtml(l.desc || '') + '</span>' +
          '</div>' +
          '<span class="mx-badge">打开</span>' +
        '</div>').join('');
    }
  }

  // ── 用量 ────────────────────────────────────────────────
  async function loadUsage() {
    try {
      const hours = state.usageHours;
      const bucket = hours <= 48 ? 'hour' : 'day';
      const [summary, recent] = await Promise.all([
        api('/api/usage?hours=' + hours + '&bucket=' + bucket),
        api('/api/usage/recent?hours=' + hours),
      ]);
      renderUsage(summary, recent);
    } catch (e) {
      toast('error', '用量加载失败：' + e.message);
    }
  }

  function renderUsage(summary, recent) {
    const s = summary.summary || {};
    const setText = (sel, text) => { const el = $(sel); if (el) el.textContent = text; };
    setText('#uRequests', fmtNumber(s.requests || 0));
    setText('#uTokens', fmtNumber(s.total_tokens || 0));
    setText('#uLatency', (s.avg_elapsed_ms || 0) + ' ms');
    setText('#uFailed', fmtNumber(s.failed_requests || 0));

    const chart = $('#usageChart');
    if (chart) {
      const series = summary.series || [];
      if (!series.length) {
        chart.innerHTML = '<div class="chart-empty">该时间段内没有请求记录</div>';
      } else {
        const max = Math.max.apply(null, series.map((x) => x.total_tokens || 0).concat([1]));
        // 点位很少时不要 flex 拉伸（否则一根柱子会占满整行），
        // 改为固定宽度左对齐；点位多时再均分宽度。
        const dense = series.length >= 12;
        const wrapStyle = dense ? '' : ' style="flex:0 0 56px"';
        chart.innerHTML = series.map((x) => {
          const h = Math.max(2, ((x.total_tokens || 0) / max) * 100);
          const d = new Date((x.ts || 0) * 1000);
          const label = d.toLocaleString('zh-CN', { month: '2-digit', day: '2-digit', hour: '2-digit' });
          const tip = label + ' · ' + fmtNumber(x.total_tokens || 0) + ' tokens · ' + (x.requests || 0) + ' 次';
          return '<div class="bar-wrap"' + wrapStyle + ' data-tip="' + escapeHtml(tip) + '">' +
                 '<div class="bar" style="height:' + h.toFixed(1) + '%"></div></div>';
        }).join('');
      }
    }

    const byModel = $('#usageByModel');
    if (byModel) {
      const rows = summary.by_model || [];
      byModel.innerHTML = rows.length
        ? rows.map((m) =>
            '<tr>' +
            '<td data-label="模型" class="mono">' + escapeHtml(m.model) + '</td>' +
            '<td data-label="请求">' + fmtNumber(m.requests || 0) + '</td>' +
            '<td data-label="Tokens">' + fmtNumber(m.total_tokens || 0) + '</td>' +
            '<td data-label="平均耗时">' + (m.avg_elapsed_ms || 0) + ' ms</td>' +
            '</tr>').join('')
        : '<tr><td colspan="4" class="empty-cell">暂无数据</td></tr>';
    }

    const recentBody = $('#usageRecent');
    if (recentBody) {
      const records = recent.records || [];
      recentBody.innerHTML = records.length
        ? records.map((r) => {
            const d = new Date((r.ts || 0) * 1000);
            const time = d.toLocaleString('zh-CN', { hour12: false });
            const result = r.ok
              ? '<span class="mx-badge mx-badge-success">成功</span>'
              : '<span class="mx-badge mx-badge-error">' + escapeHtml(r.error || '失败') + '</span>';
            return '<tr>' +
              '<td data-label="时间">' + escapeHtml(time) + '</td>' +
              '<td data-label="模型" class="mono">' + escapeHtml(r.model) + '</td>' +
              '<td data-label="账号" class="mono">' + escapeHtml(r.account || '—') + '</td>' +
              '<td data-label="Tokens">' + fmtNumber(r.total_tokens || 0) + '</td>' +
              '<td data-label="耗时">' + (r.elapsed_ms || 0) + ' ms</td>' +
              '<td data-label="结果">' + result + '</td>' +
              '</tr>';
          }).join('')
        : '<tr><td colspan="6" class="empty-cell">暂无请求记录</td></tr>';
    }

    const retained = $('#usageRetained');
    if (retained) retained.textContent = '保留 ' + (recent.retained || 0) + ' 条';
  }

  // ── 签到（AK/SK） ───────────────────────────────────────
  const KIND_LABEL = {
    USER_LOGIN: '每日签到',
    DAILY_CLAIM: '每日签到',
    NEW_USER_REGISTER: '新用户注册',
    INVITE_USER: '邀请好友',
    STUDENT_CERTIFIED: '学生认证',
  };

  async function loadCheckin() {
    const msg = $('#checkinMessage');
    if (msg) msg.textContent = '正在查询各账号签到状态…';
    try {
      const data = await api('/api/checkin/status');
      renderCheckin(data);
    } catch (e) {
      if (msg) msg.textContent = '查询失败：' + e.message;
    }
    const last = $('#checkinLast');
    try {
      const { last: result } = await api('/api/checkin/last');
      last.hidden = !result;
      if (result) last.textContent = '最近签到 · ' +
        new Date(result.at * 1000).toLocaleString() + ' · ' + result.message;
    } catch (_) { last.hidden = true; }
  }

  function statusBadge(status) {
    if (status === 'CONFIRMED') return ['已到账', 'mx-badge mx-badge-success'];
    if (status === 'CLAIMED') return ['已领取', 'mx-badge mx-badge-success'];
    if (status === 'CONSUME' || status === 'CONSUMED') return ['已使用', 'mx-badge'];
    return ['待领取', 'mx-badge mx-badge-warning'];
  }

  function renderCheckin(data) {
    const badge = $('#checkinBadge');
    const msg = $('#checkinMessage');
    const box = $('#checkinAccounts');
    const runBtn = $('#runCheckinBtn');

    const accounts = data.accounts || [];
    const completedCount = data.completed_count ?? accounts.filter((a) => a.ok && a.claimed_today).length;
    const errorCount = data.query_error_count ?? accounts.filter((a) => !a.ok).length;
    const claimed = accounts.length > 0 && completedCount === accounts.length;

    if (badge) {
      if (!accounts.length) {
        badge.textContent = '无账号';
        badge.className = 'mx-badge mx-badge-error';
      } else if (errorCount === accounts.length) {
        badge.textContent = '查询失败';
        badge.className = 'mx-badge mx-badge-error';
      } else {
        badge.textContent = claimed ? '今日已签到' : '待签到 ' + (accounts.length - completedCount) + ' 个账号';
        badge.className = 'mx-badge ' + (claimed ? 'mx-badge-success' : 'mx-badge-warning');
      }
    }
    if (runBtn) runBtn.disabled = accounts.length === 0;
    if (msg) {
      msg.textContent = !accounts.length
        ? '还没有配置账号，请先到「账号」页添加。'
        : claimed
          ? '全部 ' + accounts.length + ' 个账号的每日签到均已到账。'
          : '已完成 ' + completedCount + '/' + accounts.length + ' 个账号；剩余账号会自动领取所有待签到的每日活动。'
            + (errorCount ? '其中 ' + errorCount + ' 个账号查询失败。' : '');
    }

    if (!box) return;
    if (!accounts.length) { box.innerHTML = ''; return; }

    box.innerHTML = accounts.map((acc) => {
      if (!acc.ok) {
        return '<div class="quota-account">' +
          '<div class="quota-head"><span class="quota-name">' + escapeHtml(acc.account) + '</span>' +
          '<span class="mx-badge mx-badge-error">查询失败</span></div>' +
          '<p class="helper-block" style="margin-top:8px">' + escapeHtml(acc.message || '') + '</p>' +
          '</div>';
      }
      const rows = (acc.activities || []).map((a) => {
        const pair = statusBadge(a.status);
        const kind = KIND_LABEL[a.kind] || a.kind;
        const amount = a.benefit_amount ? (a.benefit_amount + ' 积分') : '—';
        return '<tr>' +
          '<td data-label="活动">' + escapeHtml(a.title || '') + '</td>' +
          '<td data-label="类型">' + escapeHtml(kind) + '</td>' +
          '<td data-label="奖励">' + escapeHtml(amount) + '</td>' +
          '<td data-label="状态"><span class="' + pair[1] + '">' + pair[0] + '</span></td>' +
          '</tr>';
      }).join('');

      return '<div class="quota-account">' +
        '<div class="quota-head">' +
        '<span class="quota-name">' + escapeHtml(acc.account) + '</span>' +
        '<span class="quota-nums">' +
        (acc.claimed_today ? '今日已签到' : '今日未签到') +
        (acc.benefit_amount ? ' · 待领 ' + acc.benefit_amount + ' 积分' : '') +
        '</span>' +
        '</div>' +
        '<div class="mx-table-wrap" style="margin-top:10px">' +
        '<table class="mx-table"><thead><tr>' +
        '<th>活动</th><th>类型</th><th>奖励</th><th>状态</th>' +
        '</tr></thead><tbody>' + rows + '</tbody></table>' +
        '</div></div>';
    }).join('');
  }

  async function runCheckin() {
    const btn = $('#runCheckinBtn');
    const msg = $('#checkinMessage');
    if (btn) { btn.disabled = true; btn.textContent = '签到中…'; }
    if (msg) msg.textContent = '正在提交签到…';
    try {
      const data = await api('/api/checkin/run', { method: 'POST' });
      const accounts = data.accounts || [];
      const first = accounts[0];
      toast(data.ok ? 'success' : 'error', (first && first.message) || (data.ok ? '签到完成' : '签到失败'));
      if (msg && first) msg.textContent = first.message;
      await loadCheckin();
    } catch (e) {
      toast('error', e.message);
      if (msg) msg.textContent = '签到失败：' + e.message;
    } finally {
      if (btn) { btn.disabled = false; btn.textContent = '立即签到'; }
    }
  }


  // ── OAuth 登录 ──────────────────────────────────────────
  let oauthPollTimer = null;

  async function startOauthLogin() {
    const hint = $('#oauthHint');
    const btn = $('#oauthLoginBtn');
    const linkBtn = $('#oauthLinkBtn');

    if (btn) { btn.disabled = true; btn.textContent = '正在打开…'; }
    if (hint) hint.textContent = '已打开浏览器授权页，请完成登录…';

    try {
      const data = await api('/api/oauth/start', { method: 'POST' });
      if (linkBtn) {
        linkBtn.hidden = false;
        linkBtn.onclick = () => isAndroid
          ? location.assign(data.auth_url)
          : window.open(data.auth_url, '_blank', 'noopener');
      }
      if (hint) {
        hint.textContent = data.opened || isAndroid
          ? '浏览器已打开授权页，请完成登录（本页会自动检测）'
          : '请点击「打开授权页」完成登录';
      }
      pollOauth(data.session_id);
      if (isAndroid) location.assign(data.auth_url);
    } catch (e) {
      if (hint) hint.textContent = '发起登录失败：' + e.message;
      toast('error', e.message);
      if (btn) { btn.disabled = false; btn.textContent = '登录华为云'; }
    }
  }

  function pollOauth(sessionId) {
    if (oauthPollTimer) clearInterval(oauthPollTimer);
    let tries = 0;

    oauthPollTimer = setInterval(async () => {
      tries += 1;
      if (tries > 100) {
        clearInterval(oauthPollTimer);
        oauthPollTimer = null;
        const hint = $('#oauthHint');
        const btn = $('#oauthLoginBtn');
        if (hint) hint.textContent = '登录超时，请重新点击登录';
        if (btn) { btn.disabled = false; btn.textContent = '登录华为云'; }
        return;
      }

      try {
        const res = await api('/api/oauth/poll', {
          method: 'POST',
          body: JSON.stringify({ session_id: sessionId }),
        });

        if (res.status === 'done') {
          clearInterval(oauthPollTimer);
          oauthPollTimer = null;
          const hint = $('#oauthHint');
          const btn = $('#oauthLoginBtn');
          const linkBtn = $('#oauthLinkBtn');
          if (hint) hint.textContent = '登录成功：' + (res.user_name || '');
          if (btn) { btn.disabled = false; btn.textContent = '登录华为云'; }
          if (linkBtn) linkBtn.hidden = true;
          toast('success', '登录成功');
          await loadStatus(false);
        } else if (res.status === 'error') {
          clearInterval(oauthPollTimer);
          oauthPollTimer = null;
          const hint = $('#oauthHint');
          const btn = $('#oauthLoginBtn');
          if (hint) hint.textContent = '登录失败：' + (res.error || '');
          if (btn) { btn.disabled = false; btn.textContent = '登录华为云'; }
          toast('error', res.error || '登录失败');
        }
      } catch (_) {
        // 轮询失败继续重试
      }
    }, 2000);
  }


  // ── 事件绑定 ────────────────────────────────────────────
  async function setRotationStrategy(strategy, picker) {
    const label = STRATEGY_TEXT[strategy] || strategy;
    const buttons = picker ? Array.from(picker.querySelectorAll('[data-strategy]')) : [];
    buttons.forEach((b) => { b.disabled = true; });
    try {
      const data = await api('/api/rotation', {
        method: 'POST',
        body: JSON.stringify({ strategy: strategy }),
      });
      toast('success', '轮换策略已切换为「' + (data.strategy_label || label) + '」');
      await loadStatus(false);
    } catch (e) {
      toast('error', '切换失败：' + e.message);
    } finally {
      buttons.forEach((b) => { b.disabled = false; });
    }
  }

  function bindEvents() {
    $$('.mx-nav-item, .mx-btm-item').forEach((el) => {
      el.addEventListener('click', () => {
        setRoute(el.dataset.route);
      });
    });

    const refreshBtn = $('#refreshBtn');
    if (refreshBtn) refreshBtn.addEventListener('click', refreshModels);

    const oauthBtn = $('#oauthLoginBtn');
    if (oauthBtn) oauthBtn.addEventListener('click', startOauthLogin);

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

    const runCkBtn = $('#runCheckinBtn');
    if (runCkBtn) runCkBtn.addEventListener('click', runCheckin);

    const quotaBtn = $('#refreshQuotaBtn');
    if (quotaBtn) quotaBtn.addEventListener('click', () => loadQuota(true));

    const rotationPicker = $('#rotationPicker');
    if (rotationPicker) {
      rotationPicker.addEventListener('click', (ev) => {
        const btn = ev.target.closest('[data-strategy]');
        if (!btn || btn.classList.contains('active')) return;
        setRotationStrategy(btn.dataset.strategy, rotationPicker);
      });
    }

    const rangePicker = $('#usageRange');
    if (rangePicker) {
      rangePicker.addEventListener('click', (ev) => {
        const btn = ev.target.closest('[data-hours]');
        if (!btn) return;
        state.usageHours = Number(btn.dataset.hours) || 24;
        rangePicker.querySelectorAll('[data-hours]').forEach((b) => {
          b.classList.toggle('active', b === btn);
        });
        loadUsage();
      });
    }

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
    setInterval(() => {
      if (!document.hidden) loadStatus(false);
    }, 20000);
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot);
  } else {
    boot();
  }
})();
