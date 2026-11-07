// Bridge between the auto-tag extension and the local Cloufam gateway:
// the extension does not know the gateway port (it is dynamic), while the ST
// node knows it from the environment variable that Rust sets on spawn.
// Route: POST /api/plugins/cloufam/tags -> {tags:[...]}
//        GET  /api/plugins/cloufam/config -> tagger settings
const info = {
  id: 'cloufam',
  name: 'Cloufam (теги и режиссёр)',
  description: 'Доступ к локальному шлюзу Cloufam: авто-теги персонажей и режим режиссёра.',
};

// The gateway itself waits for Ollama up to 180 s (tags, md-import, director):
// an equal timeout cut off responses the gateway still managed to return.
const TIMEOUT_MS = 200000;

function gatewayBase() {
  const port = process.env.LOREKEEPER_GATEWAY_PORT;
  if (!port) {
    return null;
  }
  return `http://127.0.0.1:${port}`;
}

async function init(router) {
  const { default: fetch } = await import('node-fetch');

  router.get('/config', async (_request, response) => {
    const base = gatewayBase();
    if (!base) {
      return response.status(503).send('Шлюз Cloufam не запущен');
    }
    try {
      // AbortSignal.timeout: the timeout option exists only in node-fetch v2,
      // on Node 24 it is silently ignored.
      const result = await fetch(`${base}/api/cloufam/tagger-config`, {
        signal: AbortSignal.timeout(10000),
      });
      const text = await result.text();
      return response.status(result.status).type('application/json').send(text);
    } catch (error) {
      console.error('[cloufam] config:', error);
      return response.status(502).send('Шлюз Cloufam недоступен');
    }
  });

  router.post('/config', async (request, response) => {
    const base = gatewayBase();
    if (!base) {
      return response.status(503).send('Шлюз Cloufam не запущен');
    }
    try {
      const result = await fetch(`${base}/api/cloufam/tagger-config`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request.body ?? {}),
        signal: AbortSignal.timeout(10000),
      });
      const text = await result.text();
      return response.status(result.status).type('application/json').send(text);
    } catch (error) {
      console.error('[cloufam] config update:', error);
      return response.status(502).send('Шлюз Cloufam недоступен');
    }
  });

  router.post('/tags', async (request, response) => {
    const base = gatewayBase();
    if (!base) {
      return response.status(503).send('Шлюз Cloufam не запущен');
    }
    try {
      const result = await fetch(`${base}/api/cloufam/tags`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request.body ?? {}),
        signal: AbortSignal.timeout(TIMEOUT_MS),
      });
      const text = await result.text();
      return response.status(result.status).type('application/json').send(text);
    } catch (error) {
      console.error('[cloufam] tags:', error);
      return response.status(502).send('Модель не ответила: ' + String(error));
    }
  });

  // Character traits: avatar -> text for the card's depth_prompt.
  router.get('/traits', async (_request, response) => {
    const base = gatewayBase();
    if (!base) {
      return response.status(503).send('Шлюз Cloufam не запущен');
    }
    try {
      const result = await fetch(`${base}/api/cloufam/traits`, {
        signal: AbortSignal.timeout(10000),
      });
      const text = await result.text();
      return response.status(result.status).type('application/json').send(text);
    } catch (error) {
      console.error('[cloufam] traits:', error);
      return response.status(502).send('Шлюз Cloufam недоступен');
    }
  });

  // Who is in the chat right now: the gateway mixes live state in by this name.
  router.post('/active-character', async (request, response) => {
    const base = gatewayBase();
    if (!base) {
      return response.status(503).send('Шлюз Cloufam не запущен');
    }
    try {
      const result = await fetch(`${base}/api/cloufam/active-character`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request.body ?? {}),
        signal: AbortSignal.timeout(10000),
      });
      const text = await result.text();
      return response.status(result.status).type('application/json').send(text);
    } catch (error) {
      console.error('[cloufam] active-character:', error);
      return response.status(502).send('Шлюз Cloufam недоступен');
    }
  });

  // Import a text draft into the character card fields.
  router.post('/md-import', async (request, response) => {
    const base = gatewayBase();
    if (!base) {
      return response.status(503).send('Шлюз Cloufam не запущен');
    }
    try {
      const result = await fetch(`${base}/api/cloufam/md-import`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request.body ?? {}),
        signal: AbortSignal.timeout(TIMEOUT_MS),
      });
      const text = await result.text();
      return response.status(result.status).type('application/json').send(text);
    } catch (error) {
      console.error('[cloufam] md-import:', error);
      return response.status(502).send('Модель не ответила: ' + String(error));
    }
  });

  // Auto-translate imported cards and lorebooks.
  router.post('/translate', async (request, response) => {
    const base = gatewayBase();
    if (!base) {
      return response.status(503).send('Шлюз Cloufam не запущен');
    }
    try {
      const result = await fetch(`${base}/api/cloufam/translate`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request.body ?? {}),
        signal: AbortSignal.timeout(TIMEOUT_MS),
      });
      const text = await result.text();
      return response.status(result.status).type('application/json').send(text);
    } catch (error) {
      console.error('[cloufam] translate:', error);
      return response.status(502).send('Модель не ответила: ' + String(error));
    }
  });

  // Director mode: a note turns into a narrator's aside.
  router.post('/director', async (request, response) => {
    const base = gatewayBase();
    if (!base) {
      return response.status(503).send('Шлюз Cloufam не запущен');
    }
    try {
      const result = await fetch(`${base}/api/cloufam/director`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(request.body ?? {}),
        signal: AbortSignal.timeout(TIMEOUT_MS),
      });
      const text = await result.text();
      return response.status(result.status).type('application/json').send(text);
    } catch (error) {
      console.error('[cloufam] director:', error);
      return response.status(502).send('Модель не ответила: ' + String(error));
    }
  });
}

module.exports = { info, init };

