// Shared lookup of the `ws` module for CDP test stands. It used to be taken from
// `sillytavern/node_modules` next to the project, but the working copy of the app
// lives outside the repository (`Z:\AImodels\Cloufam`), so the path is searched among candidates.
import { createRequire } from 'node:module';
import path from 'node:path';

const require = createRequire(import.meta.url);

export function loadWs(root) {
  const candidates = [
    path.join(root, 'node_modules', 'ws'),
    path.join(root, 'sillytavern', 'node_modules', 'ws'),
    process.env.LOREKEEPER_INSTALL
      ? path.join(process.env.LOREKEEPER_INSTALL, 'sillytavern', 'node_modules', 'ws')
      : null,
  ].filter(Boolean);
  for (const candidate of candidates) {
    try {
      return require(candidate);
    } catch {
      // try the next path
    }
  }
  throw new Error(
    `модуль ws не найден (искал: ${candidates.join(', ')}); задайте LOREKEEPER_INSTALL`,
  );
}

