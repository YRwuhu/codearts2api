(async () => {
  const BASE = 'https://devcloud.cn-north-4.huaweicloud.com';
  const log = (...a) => console.log('%c[CodeArts 签到]', 'color:#3482FF;font-weight:bold', ...a);

  function readCookie(name) {
    const m = document.cookie.match(new RegExp('(^|;\\s*)' + name + '=([^;]*)'));
    return m ? decodeURIComponent(m[2]) : '';
  }
  const cftk = readCookie('cftk');

  const headers = {
    'Agent-Type': 'PromptCenter',
    'X-Language': 'zh-cn',
    'Accept': 'application/json, text/plain, */*',
  };
  if (cftk) headers['cftk'] = cftk;

  function pick(j) {
    let node = j;
    for (let i = 0; i < 4; i++) {
      if (node && node.data && (Array.isArray(node.data) || typeof node.data === 'object')) { node = node.data; continue; }
      if (node && node.result && (Array.isArray(node.result) || typeof node.result === 'object')) { node = node.result; continue; }
      break;
    }
    const list = Array.isArray(node) ? node
      : (node && Array.isArray(node.activities)) ? node.activities
      : (node && Array.isArray(node.list)) ? node.list : [];
    return list.map((x) => ({
      campaignId: String(x.campaignId ?? x.id ?? ''),
      type: String(x.type || ''),
      title: x.title || '',
      status: x.status || '',
      claimed: !!(x.claimed) || ['CLAIMED','CONFIRM','CONSUME'].includes(x.status),
      amount: x.benefitAmount ?? x.pendingTotalAmount ?? null,
      claimable: x.claimable !== false,
    })).filter((x) => x.campaignId);
  }

  const isDaily = (a) => {
    const t = a.type.toLowerCase();
    return t === 'daily_claim' || t === 'user_login' || t === 'login';
  };

  async function list() {
    const r = await fetch(BASE + '/PromptCenterService/v1/ops/delivery?channel=WEB', {
      credentials: 'same-origin', headers,
    });
    const ct = r.headers.get('content-type') || '';
    if (ct.includes('text/html')) throw new Error('未登录或登录已过期，请先在本页完成华为云登录');
    const j = await r.json();
    if (j.error) throw new Error(j.error);
    return pick(j);
  }

  async function claim(id) {
    const r = await fetch(BASE + '/PromptCenterService/v1/ops/claim', {
      method: 'POST', credentials: 'same-origin',
      headers: { ...headers, 'Content-Type': 'application/json' },
      body: JSON.stringify({ campaignId: id, idempotentKey: id + '_' + Date.now(), channel: 'WEB' }),
    });
    const ct = r.headers.get('content-type') || '';
    const text = await r.text();
    if (ct.includes('text/html')) throw new Error('登录已过期');
    let j = {};
    try { j = JSON.parse(text); } catch {}
    if (j.error) throw new Error(String(j.error).includes('csrf') ? 'CSRF 校验失败，请刷新页面后重试' : j.error);
    if (!((j.code === 0 || j.code === 200) || j.data != null)) {
      throw new Error(j.message || '领取失败');
    }
    return j;
  }

  async function confirm(id) {
    try {
      await fetch(BASE + '/PromptCenterService/v1/ops/confirm', {
        method: 'POST', credentials: 'same-origin',
        headers: { ...headers, 'Content-Type': 'application/json' },
        body: JSON.stringify({ campaignId: id }),
      });
    } catch (_) {}
  }

  const out = { ok: false, message: '', campaign_id: null, benefit_amount: null };
  try {
    log('读取活动列表…');
    const acts = await list();
    log('共 ' + acts.length + ' 个活动');
    const daily = acts.filter(isDaily);
    console.table(daily.map(a => ({
      活动: a.title, 类型: a.type, 状态: a.status, 已领: a.claimed, 奖励: a.amount,
    })));

    if (!daily.length) throw new Error('未找到每日签到活动');
    const pending = daily.filter(a => !a.claimed && a.claimable);
    if (!pending.length) {
      out.ok = true;
      out.message = '今日已签到';
      log('今日已签到，无需重复领取');
    } else {
      const t = pending[0];
      out.campaign_id = t.campaignId;
      out.benefit_amount = t.amount;
      log('领取中：' + (t.title || t.campaignId));
      await claim(t.campaignId);
      await confirm(t.campaignId);
      out.ok = true;
      out.message = t.amount ? ('签到成功，获得 ' + t.amount + ' 积分') : '签到成功';
      log('✅ ' + out.message);
    }
  } catch (e) {
    out.message = String((e && e.message) || e);
    log('❌ ' + out.message);
  }

  const json = JSON.stringify(out);
  console.log('%c↓↓↓ 复制下面这一行，粘贴到 CodeArts2API 的「签到」页 ↓↓↓', 'color:#34D399;font-weight:bold');
  console.log(json);
  try { await navigator.clipboard.writeText(json); console.log('（已自动复制到剪贴板）'); } catch (_) {}
  return out;
})();
