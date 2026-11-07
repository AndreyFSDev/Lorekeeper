// Diagnostic proxy between SillyTavern and the Cloufam LLM gateway.
//
// Sits on the request path and logs exactly what went to the gateway and what
// came back (content deltas, finish_reason, usage). The response is passed to the client
// byte-for-byte, so the setup works as usual.
//
// Run: node tools/lk-llm-proxy.mjs [--port 8010] [--target 11435] [--log <file>]

import { appendFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { createServer, request as httpRequest } from 'node:http';
import path from 'node:path';

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
};

const PORT = Number(arg('port', 8010));
const TARGET_PORT = Number(arg('target', 11435));
const ROOT = path.resolve(import.meta.dirname, '..');
const LOG = arg(
  'log',
  path.resolve(ROOT, '..', '..', 'research', 'cloufam-ui-check', 'proxy.log'),
);

mkdirSync(path.dirname(LOG), { recursive: true });
writeFileSync(LOG, `# прокси запущен ${new Date().toISOString()} → шлюз ${TARGET_PORT}\n`);

const log = (text) => appendFileSync(LOG, `${text}\n`);

/// Builds a readable summary from the Ollama NDJSON stream (already in OpenAI SSE format).
function describeStream(raw) {
  const deltas = [];
  let finish = null;
  const other = [];
  const chunkLog = [];
  for (const line of raw.split('\n')) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    if (!trimmed.startsWith('data:')) {
      chunkLog.push(`не-data: ${trimmed.slice(0, 120)}`);
      continue;
    }
    const payload = trimmed.slice(5).trim();
    if (payload === '[DONE]') {
      chunkLog.push('[DONE]');
      continue;
    }
    try {
      const parsed = JSON.parse(payload);
      const choice = parsed.choices?.[0];
      const content = choice?.delta?.content;
      const extraKeys = Object.keys(choice?.delta ?? {}).filter((k) => k !== 'content');
      chunkLog.push(
        `delta=${content === undefined ? '—' : content.length}${extraKeys.length ? ` +${extraKeys.join(',')}` : ''}` +
          `${choice?.finish_reason ? ` finish=${choice.finish_reason}` : ''}${parsed.error ? ` error=${JSON.stringify(parsed.error)}` : ''}`,
      );
      if (content) deltas.push(content);
      if (choice?.finish_reason) finish = choice.finish_reason;
      if (parsed.error) other.push(`error: ${JSON.stringify(parsed.error)}`);
    } catch {
      chunkLog.push(`неразобранная строка: ${trimmed.slice(0, 200)}`);
    }
  }
  return { content: deltas.join(''), finish, other, chunkLog };
}

const server = createServer((req, res) => {
  const chunks = [];
  req.on('data', (c) => chunks.push(c));
  req.on('end', () => {
    const body = Buffer.concat(chunks);
    const started = Date.now();
    const upstream = httpRequest(
      {
        host: '127.0.0.1',
        port: TARGET_PORT,
        path: req.url,
        method: req.method,
        headers: { ...req.headers, host: `127.0.0.1:${TARGET_PORT}` },
      },
      (up) => {
        res.writeHead(up.statusCode ?? 502, up.headers);
        const parts = [];
        up.on('data', (c) => {
          parts.push(c);
          res.write(c);
        });
        up.on('end', () => {
          res.end();
          let parsed = null;
          try {
            parsed = JSON.parse(body.toString('utf8'));
          } catch {
            parsed = null;
          }
          const raw = Buffer.concat(parts).toString('utf8');
          const summary = describeStream(raw);
          const messages = parsed?.messages ?? [];
          log(
            `\n=== ${new Date().toISOString()} ${req.method} ${req.url} → HTTP ${up.statusCode} за ${Date.now() - started} мс ===`,
          );
          log(
            `модель: ${parsed?.model ?? '—'}, stream=${parsed?.stream}, max_tokens=${parsed?.max_tokens ?? '—'}`,
          );
          log(`stop: ${JSON.stringify(parsed?.stop ?? [])}`);
          log(
            `сообщений: ${messages.length}, символов в промпте: ${messages.reduce((n, m) => n + String(m.content ?? '').length, 0)}`,
          );
          log(`--- роли и начало сообщений ---`);
          messages.forEach((m, i) => {
            const text = String(m.content ?? '').replace(/\s+/g, ' ');
            log(
              `  [${i}] ${m.role}${m.name ? ` (${m.name})` : ''} len=${String(m.content ?? '').length}: ${text.slice(0, 160)}`,
            );
          });
          log(`--- ответ ---`);
          log(
            `finish_reason: ${summary.finish ?? 'нет'}, символов контента: ${summary.content.length}`,
          );
          log(
            `чанков: ${summary.chunkLog.length}; последовательность: ${summary.chunkLog.slice(0, 40).join(' | ')}`,
          );
          log(`контент: ${summary.content.slice(0, 400).replace(/\s+/g, ' ')}`);
          if (summary.other.length) log(`прочее: ${summary.other.join(' | ')}`);
          if (summary.content.length === 0) {
            log(`ПУСТОЙ ОТВЕТ. Сырой хвост ответа: ${raw.slice(-600).replace(/\s+/g, ' ')}`);
          }
        });
      },
    );
    upstream.on('error', (err) => {
      log(`\n=== ОШИБКА апстрима: ${err.message} ===`);
      res.writeHead(502).end('proxy upstream error');
    });
    upstream.end(body);
  });
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`прокси слушает 127.0.0.1:${PORT} → 127.0.0.1:${TARGET_PORT}, лог: ${LOG}`);
});

