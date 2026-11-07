// Cloufam: a stream frame redraws only the tail of the message.
//
// ST runs the whole reply through messageFormatting on every stream frame
// (showdown + DOMPurify) and replaces the message's innerHTML entirely. On a long
// reply that's milliseconds of markup plus restyling of the whole chat: inserting
// nodes wakes :has() selectors on body. Here finished blocks (up to a blank
// line, after which rendering can no longer change) are formatted once
// and stay in the DOM, while the tail is morphed by morphdom — usually only
// the text node of the last paragraph changes. The final frame is rendered normally, so
// the result matches ST.
import { morphdom } from '../lib.js';

const streams = new WeakMap();

// Cloufam edit (A2-05): the tail reformatting window. Without it, when
// there is no safe boundary (one long block without blank lines), every
// frame ran messageFormatting over the whole reply and morphed all nodes — O(n²) in
// stream length. Older than the window, text is frozen as is (in an unfinished
// block it could still change — that's intermediate accuracy), but a frame
// costs O(window); the final frame is rendered fully regardless.
const TAIL_WINDOW = 2048;

const VOID_TAGS = new Set([
  'area',
  'base',
  'br',
  'col',
  'embed',
  'hr',
  'img',
  'input',
  'link',
  'meta',
  'source',
  'track',
  'wbr',
]);

/**
 * @param {HTMLElement} element `.mes_text` of the message
 * @param {string} text the whole reply text on this frame
 * @param {(text: string) => string} format messageFormatting with the message parameters
 * @param {boolean} isFinal the last stream frame
 */
export function renderStreamingText(element, text, format, isFinal) {
  if (!isFinal) {
    try {
      renderTail(element, text, format);
      return;
    } catch (error) {
      console.warn('[cloufam] хвостовой рендер стрима не удался, рендерим целиком:', error);
    }
  }
  streams.delete(element);
  element.innerHTML = format(text);
}

function renderTail(element, text, format) {
  let state = streams.get(element);
  // The finished part must remain untouched both in text and in the DOM: reasoning
  // parsing, reply cleanup, or someone else's code could rewrite both.
  if (
    !state ||
    !text.startsWith(state.source) ||
    (state.count > 0 && element.childNodes[state.count - 1] !== state.last)
  ) {
    state = { source: '', count: 0, last: null, boundaryAt: -1, boundary: null };
    streams.set(element, state);
  }
  const start = state.source.length;
  let end = stableEnd(text, start, state);
  // Cloufam edit (A2-05): the freeze boundary is no closer than TAIL_WINDOW to the end.
  const windowStart = Math.max(start, text.length - TAIL_WINDOW);
  if (end < windowStart) {
    end = windowStart;
  }
  const chunk = end > start ? format(text.slice(start, end)) : '';
  const tail = format(text.slice(end));
  // showdown puts a newline between blocks — repeat it so the nodes
  // match what the final render will produce.
  const head = parse(state.count > 0 && chunk ? `\n${chunk}` : chunk);
  const rest = parse((state.count > 0 || chunk) && tail ? `\n${tail}` : tail);
  const headCount = head.childNodes.length;
  morphRange(element, state.count, [...head.childNodes, ...rest.childNodes]);
  state.count += headCount;
  state.source = text.slice(0, end);
  state.last = state.count > 0 ? element.childNodes[state.count - 1] : null;
}

/** End of the finished part: the last blank line, if rendering up to it can no longer change. */
function stableEnd(text, start, state) {
  const blank = /\n(?:[ \t]*\n)+/g;
  blank.lastIndex = start;
  let candidate = -1;
  for (let match = blank.exec(text); match; match = blank.exec(text)) {
    candidate = match.index + match[0].length;
  }
  return candidate > start && isSafeBoundary(text, candidate, state) ? candidate : start;
}

