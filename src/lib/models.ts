/**
 * Shared utilities for normalizing and validating Ollama model installation.
 */

export function normalizeTag(tag: string): string {
  let s = tag.trim().toLowerCase();
  if (s.startsWith('registry.ollama.ai/')) s = s.slice(19);
  if (s.startsWith('library/')) s = s.slice(8);
  return s;
}

/**
 * The same Ollama tag: `bge-m3` and `bge-m3:latest` are one model.
 * A raw string comparison confused settings (the active model was not highlighted) and
 * the wizard (the size label disappeared) when the tag form differed from the catalog.
 */
export function sameTag(a: string, b: string): boolean {
  const x = normalizeTag(a);
  const y = normalizeTag(b);
  if (!x || !y) return false;
  if (x === y) return true;
  const full = (t: string) => (t.includes(':') ? t : `${t}:latest`);
  return full(x) === full(y);
}

export function isModelInstalled(installedList: string[], tag: string): boolean {
  if (!normalizeTag(tag)) return false;
  return installedList.some((m) => sameTag(m, tag));
}

/**
 * Whether a model looks like a chat model: models outside the catalog have no category,
 * and embeddings, vision, and speech recognition only get in the way in the chat picker.
 */
export function isLikelyChatModel(tag: string): boolean {
  return !/(embed|bge|nomic|minicpm|(^|[/:_-])vl([:_-]|$)|whisper|tts|stable-diffusion)/i.test(tag);
}
