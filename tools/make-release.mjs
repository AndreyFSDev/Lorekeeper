// Builds the distribution package: a portable Cloufam.exe with a signature and
// an update manifest that the installed app looks at. There is no installer:
// the program lives in a folder next to the exe, and self-update replaces only the exe.
// Usage: node tools/make-release.mjs ["release notes text"]
//
// This script is where learned mistakes live: each check below once
// saved (or would have saved) a broken rollout. Do not weaken them.

import { execFileSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const ROOT = path.resolve(import.meta.dirname, '..');
const OUT = path.join(ROOT, 'release');
const RELEASES_REPO = 'AndreyFSDev/cloufam-releases';

const confRaw = fs.readFileSync(path.join(ROOT, 'src-tauri/tauri.conf.json'), 'utf8');
const { version, plugins } = JSON.parse(confRaw);
const endpoint = plugins.updater.endpoints[0];
const manifestName = path.basename(new URL(endpoint).pathname);

// The signature is verified the same way the client will verify it (`lib.rs::verify_signature`):
// key from tauri.conf.json, base64 over the minisign text, and only the prehashed
// BLAKE2b-512 mode (allow_legacy = false). A foreign TAURI_SIGNING_PRIVATE_KEY otherwise yields
// a release every client rejects, and a published tag can no longer be fixed.
function verifyMinisign(signatureB64, data) {
  const minisignLines = (b64) =>
    Buffer.from(b64, 'base64')
      .toString('utf8')
      .split('\n')
      .map((line) => line.replace(/\r$/, ''));
  const key = Buffer.from(minisignLines(plugins.updater.pubkey)[1] ?? '', 'base64');
  if (key.length !== 42) throw new Error('ключ апдейтера в tauri.conf.json не разобран');
  // The client decodes the manifest string with strict base64 — it will not forgive garbage.
  if (!/^[A-Za-z0-9+/]+={0,2}$/.test(signatureB64)) throw new Error('подпись не base64');
  const [, sigLine = '', trusted = '', globalLine = ''] = minisignLines(signatureB64);
  const sig = Buffer.from(sigLine, 'base64');
  const globalSig = Buffer.from(globalLine, 'base64');
  const TRUSTED = 'trusted comment: ';
  if (sig.length !== 74 || globalSig.length !== 64 || !trusted.startsWith(TRUSTED)) {
    throw new Error('подпись не разобрана как minisign');
  }
  // id bytes in reverse order — that is how the id is printed in the key comment (updater.key.pub).
  const keyId = (bytes) =>
    Buffer.from(bytes.subarray(2, 10)).reverse().toString('hex').toUpperCase();
  if (keyId(sig) !== keyId(key)) {
    throw new Error(
      `подписано ключом ${keyId(sig)}, а в tauri.conf.json ключ ${keyId(key)} — не тот TAURI_SIGNING_PRIVATE_KEY`,
    );
  }
  if (sig.subarray(0, 2).toString('latin1') !== 'ED') {
    throw new Error('подпись без предхеша (legacy) — клиент её не примет');
  }
  const publicKey = crypto.createPublicKey({
    key: { kty: 'OKP', crv: 'Ed25519', x: key.subarray(10).toString('base64url') },
    format: 'jwk',
  });
  const digest = crypto.createHash('blake2b512').update(data).digest();
  if (!crypto.verify(null, digest, publicKey, sig.subarray(10))) {
    throw new Error('подпись не сходится с файлом');
  }
  const comment = trusted.slice(TRUSTED.length);
  if (
    !crypto.verify(
      null,
      Buffer.concat([sig.subarray(10), Buffer.from(comment)]),
      publicKey,
      globalSig,
    )
  ) {
    throw new Error('доверенный комментарий подписи не сходится');
  }
  return comment;
}
const verifySignedFile = (file, sigFile = `${file}.sig`) => {
  try {
    return verifyMinisign(fs.readFileSync(sigFile, 'utf8').trim(), fs.readFileSync(file));
  } catch (e) {
    throw new Error(`${path.relative(ROOT, file)}: ${e.message}`);
  }
};

// Self-check without rollout: node tools/make-release.mjs --verify <file> [<file.sig>]
if (process.argv[2] === '--verify') {
  const [file, sigFile] = process.argv.slice(3);
  if (!file)
    throw new Error('укажи файл: node tools/make-release.mjs --verify <файл> [<файл.sig>]');
  const comment = verifySignedFile(path.resolve(file), sigFile && path.resolve(sigFile));
  console.log(`подпись сходится с ключом из tauri.conf.json: ${file} (${comment})`);
  process.exit(0);
}

// The version lives in three files: the updater and tag look at tauri.conf.json, while the badge
// in the UI and the window title look at package.json and Cargo.toml. When they diverge,
// they show the user the wrong version (this already happened in 1.0.35).
const pkgVersion = JSON.parse(fs.readFileSync(path.join(ROOT, 'package.json'), 'utf8')).version;
const cargoVersion = /^version\s*=\s*"([^"]+)"/m.exec(
  fs.readFileSync(path.join(ROOT, 'src-tauri/Cargo.toml'), 'utf8'),
)?.[1];
for (const [file, value] of [
  ['package.json', pkgVersion],
  ['src-tauri/Cargo.toml', cargoVersion],
]) {
  if (value !== version) {
    throw new Error(`версия в ${file} (${value}) не совпадает с tauri.conf.json (${version})`);
  }
}

