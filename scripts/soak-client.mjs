#!/usr/bin/env node
// soak 客户端：连上 `hbot serve` 的 WebSocket，按固定间隔注入消息并量往返延迟。
//
// 为什么是独立的小脚本而不是塞进 soak-serve.ps1：PowerShell 手写 WS 帧太长、错一次
// 就变成「协议 bug 还是脚本 bug」两难；Node 24 自带全局 WebSocket，这里只做三件事：
// 认证 → 注入 → 等 `run.log` 事件（用事件时间算往返，不轮询日志表）。
//
// 用法：
//   node scripts/soak-client.mjs --address ws://127.0.0.1:PORT --token TOK \
//     --project PATH --bot UUID --minutes 5 --interval 5 --out result.json

import { appendFileSync, writeFileSync } from 'node:fs';

function arg(name, fallback) {
  const index = process.argv.indexOf('--' + name);
  return index === -1 ? fallback : process.argv[index + 1];
}

const address = arg('address');
const token = arg('token');
const project = arg('project');
const bot = arg('bot');
const minutes = Number(arg('minutes', '5'));
const intervalSec = Number(arg('interval', '5'));
const timeoutMs = Number(arg('timeout', '10000'));
const out = arg('out', '');
const marker = arg('marker', 'soak');

if (!address || !token || !project || !bot) {
  console.error('缺参数：需要 --address --token --project --bot');
  process.exit(2);
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

let nextId = 1;
const pending = new Map();
const listeners = new Set();
let clientErrors = 0;
let errorLines = 0;

const socket = new WebSocket(address);
await new Promise((resolve, reject) => {
  socket.addEventListener('open', resolve, { once: true });
  socket.addEventListener('error', () => reject(new Error('WebSocket 连接失败：' + address)), { once: true });
});

socket.addEventListener('message', (event) => {
  let message;
  try { message = JSON.parse(event.data); }
  catch { clientErrors += 1; return; }
  if (message.method === 'event') {
    const payload = message.params;
    if (payload && payload.type === 'run.log' && payload.level === 'error') errorLines += 1;
    for (const listener of listeners) listener(payload);
    return;
  }
  const waiter = pending.get(message.id);
  if (!waiter) return;
  pending.delete(message.id);
  if (message.error) {
    const error = new Error(message.error.message);
    error.code = message.error.code;
    waiter.reject(error);
  } else {
    waiter.resolve(message.result);
  }
});

function rpc(method, params) {
  const id = nextId;
  nextId += 1;
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
    socket.send(JSON.stringify({ jsonrpc: '2.0', id, method, params }));
  });
}

const dispatch = (command) => rpc('dispatch', { command });

await rpc('auth', { token });
console.error('[soak] 已认证');

await dispatch({ type: 'project_open', path: project });
console.error('[soak] 项目已打开：' + project);
await dispatch({ type: 'bot_start', botId: bot });
console.error('[soak] bot 已启动');

let conversationId = '';
for (let attempt = 0; attempt < 120 && !conversationId; attempt += 1) {
  const list = await dispatch({ type: 'bot_conversations', botId: bot });
  const rows = Array.isArray(list) ? list : [];
  const local = rows.find((row) => row.externalId === 'local:default') || rows[0];
  if (local) conversationId = local.conversationId;
  else await sleep(250);
}
if (!conversationId) {
  console.error('[soak] 拿不到会话 id：local 平台应当有一条 externalId=local:default 的行');
  process.exit(3);
}
console.error('[soak] 会话：' + conversationId);

const samples = [];
const latencies = [];
let sent = 0;
let replied = 0;
let timedOut = 0;
let sendFailed = 0;
const startedAt = Date.now();
const deadline = startedAt + minutes * 60 * 1000;

while (Date.now() < deadline) {
  const seq = sent;
  const text = marker + '-' + seq;
  const began = Date.now();
  let done = false;
  const waiter = (payload) => {
    if (done || !payload) return;
    if (payload.type !== 'run.log') return;
    if (payload.botId !== bot) return;
    if (!String(payload.line).includes(text)) return;
    done = true;
    const elapsed = Date.now() - began;
    latencies.push(elapsed);
    replied += 1;
    samples.push({ at: Date.now(), seq, latencyMs: elapsed });
  };
  listeners.add(waiter);
  try {
    await dispatch({ type: 'bot_inject', botId: bot, conversationId, text });
    sent += 1;
  } catch (error) {
    sendFailed += 1;
    console.error('[soak] 注入失败：' + error.message);
  }
  const until = began + timeoutMs;
  while (!done && Date.now() < until) await sleep(20);
  listeners.delete(waiter);
  if (!done) {
    timedOut += 1;
    samples.push({ at: Date.now(), seq, latencyMs: null });
  }
  const rest = intervalSec * 1000 - (Date.now() - began);
  if (rest > 0) await sleep(rest);
  if (sent % 12 === 0 && !done) console.error('[soak] 已注入 ' + sent + ' 条');
}

function percentile(values, fraction) {
  if (values.length === 0) return null;
  const sorted = [...values].sort((a, b) => a - b);
  const index = Math.min(sorted.length - 1, Math.max(0, Math.ceil(fraction * sorted.length) - 1));
  return sorted[index];
}

const summary = {
  minutes,
  intervalSec,
  sent,
  replied,
  timedOut,
  sendFailed,
  errorLines,
  clientErrors,
  elapsedMs: Date.now() - startedAt,
  p50: percentile(latencies, 0.5),
  p95: percentile(latencies, 0.95),
  max: latencies.length === 0 ? null : Math.max(...latencies),
  min: latencies.length === 0 ? null : Math.min(...latencies),
  samples,
};

if (out) writeFileSync(out, JSON.stringify(summary, null, 2), 'utf8');
console.log(JSON.stringify({ ...summary, samples: undefined }));
socket.close();
process.exit(0);