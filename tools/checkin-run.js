/* 无头签到：复用 pw_login.js 保存的浏览器 profile，完成每日签到。
 *
 * 输出一行 JSON 到 stdout，供 Rust 后端解析。
 * 退出码 0 = 执行成功（无论今日是否已签），1 = 失败。
 */
const { chromium } = require('playwright');
const path = require('path');

const PROFILE = process.env.CK_PROFILE || path.join(__dirname, 'profile');
const HOME = 'https://devcloud.cn-north-4.huaweicloud.com/chat/home';

function emit(obj, code) {
  process.stdout.write(JSON.stringify(obj) + '\n');
  process.exit(code || 0);
}

(async () => {
  let ctx;
  try {
    ctx = await chromium.launchPersistentContext(PROFILE, {
      headless: true,
      args: ['--no-sandbox', '--disable-blink-features=AutomationControlled'],
    });
  } catch (e) {
    emit({ ok: false, message: '无法启动浏览器: ' + e.message, needs_login: true }, 1);
    return;
  }

  try {
    const page = ctx.pages()[0] || await ctx.newPage();
    await page.goto(HOME, { waitUntil: 'domcontentloaded', timeout: 60000 });
    await page.waitForTimeout(2500);

    // 1) 确认登录态
    const loggedIn = await page.evaluate(() => {
      const txt = document.body.innerText || '';
      return !txt.includes('登录华为云') && !txt.includes('登录参与');
    });
    if (!loggedIn) {
      await ctx.close();
      emit({ ok: false, message: '登录已过期，请在面板重新登录', needs_login: true }, 1);
      return;
    }

    // 2) 读取活动并签到
    const result = await page.evaluate(async () => {
      const readCookie = (name) => {
        const parts = document.cookie ? document.cookie.split(';') : [];
        for (const p of parts) {
          const i = p.indexOf('=');
          if (i === -1) continue;
          if (p.slice(0, i).trim() === name) return decodeURIComponent(p.slice(i + 1));
        }
        return '';
      };
      const cftk = readCookie('devclouddevuibjtcftk') || readCookie('cftk');
      if (!cftk) return { ok: false, message: '未找到 cftk', needs_login: true };

      const H = {
        Accept: 'application/json, text/plain, */*',
        'X-Requested-With': 'XMLHttpRequest',
        'Agent-Type': 'PromptCenter',
        'X-Language': 'zh-cn',
        cftk,
      };

      const out = { ok: false, message: '', claimed_today: false, activities: [] };

      try {
        const r = await fetch('/chat/PromptCenterService/v1/ops/delivery?channel=WEB&_=' + Date.now(), {
          credentials: 'same-origin',
          headers: H,
        });
        const ct = r.headers.get('content-type') || '';
        if (ct.includes('text/html')) return { ok: false, message: '登录已失效', needs_login: true };
        const text = await r.text();
        if (!text.trim()) return { ok: false, message: '服务端返回空响应（可能登录态无效）', needs_login: true };
        const j = JSON.parse(text);
        if (j.code !== 0) return { ok: false, message: j.message || ('code=' + j.code) };

        const items = (j.data && j.data.items) || [];
        out.activities = items.map((x) => ({
          campaignId: x.campaignId,
          type: x.type,
          title: x.title,
          benefitAmount: x.benefitAmount,
          status: x.status,
          claimable: x.claimable,
        }));

        const daily = items.find((x) => x.type === 'USER_LOGIN' || x.type === 'DAILY_CLAIM');
        if (!daily) return { ...out, ok: false, message: '未找到每日签到活动' };

        if (['CLAIMED', 'CONFIRMED', 'CONSUME'].includes(daily.status)) {
          return { ...out, ok: true, claimed_today: true, message: '今日已签到（' + daily.status + '）' };
        }

        // claim
        const cr = await fetch('/chat/PromptCenterService/v1/ops/claim', {
          method: 'POST',
          credentials: 'same-origin',
          headers: { ...H, 'Content-Type': 'application/json' },
          body: JSON.stringify({
            campaignId: daily.campaignId,
            idempotentKey: daily.campaignId + '_' + Date.now(),
            channel: 'WEB',
          }),
        });
        const ctext = await cr.text();
        if (!ctext.trim()) return { ...out, ok: false, message: '领取返回空响应' };
        const cj = JSON.parse(ctext);

        if (cj.code !== 0) {
          const msg = String(cj.message || '');
          const benign = msg.includes('尚未到达') || msg.includes('已领取');
          return {
            ...out,
            ok: benign,
            claimed_today: benign,
            message: benign ? '今日已签到（' + msg + '）' : ('领取失败: ' + (msg || cj.code)),
          };
        }

        // confirm
        try {
          await fetch('/chat/PromptCenterService/v1/ops/confirm', {
            method: 'POST',
            credentials: 'same-origin',
            headers: { ...H, 'Content-Type': 'application/json' },
            body: JSON.stringify({ campaignId: daily.campaignId }),
          });
        } catch (_) {}

        return {
          ...out,
          ok: true,
          claimed_today: true,
          message: daily.benefitAmount ? ('签到成功，获得 ' + daily.benefitAmount + ' 积分') : '签到成功',
        };
      } catch (e) {
        return { ...out, ok: false, message: String((e && e.message) || e) };
      }
    });

    await ctx.close();
    emit(result, result.ok ? 0 : 1);
  } catch (e) {
    try { await ctx.close(); } catch (_) {}
    emit({ ok: false, message: '执行异常: ' + e.message }, 1);
  }
})();
