// 客服示例：外部系统推进来的工单 + 聊天里的多轮追问 + 主动通知。
//
// 三条能力的分工：
// * onWebhook：外部系统 POST /hook/<botId>/tickets，密钥在环境变量
//   HBOT_TICKET_SECRET 里（清单只写变量名）。返回值就是 HTTP 应答。
// * ctx.session.prompt：**注册**一条等待并立刻返回；用户下一条消息交给
//   onResume，而不是让 onMessage 再猜一次他想说什么。
// * ctx.notify：收件人来自清单，不来自调用点 —— 一段被注入的代码没法拿它外发。

import { defineBot, reply } from 'hbot';

const TICKET_KEY = 'ticket';
export default defineBot({
  // 聊天里发 /ticket：先问一句，等用户回。
  async onMessage(ctx) {
    const text = (ctx.text ?? '').trim();
    if (text !== '/ticket') {
      reply('发 /ticket 提交一个问题，我会追问细节。');
      return;
    }
    await ctx.session.prompt(
      TICKET_KEY,
      '请一句话描述你遇到的问题（5 分钟内回复有效）。',
      { timeoutMs: 300_000 },
    );
  },

  // 用户回了那条消息。ctx 里带 waitKey / waitPrompt 与那条消息的平铺字段。
  async onResume(ctx) {
    if (ctx.waitKey !== TICKET_KEY) {
      reply('这条等待不认识了：' + ctx.waitKey);
      return;
    }
    const body = (ctx.text ?? '').trim();
    if (body.length === 0) {
      reply('没读到内容，已取消。');
      return;
    }
    const saved = await ctx.sql.exec(
      'INSERT INTO tickets (source, contact, body, created_at) VALUES (?, ?, ?, ?)',
      ['chat', ctx.senderId, body, Date.now()],
    );
    ctx.log('工单已登记，affected=' + saved.affected);
    reply('已记录（编号 ' + Date.now() + '）。有进展我会私聊你。');
  },

  // 外部系统打进来。返回值就是 HTTP 应答：对象按 JSON 发回。
  async onWebhook(ctx) {
    if (ctx.route !== 'tickets') {
      return { status: 404, body: { error: 'unknown route' } };
    }
    const payload = ctx.bodyJson;
    if (payload === undefined || typeof payload !== 'object' || payload === null) {
      return { status: 400, body: { error: 'body must be a JSON object' } };
    }
    const record = payload as { contact?: unknown; body?: unknown };
    const contact = typeof record.contact === 'string' ? record.contact : 'unknown';
    const text = typeof record.body === 'string' ? record.body.trim() : '';
    if (text.length === 0 || text.length > 2000) {
      return { status: 400, body: { error: 'body must be 1..2000 chars' } };
    }

    await ctx.sql.exec(
      'INSERT INTO tickets (source, contact, body, created_at) VALUES (?, ?, ?, ?)',
      ['webhook', contact, text, Date.now()],
    );
    try {
      await ctx.notify('新工单（' + contact + '）：' + text);
    } catch (error) {
      // 通知失败不该让工单失败：已经落库了，只是没提醒到人。
      ctx.log('通知失败：' + error.message);
    }
    return { status: 201, body: { ok: true } };
  },
});