// Debug flags must not ship to users.
for (const bad of [
  'remote-debugging-port',
  'use-fake-ui-for-media-stream',
  'use-fake-device-for-media-stream',
  'dangerousInsecureTransportProtocol',
]) {
  if (confRaw.includes(bad)) {
    throw new Error(`в tauri.conf.json остался отладочный флаг: ${bad}`);
  }
}

// SillyTavern scripts embedded in the binary (bridge plugins and extensions) are checked
// by neither the compiler nor svelte-check: a syntax error in them reaches
// the user silently. It has happened before — a broken bridge plugin kills all its routes
// (404 on /api/plugins/cloufam/*), and auto-tags fall off with it,
// along with personality traits and director mode.
const embedded = [];
const SKIP_DIRS = new Set(['target', 'node_modules', 'moonlit']);
const walk = (dir) => {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      // target — files generated by the build, moonlit — the theme author's code.
      if (!SKIP_DIRS.has(entry.name)) walk(full);
    } else if (entry.name.endsWith('.js')) {
      embedded.push(full);
    }
  }
};
walk(path.join(ROOT, 'src-tauri'));
for (const file of embedded) {
  try {
    execFileSync('node', ['--check', file], { stdio: 'pipe' });
  } catch (e) {
    throw new Error(
      `синтаксис вшитого скрипта SillyTavern сломан: ${path.relative(ROOT, file)}\n${e.stderr ?? e.message}`,
    );
  }
}
console.log(`Вшитые скрипты SillyTavern: синтаксис в порядке (${embedded.length} файлов)`);

// A forgotten version bump = "shipped but nobody updated": the updater installs
// only strictly newer versions. We check against the live manifest.
const newer = (a, b) => {
  const pa = a.split('.').map(Number);
  const pb = b.split('.').map(Number);
  for (let i = 0; i < 3; i++) if (pa[i] !== pb[i]) return pa[i] > pb[i];
  return false;
};
try {
  const live = await fetch(endpoint, { signal: AbortSignal.timeout(15000) }).then((r) =>
    r.ok ? r.json() : null,
  );
  if (live && !newer(version, live.version)) {
    throw new Error(
      `версия ${version} не новее прода (${live.version}) — подними version в tauri.conf.json`,
    );
  }
} catch (e) {
  if (String(e).includes('не новее')) throw e;
  console.warn('прод-манифест недоступен — сверка версии пропущена');
}

