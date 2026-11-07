// Narrow loop to diagnose the "Director mode" toggle (smoke 35/39, 17.10.2026:
// "director mode turns on/off with the button" fail, and the aside therefore
// goes out as a user message instead of narrator).
//
// What it measures: whether the `cloufam-director-nav-btn` button exists, what happens
// on click (exception?), and in how many milliseconds the flag
// `extensionSettings.cloufam_director.enabled` flips (or does not
// flip at all). Separates "broken" (NEVER) from "race" (flip > 500 ms).
//
// Start (app closed, CDP port free):
//   LOREKEEPER_INSTALL=Z:\AImodels\Cloufam node tools/lk-director-toggle-loop.mjs
// Exit 0 — both flips are fast (≤2 s); 1 — otherwise (loop is red).
import { spawn } from 'node:child_process';
import path from 'node:path';
import { loadWs } from './ws-resolve.mjs';

const INSTALL = process.env.LOREKEEPER_INSTALL ?? 'Z:\\AImodels\\Cloufam';
process.env.LOREKEEPER_INSTALL = INSTALL;
const ws = loadWs(path.resolve(import.meta.dirname, '..'));
const CDP_PORT = Number(
  process.argv.includes('--port') ? process.argv[process.argv.indexOf('--port') + 1] : 9224,
);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function targets() {
  try {
    return await (await fetch(`http://127.0.0.1:${CDP_PORT}/json/list`)).json();
  } catch {
    return [];
  }
}

async function attachSt(timeoutMs = 120000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    for (const t of await targets()) {
      if (t.type !== 'page') continue;
      const socket = new ws(t.webSocketDebuggerUrl);
      try {
        await new Promise((resolve, reject) => {
          socket.once('open', resolve);
          socket.once('error', reject);
        });
      } catch {
        continue;
      }
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
      try {
        if (await evalJs(`!!document.getElementById('send_textarea')`)) return { socket, evalJs };
      } catch {
        // wrong page
      }
      socket.close();
    }
    await sleep(1000);
  }
  throw new Error('страница SillyTavern не найдена за отведённое время');
}

async function pollEnabled(evalJs, want, timeoutMs = 20000) {
  const t0 = Date.now();
  while (Date.now() - t0 < timeoutMs) {
    try {
      const v = await evalJs(
        `!!SillyTavern.getContext().extensionSettings.cloufam_director?.enabled`,
      );
      if (v === want) return Date.now() - t0;
    } catch {
      // context not ready yet
    }
    await sleep(250);
  }
  return null;
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
console.log(`запущен ${exe}`);

const out = { probes: [] };
try {
  // 1. Main window: wait for startup to become ready and learn the ST port (like lk-audit-smoke).
  let main = null;
  const deadlineMain = Date.now() + 90000;
  while (!main && Date.now() < deadlineMain) {
    const t = (await targets()).find(
      (x) =>
        x.type === 'page' && x.url.startsWith('http://tauri.localhost') && !x.url.includes('#'),
    );
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
      main = {
        socket,
        evalJs: async (expression) => {
          const res = await send('Runtime.evaluate', {
            expression,
            awaitPromise: true,
            returnByValue: true,
          });
          const details = res.result?.exceptionDetails;
          if (details) throw new Error(details.exception?.description || JSON.stringify(details));
          return res.result?.result?.value;
        },
      };
    } else {
      await sleep(1000);
    }
  }
  if (!main) throw new Error('главное окно не появилось в CDP');
  let boot = null;
  const deadlineBoot = Date.now() + 240000;
  while (Date.now() < deadlineBoot) {
    boot = await main
      .evalJs(
        `window.__TAURI_INTERNALS__.invoke('boot_state').then((s) => (s.ready || s.error) ? s : null)`,
      )
      .catch(() => null);
    if (boot) break;
    await sleep(1000);
  }
  if (!boot?.ready) throw new Error(`подъём не завершился: ${JSON.stringify(boot)}`);
  out.stPort = boot.port;
  await main.evalJs(
    `window.__TAURI_INTERNALS__.invoke('mark_st_opened').then(() => { location.href = 'http://127.0.0.1:${boot.port}/'; return true; })`,
  );

  const st = await attachSt();
  // Wait for the extension button to appear — up to 60 s after page load.
  const btnReady = await (async () => {
    const deadline = Date.now() + 60000;
    while (Date.now() < deadline) {
      const r = await st
        .evalJs(`(() => {
          const b = document.getElementById('cloufam-director-nav-btn');
          return b ? { exists: true, tag: b.tagName, text: (b.textContent || '').slice(0, 40) } : { exists: false };
        })()`)
        .catch((e) => ({ exists: false, err: String(e) }));
      if (r.exists) return r;
      await sleep(1000);
    }
    return { exists: false };
  })();
  out.probes.push({ probe: 'button', ...btnReady });

  out.probes.push({
    probe: 'settings-before',
    ...(await st
      .evalJs(`(() => {
        const d = SillyTavern.getContext().extensionSettings.cloufam_director;
        return d ? { keys: Object.keys(d), enabled: !!d.enabled } : { keys: null };
      })()`)
      .catch((e) => ({ err: String(e) }))),
  });

  // The click enables the mode; record the exception and the flag latency.
  out.probes.push({
    probe: 'click-on',
    ...(await st
      .evalJs(`(() => {
        try { document.getElementById('cloufam-director-nav-btn').click(); return { clicked: true }; }
        catch (e) { return { clicked: false, err: String(e) }; }
      })()`)
      .catch((e) => ({ clicked: false, err: String(e) }))),
  });
  const onMs = await pollEnabled(st.evalJs, true);
  out.probes.push({
    probe: 'flip-on',
    ms: onMs,
    verdict: onMs === null ? 'NEVER' : onMs <= 2000 ? 'fast' : 'late',
  });

  // Disabling.
  out.probes.push({
    probe: 'click-off',
    ...(await st
      .evalJs(`(() => {
        try { document.getElementById('cloufam-director-nav-btn').click(); return { clicked: true }; }
        catch (e) { return { clicked: false, err: String(e) }; }
      })()`)
      .catch((e) => ({ clicked: false, err: String(e) }))),
  });
  const offMs = await pollEnabled(st.evalJs, false);
  out.probes.push({
    probe: 'flip-off',
    ms: offMs,
    verdict: offMs === null ? 'NEVER' : offMs <= 2000 ? 'fast' : 'late',
  });

  st.socket.close();
} catch (e) {
  out.fatal = String(e);
} finally {
  spawn('taskkill', ['/F', '/IM', 'Cloufam.exe'], { stdio: 'ignore' });
}

console.log(JSON.stringify(out, null, 2));
const flips = out.probes.filter((p) => p.probe.startsWith('flip'));
const ok = flips.length === 2 && flips.every((p) => p.verdict === 'fast');
process.exit(ok ? 0 : 1);

