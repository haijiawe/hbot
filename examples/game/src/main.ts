// 游戏示例：每人一条最高分，外加每天的公告与清零。
//
// 三个要点：
// 1. 表结构来自 migrations/*.sql，运行时不许 DDL（会 HBOT_SQL_DENIED）。
// 2. 参数一律走占位符，拼字符串会被拒（「只接受参数化语句」）。
// 3. onSchedule 的 ctx 里**没有** ctx.sql：定时器没有 bot 身份。要动数据
//    就让它回复一句话，由下一条消息那条路径去写 —— 或者像这里一样只用 reply。

import { defineBot, reply } from 'hbot';

const TOP_LIMIT = 5;

export default defineBot({
  async onMessage(ctx) {
    const text = (ctx.text ?? '').trim();
    if (!text.startsWith('/')) return;

    const [command, argument] = text.slice(1).split(/\s+/, 2);

    if (command === 'score') {
      const value = Number(argument);
      if (!Number.isInteger(value) || value < 0) {
        reply('用法：/score 120（非负整数）');
        return;
      }
      // 只在更高时覆盖：一条 UPDATE + 一条 INSERT，两条都在事务外也要能重入。
      const changed = await ctx.sql.exec(
        'UPDATE scores SET best = ?, updated_at = ? WHERE sender_id = ? AND best < ?',
        [value, Date.now(), ctx.senderId, value],
      );
      if (changed.affected === 0) {
        await ctx.sql.exec(
          'INSERT OR IGNORE INTO scores (sender_id, best, updated_at) VALUES (?, ?, ?)',
          [ctx.senderId, value, Date.now()],
        );
      }
      reply('记下了：' + value);
      return;
    }

    if (command === 'top') {
      const rows = await ctx.sql.query(
        'SELECT sender_id, best FROM scores ORDER BY best DESC LIMIT ?',
        [TOP_LIMIT],
      );
      if (rows.rows.length === 0) {
        reply('还没有人上榜，发 /score 120 试试。');
        return;
      }
      const lines = rows.rows.map((row, index) => (index + 1) + '. ' + row[0] + ' — ' + row[1]);
      reply(lines.join('\n'));
      return;
    }

    if (command === 'me') {
      const rows = await ctx.sql.query('SELECT best FROM scores WHERE sender_id = ?', [ctx.senderId]);
      reply(rows.rows.length === 0 ? '你还没有成绩。' : '你的最高分：' + rows.rows[0][0]);
      return;
    }

    reply('可用命令：/score <数字>、/top、/me');
  },

  // 每天 0 点（UTC+8）说一句话。ctx 里只有 name / reply / log / sleep ——
  // 没有 ctx.sql、没有入站字段：计划背后没有会话，也没有 bot 身份。
  async onSchedule(ctx, name) {
    if (name !== 'daily-reset') return;
    ctx.log('每日结算：' + new Date().toISOString());
    ctx.reply('新的一天，榜单继续累计（发 /top 看排名）。');
  },
});
