// Web search without API keys: GET to DuckDuckGo Lite (the html output blocks
// datacenter IPs, lite works). Route:
// POST /api/plugins/websearch/search {query} -> {results:[{title,url,snippet}]}
const info = {
  id: 'websearch',
  name: 'WebSearch (DuckDuckGo)',
  description: 'Веб-поиск для персонажей Cloufam через DuckDuckGo Lite, без API-ключей.',
};

const USER_AGENT =
  'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36';

function decodeEntities(s) {
  return s
    .replace(/&amp;/g, '&')
    .replace(/&lt;/g, '<')
    .replace(/&gt;/g, '>')
    .replace(/&quot;/g, '"')
    .replace(/&#x27;/g, "'")
    .replace(/&nbsp;/g, ' ')
    .replace(/&#(\d+);/g, (_, n) => String.fromCharCode(Number(n)));
}

function stripTags(s) {
  return s
    .replace(/<[^>]+>/g, ' ')
    .replace(/\s+/g, ' ')
    .trim();
}

async function init(router) {
  // node-fetch lives in SillyTavern's node_modules — resolves up the tree.
  const { default: fetch } = await import('node-fetch');

  router.post('/search', async (request, response) => {
    try {
      const query = String(request.body?.query ?? '').trim();
      if (!query) {
        return response.status(400).send('Пустой запрос');
      }

      const url = 'https://lite.duckduckgo.com/lite/?' + new URLSearchParams({ q: query });
      const result = await fetch(url, {
        method: 'GET',
        headers: {
          'User-Agent': USER_AGENT,
          Accept: 'text/html',
          'Accept-Language': 'ru-RU,ru;q=0.9,en;q=0.5',
        },
        // AbortSignal.timeout: node-fetch v2's timeout option is ignored on Node 24
        signal: AbortSignal.timeout(15000),
      });

      if (!result.ok) {
        console.error('[websearch] HTTP', result.status);
        return response
          .status(502)
          .send('Поисковик недоступен (HTTP ' + result.status + '), попробуйте позже');
      }

      const html = await result.text();
      if (!html.includes('result-link')) {
        return response
          .status(429)
          .send('Поисковик временно ограничил запросы, попробуйте через минуту');
      }

      const results = [];
      const re =
        /<a rel="nofollow" href="\/\/duckduckgo\.com\/l\/\?uddg=([^&"]+)[^"]*" class='result-link'>([\s\S]*?)<\/a>[\s\S]*?<td class='result-snippet'>([\s\S]*?)<\/td>/g;
      let match;
      // biome-ignore lint/suspicious/noAssignInExpressions: iterating over global regex matches
      while ((match = re.exec(html)) !== null && results.length < 5) {
        let realUrl;
        try {
          realUrl = decodeURIComponent(match[1]);
        } catch {
          realUrl = match[1];
        }
        results.push({
          title: stripTags(decodeEntities(match[2])),
          url: realUrl,
          snippet: stripTags(decodeEntities(match[3])),
        });
      }

      if (results.length === 0) {
        return response.json({ results: [], message: 'Ничего не найдено' });
      }
      return response.json({ results });
    } catch (error) {
      console.error('[websearch]', error);
      return response.status(500).send('Ошибка веб-поиска');
    }
  });
}

module.exports = { info, init };

