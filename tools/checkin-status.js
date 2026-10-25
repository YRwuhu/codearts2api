/* 查询签到状态（不领取）。复用登录后的 profile，输出一行 JSON。 */
const { chromium } = require('playwright');
const path = require('path');

const PROFILE = process.env.CK_PROFILE || path.join(__dirname, 'profile');
const HOME = 'https://devcloud.cn-north-4.huaweicloud.com/chat/home';

function emit(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
  process.exit(obj.ok ? 0 : 1);
}

(async () => {
  let ctx;
  try {
    ctx = await chromium.launchPersistentContext(PROFILE, {
      headless: true,
      args: ['--no-sandbox', '--disable-blink-features=AutomationControlled'],
    });
  } catch (e) {
    emit({ ok: false, message: '无法启动浏览器: ' + e.message, needs_login: true });
    return;
  }

  try {
    const page = ctx.pages()[0] || await ctx.newPage();
    await page.goto(HOME, { waitUntil: 'domcontentloaded', timeout: 60000 });
    await page.waitForTimeout(2500);

    const loggedIn = await page.evaluate(() => {
      const txt = document.body.innerText || '';
      return !txt.includes('登录华为云') && !txt.includes('登录参与');
    });
    if (!loggedIn) {
      await ctx.close();
      emit({ ok: false, message: '尚未登录，请先在面板点「打开登录窗口」', needs_login: true });
      return;
    }

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
      const H = {
        Accept: 'application/json, text/plain, */*',
        'X-Requested-With': 'XMLHttpRequest',
        'Agent-Type': 'PromptCenter',
        'X-Language': 'zh-cn',
        cftk,
      };

      try {
        const r = await fetch('/chat/PromptCenterService/v1/ops/delivery?channel=WEB&_=' + Date.now(), {
          credentials: 'same-origin',
          headers: H,
        });
        const ct = r.headers.get('content-type') || '';
        if (ct.includes('text/html')) return { ok: false, message: '登录已失效', needs_login: true };
        const text = await r.text();
        if (!text.trim()) return { ok: false, message: '服务端返回空响应', needs_login: true };
        const j = JSON.parse(text);
        if (j.code !== 0) return { ok: false, message: j.message || ('code=' + j.code) };

        const items = (j.data && j.data.items) || [];
        const activities = items.map((x) => ({
          campaignId: x.campaignId,
          type: x.type,
          title: x.title,
          benefitAmount: x.benefitAmount,
          status: x.status,
          claimable: x.claimable,
        }));
        const daily = activities.find((x) => x.type === 'USER_LOGIN' || x.type === 'DAILY_CLAIM');
        const claimed = daily
          ? ['CLAIMED', 'CONFIRMED', 'CONSUME'].includes(daily.status)
          : false;

        return {
          ok: true,
          claimed_today: claimed,
          message: claimed ? '今日已签到' : '今日未签到',
          activities,
        };
      } catch (e) {
        return { ok: false, message: String((e && e.message) || e) };
      }
    });

    await ctx.close();
    emit(result);
  } catch (e) {
    try { await ctx.close(); } catch (_) {}
    emit({ ok: false, message: '执行异常: ' + e.message });
  }
})();
