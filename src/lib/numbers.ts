/**
 * Value of a form number field before it is sent to a Tauri command. Svelte 5 returns
 * an empty `<input type=number>` as null, and serde on the Rust side accepts neither
 * null nor an out-of-range value (u8, u32): the whole command fails.
 */
export function clampNumber(
  value: number | null | undefined,
  min: number,
  max: number,
  fallback: number,
): number {
  if (value == null || !Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, value));
}
