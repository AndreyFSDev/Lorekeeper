/**
 * The «Модели Cloufam» menu inside SillyTavern: a showcase for this hardware with
 * descriptions, one-click installation, live progress, and model selection for
 * chat, memory, or vision. Data and actions go through the app gateway
 * (`/api/cloufam/models*`), the port comes from `window.__LK_GATEWAY`.
 */
(() => {
  var MENU_ITEM_ID = 'cloufam-models-menu-item';
  var OVERLAY_ID = 'lk-models-overlay';
  var CATEGORIES = [
    { id: 'chat', label: 'Чат' },
    { id: 'embed', label: 'Память' },
    { id: 'vision', label: 'Зрение' },
  ];

  // The start-download request is already in flight: a second click must not send 409.
  var downloadInFlight = false;
  var state = {
    open: false,
    loading: false,
    tab: 'chat',
    data: null,
    progress: null,
    timer: null,
    error: '',
  };

  function escapeHtml(value) {
    return String(value == null ? '' : value)
      .replace(/&/g, '&amp;')
      .replace(/</g, '&lt;')
      .replace(/>/g, '&gt;')
      .replace(/"/g, '&quot;');
  }

  function mb(bytes) {
    if (!bytes) return '0 МБ';
    if (bytes < 1048576) return `${(bytes / 1024).toFixed(0)} КБ`;
    if (bytes < 1073741824) return `${(bytes / 1048576).toFixed(0)} МБ`;
    return `${(bytes / 1073741824).toFixed(1)} ГБ`;
  }

  function api(path, options) {
    // The gateway is read on every call: __LK_GATEWAY is set after the panel
    // loads, and a frozen port sent requests to 11435.
    var gateway = window.__LK_GATEWAY || 'http://127.0.0.1:11435';
    var controller = new AbortController();
    var timer = setTimeout(() => {
      controller.abort();
    }, 12000);
    var opts = {};
    if (options) {
      for (var key in options) {
        if (Object.hasOwn(options, key)) opts[key] = options[key];
      }
    }
    opts.signal = controller.signal;
    return fetch(gateway + path, opts)
      .then((response) => {
        if (!response.ok) {
          return response.text().then((text) => {
            // The gateway answers with a JSON error: the user needs its text, not the response body.
            var message = text;
            try {
              const parsed = JSON.parse(text);
              if (parsed?.error) message = parsed.error;
            } catch (_e) {
              // not JSON — show it as is
            }
            throw new Error(message || `HTTP ${response.status}`);
          });
        }
        return response.json();
      })
      .finally(() => {
        clearTimeout(timer);
      });
  }

  function normalizeTag(tag) {
    return String(tag || '')
      .trim()
      .toLowerCase()
      .replace(/^registry\.ollama\.ai\//, '')
      .replace(/^library\//, '');
  }

  function sameTag(a, b) {
    var x = normalizeTag(a);
    var y = normalizeTag(b);
    if (!x || !y) return false;
    var full = (t) => (t.indexOf(':') === -1 ? `${t}:latest` : t);
    return x === y || full(x) === full(y);
  }

  function isInstalled(tag) {
    var list = state.data?.installed || [];
    return list.some((item) => sameTag(item, tag));
  }

  function activeTag(category) {
    var settings = state.data?.settings || {};
    if (category === 'chat') return settings.chat_model || '';
    if (category === 'embed') return settings.embed_model || '';
    if (category === 'vision') return settings.vision_model || '';
    return '';
  }

  function hardwareLine() {
    var hardware = state.data?.hardware || {};
    var parts = [];
    parts.push(`<b>${escapeHtml(hardware.name || 'Видеокарта не найдена')}</b>`);
    if (hardware.vram_mb) parts.push(`${(hardware.vram_mb / 1024).toFixed(1)} ГБ VRAM`);
    var backend = { cuda: 'CUDA', vulkan: 'Vulkan', cpu: 'процессор' }[hardware.backend] || '';
    if (backend) parts.push(`ускорение: ${backend}`);
    var tier = { minimal: 'Minimal', low: 'Low', mid: 'Mid', high: 'High' }[hardware.tier] || '';
    if (tier) parts.push(`тир ${tier}`);
    var ceiling = state.data?.context_ceiling;
    if (ceiling) parts.push(`контекст до ${Number(ceiling).toLocaleString('ru-RU')} токенов`);
    var notes = (hardware.notes || []).map(escapeHtml).join(' · ');
    return parts.join(' · ') + (notes ? `<br><span style="opacity:.85">${notes}</span>` : '');
  }

  function renderProgress(card) {
    var progress = state.progress;
    if (!progress?.running || !sameTag(progress.model, card.tag)) return '';
    var percent =
      progress.total > 0
        ? Math.min(100, Math.round((progress.completed / progress.total) * 100))
        : 8;
    var meta = [];
    if (progress.total > 0) meta.push(`${percent}%`);
    meta.push(mb(progress.completed) + (progress.total > 0 ? ` / ${mb(progress.total)}` : ''));
    if (progress.speed > 0) meta.push(`${(progress.speed / 1048576).toFixed(1)} МБ/с`);
    if (progress.eta > 0) meta.push(`~${Math.ceil(progress.eta / 60)} мин`);
    return (
      '<div class="lk-model-progress">' +
      '<div class="lk-track"><div class="lk-fill" style="width:' +
      Math.max(4, percent) +
      '%"></div></div>' +
      '<div class="lk-meta"><span>' +
      escapeHtml(progress.status || 'загрузка…') +
      '</span><span>' +
      meta.join(' · ') +
      '</span></div>' +
      '</div>'
    );
  }

  function renderCard(model) {
    var installed = isInstalled(model.tag);
    var active = sameTag(activeTag(model.category), model.tag);
    var downloading = state.progress?.running && sameTag(state.progress.model, model.tag);
    var classes = ['lk-model-card'];
    if (installed) classes.push('lk-installed');
    if (active) classes.push('lk-active');

    var badges = [];
    if (active) badges.push('<span class="lk-chip lk-chip-active">★ выбрана</span>');
    else if (installed) badges.push('<span class="lk-chip lk-chip-installed">✓ установлена</span>');
    else badges.push('<span class="lk-chip">не скачана</span>');
    if (model.recommended) badges.push('<span class="lk-chip lk-chip-rec">рекомендуем</span>');
    if (model.badge) badges.push(`<span class="lk-chip">${escapeHtml(model.badge)}</span>`);

    var actions = [];
    if (!installed) {
      // A selected but not installed model (first launch/ollama rm) must still
      // be downloadable — otherwise the «используется» button locks it out.
      actions.push(
        '<button class="lk-btn lk-btn-primary" data-action="download" data-model="' +
          escapeHtml(model.tag) +
          '" data-category="' +
          model.category +
          '"' +
          (downloading ? ' disabled' : '') +
          '>⬇ Скачать (' +
          escapeHtml(model.size) +
          ')</button>',
      );
    } else if (active) {
      actions.push('<button class="lk-btn lk-btn-done" disabled>✓ используется</button>');
    } else {
      actions.push(
        '<button class="lk-btn lk-btn-primary" data-action="select" data-model="' +
          escapeHtml(model.tag) +
          '" data-category="' +
          model.category +
          '">Выбрать</button>',
      );
    }
    if (installed && !active) {
      actions.push(
        '<button class="lk-btn" data-action="select" data-model="' +
          escapeHtml(model.tag) +
          '" data-category="' +
          model.category +
          '">Сделать активной</button>',
      );
    }

    return (
      '<div class="' +
      classes.join(' ') +
      '">' +
      '<div class="lk-model-head">' +
      '<div><div class="lk-model-name">' +
      escapeHtml(model.name) +
      '</div>' +
      '<div class="lk-model-tag">' +
      escapeHtml(model.tag) +
      '</div></div>' +
      '<div class="lk-badges">' +
      badges.join('') +
      '</div>' +
      '</div>' +
      '<p class="lk-model-desc">' +
      escapeHtml(model.description) +
      '</p>' +
      '<div class="lk-model-facts">' +
      '<span>VRAM: <b>' +
      escapeHtml(model.vram) +
      '</b></span>' +
      '<span>Весы: <b>' +
      escapeHtml(model.size) +
      '</b></span>' +
      '<span>Квант: <b>' +
      escapeHtml(model.quant) +
      '</b></span>' +
      '</div>' +
      (model.roleplay_style
        ? '<p class="lk-model-desc" style="opacity:.82">Отыгрыш: ' +
          escapeHtml(model.roleplay_style) +
          '</p>'
        : '') +
      '<div class="lk-model-actions">' +
      actions.join('') +
      '</div>' +
      renderProgress(model) +
      '</div>'
    );
  }

  function render() {
    var overlay = document.getElementById(OVERLAY_ID);
    if (!overlay) return;
    var panel = overlay.querySelector('#lk-models-panel');
    var list = overlay.querySelector('#lk-models-list');
    var hardware = overlay.querySelector('#lk-models-hardware');

    panel.querySelectorAll('#lk-models-tabs button').forEach((button) => {
      button.classList.toggle('lk-active', button.dataset.tab === state.tab);
    });

    if (state.loading && !state.data) {
      hardware.innerHTML = 'Читаю железо и список моделей…';
      list.innerHTML =
        '<div class="lk-skeleton"></div><div class="lk-skeleton"></div><div class="lk-skeleton"></div>';
      return;
    }

    hardware.innerHTML = hardwareLine();
    var models = (state.data?.models || []).filter((model) => model.category === state.tab);

    var progressNote = state.progress?.running
      ? '<div class="lk-model-progress"><div class="lk-meta"><span>Скачивается ' +
        escapeHtml(state.progress.model) +
        '</span><span>' +
        escapeHtml(state.progress.status || '') +
        '</span></div></div>'
      : '';

    var errorNote = state.error
      ? `<div class="lk-model-error">${escapeHtml(state.error)}</div>`
      : '';

    if (models.length === 0) {
      // A load error must not look like an empty hardware catalog.
      list.innerHTML = `${progressNote}${errorNote}<p class="lk-model-desc">${
        state.error
          ? 'Не удалось получить список моделей — повторите позже.'
          : 'Для этого железа в категории нет моделей.'
      }</p>`;
      return;
    }
    list.innerHTML = errorNote + models.map(renderCard).join('');
  }

  function load() {
    state.loading = true;
    state.error = '';
    render();
    api('/api/cloufam/models')
      .then((data) => {
        state.data = data;
      })
      .catch((error) => {
        state.error = `Не удалось получить список моделей: ${error.message}`;
      })
      .finally(() => {
        // loading is always cleared: otherwise the skeleton hangs on a timeout/error.
        state.loading = false;
        render();
        // The download could have been started by anyone (app settings, the wizard,
        // an earlier opening of the menu): we ask for the state right away, otherwise the
        // panel would show «ничего не качается» until the counters arrive as an event.
        pollProgress();
      });
  }

  function pollProgress() {
    // Cloufam fix (A2-07): one request at a time — an 800 ms interval with a
    // 12 s timeout spawned parallel requests, and a late reply clobbered
    // newer progress. While a request is in flight, the tick is simply skipped.
    if (state.pollInFlight) return;
    state.pollInFlight = true;
    api('/api/cloufam/models/progress')
      .then((progress) => {
        var wasRunning = state.progress?.running;
        state.progress = progress;
        if (wasRunning && !progress.running) {
          // The download finished — refresh the list and the installation state.
          load();
        } else {
          render();
        }
        // We keep polling while the download runs: this screen may not be the one that started it.
        ensurePolling();
      })
      .catch(() => {
        ensurePolling();
      })
      .finally(() => {
        state.pollInFlight = false;
      });
  }

  function ensurePolling() {
    // A closed panel does not poll the gateway, even if an in-flight response finished
    // after closing and called ensurePolling again.
    var running = state.progress?.running && state.open;
    if (running && !state.timer) {
      state.timer = setInterval(pollProgress, 800);
    } else if (!running && state.timer) {
      clearInterval(state.timer);
      state.timer = null;
    }
  }

  function startDownload(tag) {
    // A quick double click sent a second POST before the first reply: the gateway
    // answered 409 «уже скачивается», and the red banner «Не удалось начать
    // скачивание» hung over the live progress of that same download.
    if (downloadInFlight) return;
    downloadInFlight = true;
    state.error = '';
    api('/api/cloufam/models/download', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ model: tag }),
    })
      .then(() => {
        state.progress = {
          running: true,
          model: tag,
          status: 'подготовка…',
          completed: 0,
          total: 0,
        };
        render();
        ensurePolling();
      })
      .catch((error) => {
        // 409 — the download is already running (started from another window, for
        // example): this is not a start error, so we re-sync progress instead of a red banner.
        if (/уже скач/i.test(String(error.message))) {
          pollProgress();
          return;
        }
        state.error = `Не удалось начать скачивание: ${error.message}`;
        render();
      })
      .finally(() => {
        downloadInFlight = false;
      });
  }

  function selectModel(tag, category) {
    state.error = '';
    api('/api/cloufam/models/select', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ model: tag, category: category }),
    })
      .then(() => {
        load();
      })
      .catch((error) => {
        state.error = `Не удалось выбрать модель: ${error.message}`;
        render();
      });
  }

  function onClick(event) {
    var button = event.target.closest('button[data-action]');
    if (button) {
      if (button.dataset.action === 'download') startDownload(button.dataset.model);
      if (button.dataset.action === 'select')
        selectModel(button.dataset.model, button.dataset.category);
      return;
    }
    var tab = event.target.closest('#lk-models-tabs button');
    if (tab) {
      state.tab = tab.dataset.tab;
      render();
      return;
    }
    if (event.target.id === 'lk-models-refresh') {
      load();
      return;
    }
    if (event.target.id === 'lk-models-open-settings') {
      fetch(`${window.__LK_GATEWAY || 'http://127.0.0.1:11435'}/api/cloufam/open-settings`, {
        method: 'POST',
      }).catch(() => {});
      return;
    }
    if (event.target.id === 'lk-models-close' || event.target.id === OVERLAY_ID) {
      close();
    }
  }

  function onKey(event) {
    if (event.key === 'Escape' && state.open) close();
  }

  function open() {
    var overlay = document.getElementById(OVERLAY_ID);
    if (!overlay) return;
    state.open = true;
    overlay.classList.remove('lk-hidden');
    document.addEventListener('keydown', onKey);
    load();
  }

  function close() {
    var overlay = document.getElementById(OVERLAY_ID);
    state.open = false;
    if (overlay) overlay.classList.add('lk-hidden');
    document.removeEventListener('keydown', onKey);
    // The panel is closed — progress polling stops right here.
    ensurePolling();
  }

  function buildOverlay() {
    if (document.getElementById(OVERLAY_ID)) return;
    var overlay = document.createElement('div');
    overlay.id = OVERLAY_ID;
    overlay.className = 'lk-hidden';
    overlay.innerHTML =
      '<div id="lk-models-panel">' +
      '<header>' +
      '<div><div class="lk-models-title">Модели Cloufam</div>' +
      '<div class="lk-models-sub">Витрина под это железо: что скачано, что подойдёт и чем это ускоряется</div></div>' +
      '<div class="lk-models-spacer"></div>' +
      '<button class="lk-models-icon-btn" id="lk-models-open-settings" title="Открыть настройки приложения">⚙ Настройки</button>' +
      '<button class="lk-models-icon-btn" id="lk-models-refresh" title="Обновить список">🔄</button>' +
      '<button class="lk-models-close" id="lk-models-close" title="Закрыть">✕</button>' +
      '</header>' +
      '<div id="lk-models-hardware"></div>' +
      '<div id="lk-models-tabs">' +
      CATEGORIES.map(
        (category) => `<button data-tab="${category.id}">${category.label}</button>`,
      ).join('') +
      '</div>' +
      '<div id="lk-models-list"></div>' +
      '<div id="lk-models-note">Модель скачивается в фоне: можно закрыть окно и продолжать чат. Выбор модели применяется сразу — без перезапуска.</div>' +
      '</div>';
    overlay.addEventListener('click', onClick);
    document.body.append(overlay);
  }

  // Cloufam fix (A2-07): retry the menu item injection — until the first
  // successful insert, rather than an endless setInterval every 2 s.
  var injectTimer = null;

  function stopInjectRetry() {
    if (injectTimer !== null) {
      clearInterval(injectTimer);
      injectTimer = null;
    }
  }

  function injectMenuItem() {
    var menu = document.getElementById('extensionsMenu');
    if (!menu) return;
    if (document.getElementById(MENU_ITEM_ID)) {
      stopInjectRetry();
      return;
    }
    var container = document.createElement('div');
    container.id = 'cloufam_models_container';
    container.className = 'extension_container';
    var item = document.createElement('div');
    item.id = MENU_ITEM_ID;
    item.className = 'interactable';
    item.title = 'Модели Cloufam (скачать, выбрать, посмотреть описания)';
    item.innerHTML =
      '<div class="fa-solid fa-brain extensionsMenuExtensionButton"></div><span>Модели Cloufam</span>';
    item.onclick = open;
    container.append(item);
    menu.prepend(container);
    stopInjectRetry();
  }

  function init() {
    buildOverlay();
    // The interval is set before the first attempt: a successful inject stops it right away.
    injectTimer = setInterval(injectMenuItem, 2000);
    injectMenuItem();
    window.__lkOpenModels = open;
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', init);
  } else {
    init();
  }
})();

