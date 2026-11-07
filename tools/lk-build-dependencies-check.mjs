import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import test from 'node:test';

const require = createRequire(import.meta.url);
const pluginRequire = createRequire(require.resolve('@sveltejs/vite-plugin-svelte'));
const merge = pluginRequire('deepmerge');

test('merging a target with an own __proto__ cannot change the result prototype', () => {
  const target = JSON.parse('{"__proto__":{"polluted":true},"valid":{"a":1}}');
  const result = merge(target, { valid: { b: 2 } });
  assert.equal(Object.getPrototypeOf(result), Object.prototype);
  assert.equal(result.polluted, undefined);
  assert.deepEqual(result.valid, { a: 1, b: 2 });
});