// The app's own updater downloads `Cloufam.exe` and replaces
// the working file with it, so it must be a build of exactly this version: an old exe in
// the tag gives an endless update loop. We look for the version in the PE resources — as a
// UTF-16LE string, as the compiler writes it.
const releaseExe = path.join(ROOT, 'src-tauri/target/release/cloufam.exe');
if (!fs.existsSync(releaseExe)) {
  throw new Error('нет src-tauri/target/release/cloufam.exe — сначала pnpm tauri build');
}
const exeBytes = fs.readFileSync(releaseExe);
// The value as a whole, with the key and a trailing zero: the substring "1.2.5" would also be found
// in an exe of version 1.2.52. Between key and value — alignment to 4 bytes.
const versionEntry = (pad) => Buffer.from(`ProductVersion\0${pad}${version}\0`, 'utf16le');
if (!exeBytes.includes(versionEntry('')) && !exeBytes.includes(versionEntry('\0'))) {
  throw new Error(
    `в target/release/cloufam.exe нет версии ${version} — exe от другой сборки, пересобери`,
  );
}

// After this only writing: everything that can be checked before it is checked above, so
// a failed check does not leave a half-built release/.
fs.mkdirSync(OUT, { recursive: true });
fs.copyFileSync(releaseExe, path.join(OUT, 'Cloufam.exe'));

// The exe signature is made with the updater key: the client verifies it before replacing the file,
// otherwise the update will not install.
const exeSigPath = `${path.join(OUT, 'Cloufam.exe')}.sig`;
fs.rmSync(exeSigPath, { force: true });
try {
  // shell: true — on Windows pnpm is a .cmd shim, spawn cannot find it without a shell.
  // --password= (with the equals sign) — key with an empty password: without it the CLI fails with
  // "Wrong password" or hangs on an interactive prompt.
  execFileSync(
    'pnpm',
    ['tauri', 'signer', 'sign', '--password=', path.join(OUT, 'Cloufam.exe')],
    {
      cwd: ROOT,
      stdio: 'inherit',
      shell: true,
    },
  );
} catch (e) {
  throw new Error(`не удалось подписать Cloufam.exe: ${e.message}`);
}
if (!fs.existsSync(exeSigPath)) {
  throw new Error('подпись exe не создана — проверь TAURI_SIGNING_PRIVATE_KEY');
}
// The exe is signed by the key from this process's environment, not from the session where the build ran.
verifySignedFile(path.join(OUT, 'Cloufam.exe'), exeSigPath);
console.log('Подпись Cloufam.exe сходится с ключом из tauri.conf.json');
const exeSignature = fs.readFileSync(exeSigPath, 'utf8').trim();

const manifest = {
  version,
  notes: process.argv[2] ?? `Cloufam ${version}`,
  pub_date: new Date().toISOString(),
  exe: {
    url: `https://github.com/${RELEASES_REPO}/releases/download/v${version}/Cloufam.exe`,
    signature: exeSignature,
  },
};
fs.writeFileSync(path.join(OUT, manifestName), `${JSON.stringify(manifest, null, 2)}\n`);

try {
  fs.copyFileSync(releaseExe, path.join(ROOT, 'Cloufam.exe'));
  fs.copyFileSync(releaseExe, path.join(ROOT, 'local-chatbot.exe'));
  console.log(`Обновлены бинарники в корне: Cloufam.exe, local-chatbot.exe`);
} catch (err) {
  console.warn(`Не удалось обновить exe в корне (занят процессом): ${err.message}`);
}
console.log(`Обновлен релизный бинарник: release/Cloufam.exe (v${version})`);

const mb = (fs.statSync(path.join(OUT, 'Cloufam.exe')).size / 1048576).toFixed(1);
console.log(`release/Cloufam.exe  (${mb} МБ)`);
console.log(`release/${manifestName}`);
console.log(
  `\ngh release create v${version} --repo ${RELEASES_REPO} --title "Cloufam ${version}" --notes "${process.argv[2] ?? ''}" release/Cloufam.exe release/${manifestName}`,
);
console.log(`Проверить: curl -sL ${endpoint}`);

