// Exit-hang diagnostics loop (smoke 36/39, 18.10.2026: "the app
// closed cleanly" and "no orphans" — yet the app did not exit within 30 s after WM_CLOSE,
// and ollama/node/python/llama-server stayed in the folder).
//
// Modes:
//   --phase boot      — WM_CLOSE right after boot readiness;
//   --phase settings  — call trainer_status (exactly what the «Дообучение» tab of the
//                       settings window does: probe python), wait for the
//                       reply, then WM_CLOSE.
// Measurement: time from WM_CLOSE until Cloufam.exe disappears; orphans in the
// install folder — 5 s after exit.
//
// Run (app closed):
//   LOREKEEPER_INSTALL=Z:\AImodels\Cloufam node tools/lk-exit-hang-loop.mjs --phase settings
// Exit 0 — exited within 15 s and no orphans; 1 — otherwise.
import { execSync, spawn } from 'node:child_process';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const INSTALL = process.env.LOREKEEPER_INSTALL ?? 'Z:\\AImodels\\Cloufam';
process.env.LOREKEEPER_INSTALL = INSTALL;
const ws = loadWs(path.resolve(import.meta.dirname, '..'));
const PHASE = process.argv.includes('--phase')
  ? process.argv[process.argv.indexOf('--phase') + 1]
  : 'boot';
const CDP_PORT = 9225;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const running = () => {
  try {
    return execSync('tasklist /FI "IMAGENAME eq Cloufam.exe" /NH', {
      encoding: 'utf8',
    }).includes('Cloufam.exe');
  } catch {
    return false;
  }
};

async function attachMain(timeoutMs = 120000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    let targets = [];
    try {
      targets = await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
    } catch {
      // CDP has not come up yet
    }
    const t = targets.find((x) => x.type === 'page' && x.url.startsWith('http://tauri.localhost'));
    if (t) {
      const socket = new ws(t.webSocketDebuggerUrl);
      await new Promise((resolve, reject) => {
        socket.once('open', resolve);
        socket.once('error', reject);
      });
      let id = 0;
      const pending = new Map();
      socket.on('message', (raw) => {
        const msg = JSON.parse(raw.toString());
        const entry = pending.get(msg.id);
        if (entry) {
          pending.delete(msg.id);
          entry(msg);
        }
      });
      const send = (method, params) =>
        new Promise((resolve) => {
          const i = ++id;
          pending.set(i, resolve);
          socket.send(JSON.stringify({ id: i, method, params }));
        });
      await send('Runtime.enable', {});
      const evalJs = async (expression) => {
        const res = await send('Runtime.evaluate', {
          expression,
          awaitPromise: true,
          returnByValue: true,
        });
        const details = res.result?.exceptionDetails;
        if (details) throw new Error(details.exception?.description || JSON.stringify(details));
        return res.result?.result?.value;
      };
      return { socket, evalJs };
    }
    await sleep(1000);
  }
  throw new Error('главное окно не появилось в CDP');
}

const exe = path.join(INSTALL, 'Cloufam.exe');
const app = spawn(exe, [], {
  cwd: INSTALL,
  detached: true,
  stdio: 'ignore',
  env: {
    ...process.env,
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${CDP_PORT}`,
  },
});
app.unref();
console.log(`запущен ${exe}, фаза: ${PHASE}`);

// 1. Boot readiness — from the main window's boot_state (not from the log text).
let booted = false;
try {
  const main = await attachMain();
  const deadline = Date.now() + 240000;
  while (Date.now() < deadline) {
    const s = await main
      .evalJs(
        `window.__TAURI_INTERNALS__.invoke('boot_state').then((s) => (s.ready || s.error) ? s : null)`,
      )
      .catch(() => null);
    if (s) {
      booted = Boolean(s.ready);
      console.log(`подъём: готов (порт ST ${s.port}${s.error ? `, ошибка: ${s.error}` : ''})`);
      break;
    }
    await sleep(1000);
  }
  if (PHASE === 'settings') {
    // The «Дообучение» tab of the settings window calls trainer_status; the probe starts
    // python (torch import) — and that was exactly what stayed alive when the smoke failed.
    const startedAt = Date.now();
    const status = await main
      .evalJs(
        `window.__TAURI_INTERNALS__.invoke('trainer_status').then((s) => ({ available: s.available, reason: s.reason ?? null })).catch((e) => ({ err: String(e) }))`,
      )
      .catch((e) => ({ err: String(e) }));
    console.log(`trainer_status: ${JSON.stringify(status)} за ${Date.now() - startedAt} мс`);
    await sleep(3000);
  }
  main.socket.close();
} catch (e) {
  console.log(`CDP: ${e}`);
}
if (!booted) console.log('подъём: НЕ дождались (продолжаем к закрытию)');

// 2. WM_CLOSE and exit-time measurement.
const t0 = Date.now();
try {
  execSync('taskkill /IM Cloufam.exe', { stdio: 'ignore' });
} catch {
  // already closed
}
let exited = false;
while (Date.now() - t0 < 45000) {
  if (!running()) {
    exited = true;
    break;
  }
  await sleep(1000);
}
const closeMs = Date.now() - t0;
console.log(`выход: ${exited ? `за ${closeMs} мс` : 'НЕ ВЫШЕЛ за 45 с'}`);

// 3. Orphans from the install folder.
await sleep(5000);
let orphans = '';
try {
  orphans = execSync(
    `powershell -NoProfile -Command "Get-CimInstance Win32_Process | Where-Object { $_.ExecutablePath -like '${INSTALL.replace(/'/g, "''")}\\*' } | ForEach-Object { $_.Name + ' ' + $_.ProcessId }"`,
    { encoding: 'utf8' },
  ).trim();
} catch {
  /* noop */
}
console.log(`сироты: ${orphans === '' ? 'нет' : orphans.replace(/\s+/g, ' ')}`);

const ok = exited && closeMs <= 15000 && orphans === '';
console.log(`вердикт: ${ok ? 'OK' : 'FAIL'}`);
process.exit(ok ? 0 : 1);

