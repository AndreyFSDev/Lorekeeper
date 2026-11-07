// Slash command /поиск: searches the internet through the server-side websearch plugin
// and puts the results into the chat as a narrator message so the character can use them.

import { event_types, eventSource } from '../../../events.js';
import { SlashCommand } from '../../../slash-commands/SlashCommand.js';
import {
  ARGUMENT_TYPE,
  SlashCommandArgument,
} from '../../../slash-commands/SlashCommandArgument.js';
import { sendNarratorMessage } from '../../../slash-commands.js';
import { getContext } from '../../../st-context.js';

const { getRequestHeaders, SlashCommandParser } = getContext();
let chatEpoch = 0;

// The epoch cuts off a stale result even after A → B → A.
eventSource.on(event_types.CHAT_CHANGED, () => {
  chatEpoch += 1;
});

function currentChatKey() {
  const context = getContext();
  return JSON.stringify([
    context.groupId ?? null,
    context.characterId ?? null,
    context.getCurrentChatId?.() ?? context.chatId ?? null,
  ]);
}

/** Failures are shown as a toast: they do not go into the chat, so they never reach the model's prompt. */
function notify(message) {
  toastr.warning(message, 'Веб-поиск');
  return message;
}

SlashCommandParser.addCommandObject(
  SlashCommand.fromProps({
    name: 'поиск',
    aliases: ['search', 'ищи'],
    callback: async (_args, query) => {
      // Snapshot before the first network await.
      const chatKey = currentChatKey();
      const epoch = chatEpoch;
      try {
        const q = String(query ?? '').trim();
        if (!q) {
          return notify('Укажите запрос: /поиск <что искать>');
        }
        const result = await fetch('/api/plugins/websearch/search', {
          method: 'POST',
          headers: getRequestHeaders(),
          body: JSON.stringify({ query: q }),
        });
        if (!result.ok) {
          const text = await result.text();
          return notify('Поиск не удался: ' + text);
        }
        const data = await result.json();
        if (!data.results || data.results.length === 0) {
          return notify('По запросу «' + q + '» ничего не найдено');
        }
        const lines = data.results.map(
          (r, i) => `${i + 1}. ${r.title}\n   ${r.url}\n   ${r.snippet}`,
        );
        const report =
          'Результаты веб-поиска по запросу «' +
          q +
          '»:\n' +
          lines.join('\n') +
          '\n\nОпирайся на эти сведения в ответе, ссылайся на источники.';
        if (epoch !== chatEpoch || chatKey !== currentChatKey()) {
          return notify('Чат изменился — результаты поиска не добавлены в другой разговор.');
        }
        // ST's input field discards the command's return value
        // (executeSlashCommandsOnChatInput): the result reaches the character
        // only as a chat message. The narrator reaches the model with the system role.
        await sendNarratorMessage({ name: 'Веб-поиск' }, report);
        return report;
      } catch (error) {
        return notify('Ошибка веб-поиска: ' + error);
      }
    },
    helpString:
      'Найти в интернете: /поиск <запрос>. Результаты приходят в чат сообщением рассказчика — персонаж видит их в следующем ответе.',
    returns: ARGUMENT_TYPE.STRING,
    unnamedArgumentList: [new SlashCommandArgument('запрос', ARGUMENT_TYPE.STRING, true, false)],
  }),
);