function isSafeBoundary(text, end, state) {
  const lineEnd = text.indexOf('\n', end);
  const firstLine = text.slice(end, lineEnd === -1 ? undefined : lineEnd);
  // The start of the next block is not visible yet: can't decide whether it continues a list or a quote.
  if (lineEnd === -1 && firstLine.length < 8) {
    return false;
  }
  // Indentation continues a list or a code block across a blank line.
  if (/^\s/.test(firstLine)) {
    return false;
  }
  // Cloufam edit (A2-05): the heavy prefix check (markers, open HTML,
  // kind of the last block) is cached by indentation — text is only appended, and
  // its beginning for the same indentation doesn't change, while scanning the whole prefix
  // on every frame was O(n²). Tail checks over the first line of the next
  // block are always recomputed: it is still being appended.
  if (!state || state.boundaryAt !== end) {
    const boundary = boundaryPrefix(text, end);
    if (state) {
      state.boundaryAt = end;
      state.boundary = boundary;
    }
    if (!boundary.safe) {
      return false;
    }
    return boundary.lastKind === '' || boundary.lastKind !== blockKind(firstLine);
  }
  const cached = state.boundary;
  return cached.safe && (cached.lastKind === '' || cached.lastKind !== blockKind(firstLine));
}

/** Prefix check by the indentation of `end`: unbalanced markers, open HTML, kind of the last block. */
function boundaryPrefix(text, end) {
  const prefix = text.slice(0, end);
  // ST balances unbalanced markers over the whole text (stream balancing,
  // fixMarkdown), and their pair may end up in the tail.
  for (const mark of ['```', '~~~', '*', '_', '"']) {
    if (count(prefix, mark) % 2) {
      return { safe: false, lastKind: '' };
    }
  }
  if (hasOpenHtml(prefix)) {
    return { safe: false, lastKind: '' };
  }
  // Two list items (or two quotes) separated by a blank line are merged by showdown into
  // one block — the render of the finished part would change.
  const lastBlock =
    prefix
      .trimEnd()
      .split(/\n[ \t]*\n/)
      .pop() ?? '';
  return { safe: true, lastKind: blockKind(lastBlock) };
}

function blockKind(line) {
  if (/^([-*+]|\d{1,9}[.)])\s/.test(line)) {
    return 'list';
  }
  return line.startsWith('>') ? 'quote' : '';
}

function count(text, mark) {
  let total = 0;
  for (let at = text.indexOf(mark); at !== -1; at = text.indexOf(mark, at + mark.length)) {
    total++;
  }
  return total;
}

/** An unclosed HTML tag or comment: `<div>`/`<style>` across a blank line — one block. */
function hasOpenHtml(prefix) {
  if (prefix.lastIndexOf('<!--') > prefix.lastIndexOf('-->') || /<[a-zA-Z][^>]*$/.test(prefix)) {
    return true;
  }
  const depth = new Map();
  for (const [, closing, name, selfClosing] of prefix.matchAll(
    /<(\/?)([a-zA-Z][\w-]*)\b[^>]*?(\/?)>/g,
  )) {
    const tag = name.toLowerCase();
    if (VOID_TAGS.has(tag) || selfClosing) {
      continue;
    }
    depth.set(tag, (depth.get(tag) ?? 0) + (closing ? -1 : 1));
  }
  return [...depth.values()].some((open) => open > 0);
}

function parse(html) {
  const template = document.createElement('template');
  template.innerHTML = html;
  return template.content;
}

/** Bring the element's nodes starting at offset to next without touching the finished part before offset. */
function morphRange(element, offset, next) {
  const current = Array.prototype.slice.call(element.childNodes, offset);
  next.forEach((node, i) => {
    const old = current[i];
    if (!old) {
      element.appendChild(node);
    } else if (old.nodeType === node.nodeType && old.nodeName === node.nodeName) {
      morphdom(old, node);
    } else {
      old.replaceWith(node);
    }
  });
  for (let i = next.length; i < current.length; i++) {
    current[i].remove();
  }
}

