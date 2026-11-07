// Roleplay card markup: *action* in italics, **emphasis** in bold.
// All text is escaped first, so {@html} does not let HTML from the model reply through.

const ESCAPES: Record<string, string> = {
  '&': '&amp;',
  '<': '&lt;',
  '>': '&gt;',
  '"': '&quot;',
  "'": '&#39;',
};

export function renderMarkup(text: string): string {
  return text
    .replace(/[&<>"']/g, (ch) => ESCAPES[ch])
    .replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>')
    .replace(/\*([^*\n]+)\*/g, '<em>$1</em>');
}
