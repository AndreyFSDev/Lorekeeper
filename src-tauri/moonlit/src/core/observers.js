import { getSettings as getExtensionSettings } from '../services/settings-service.js';

// Правка Lorekeeper (A2-01): признак включённой темы moonlit — наблюдатели и
// мониторы работают только при enabled=true (раньше toggleCss снимал лишь
// link'и темы, и инжекторы продолжали работать при выключенной теме). Поле
// может быть ещё не засеяно (defaultSettings.enabled = true), поэтому
// выключенным считаем только явный `false`.
function themeEnabled() {
  const context = SillyTavern.getContext();
  const settings = getExtensionSettings(context) || {};
  return settings.enabled !== false;
}

function stripOrigin(url) {
  if (!url) return '';
  if (url.startsWith(window.location.origin)) {
    return url.replace(window.location.origin, '');
  }
  return url;
}

function parseAvatarSource(rawSrc) {
  if (!rawSrc) return null;

  const normalized = stripOrigin(rawSrc);
  const trimmed = normalized.startsWith('/') ? normalized.slice(1) : normalized;

  try {
    const parsed = new URL(normalized, window.location.origin);
    if (parsed.pathname.endsWith('thumbnail')) {
      const type = parsed.searchParams.get('type');
      const file = parsed.searchParams.get('file');
      if (type && file) {
        return { type, file: decodeURIComponent(file) };
      }
    }
  } catch (_err) {
    // Ignore URL parse errors and fall back to path inspection
  }

  if (trimmed.startsWith('characters/')) {
    return { type: 'avatar', file: trimmed.replace(/^characters\//, '') };
  }

  if (trimmed.startsWith('User Avatars/')) {
    return { type: 'persona', file: trimmed.replace(/^User Avatars\//, '') };
  }

  return { type: null, file: trimmed };
}

function getAvatarSources(rawSrc) {
  const info = parseAvatarSource(rawSrc);
  if (!info) {
    return { thumb: null, original: null };
  }

  const { type, file } = info;
  const ensureAbsolute = (path) => {
    if (!path) return '';
    return path.startsWith('/') ? path : `/${path}`;
  };

  const thumb =
    type === 'avatar' || type === 'persona'
      ? `/thumbnail?type=${type}&file=${encodeURIComponent(file)}`
      : ensureAbsolute(info.file);

  const original =
    type === 'avatar'
      ? ensureAbsolute(`characters/${file}`)
      : type === 'persona'
        ? ensureAbsolute(`User Avatars/${file}`)
        : ensureAbsolute(info.file);

  return {
    thumb: stripOrigin(thumb),
    original: stripOrigin(original),
  };
}

function formatSrcsetUrl(url) {
  try {
    return encodeURI(url).replace(/,/g, '%2C');
  } catch (_err) {
    return url.replace(/,/g, '%2C');
  }
}

function applyAvatarSources(mes, avatarImg, preferOriginal) {
  const srcCandidate = avatarImg.getAttribute('src') || avatarImg.getAttribute('data-src');
  if (!srcCandidate) return;

  const { thumb, original } = getAvatarSources(srcCandidate);
  if (!thumb && !original) return;

  const thumbUrl = thumb || original;
  const originalUrl = original || thumbUrl;
  const targetUrl = preferOriginal ? originalUrl : thumbUrl;

  // Правка Lorekeeper (A2-01): значения пишем только при изменении — раньше
  // каждый `.mes` получал dataset×3 + setProperty×3 безусловно на каждую пачку
  // мутаций стрима (ST переписывает сообщение целиком каждый кадр), и это
  // перестилизовывало чат через :has()-селекторы (см. gotchas «textContent
  // внутри чата ST перестилизует весь чат»).
  if (mes.dataset.avatarThumb !== thumbUrl) {
    mes.dataset.avatarThumb = thumbUrl;
  }
  if (mes.dataset.avatarOriginal !== originalUrl) {
    mes.dataset.avatarOriginal = originalUrl;
  }
  if (mes.dataset.avatar !== targetUrl) {
    mes.dataset.avatar = targetUrl;
  }

  const thumbProp = `url('${thumbUrl}')`;
  if (mes.style.getPropertyValue('--mes-avatar-thumb-url') !== thumbProp) {
    mes.style.setProperty('--mes-avatar-thumb-url', thumbProp);
  }
  const originalProp = `url('${originalUrl}')`;
  if (mes.style.getPropertyValue('--mes-avatar-original-url') !== originalProp) {
    mes.style.setProperty('--mes-avatar-original-url', originalProp);
  }
  const targetProp = `url('${targetUrl}')`;
  if (mes.style.getPropertyValue('--mes-avatar-url') !== targetProp) {
    mes.style.setProperty('--mes-avatar-url', targetProp);
  }

  const currentSrc = stripOrigin(avatarImg.getAttribute('src') || '');
  const desiredSrc = stripOrigin(thumbUrl);
  if (desiredSrc && currentSrc !== desiredSrc) {
    avatarImg.setAttribute('src', thumbUrl);
  }

  const wantSrcset = Boolean(preferOriginal && originalUrl && originalUrl !== thumbUrl);
  const nextSrcset = wantSrcset ? formatSrcsetUrl(originalUrl) : null;
  const currentSrcset = avatarImg.getAttribute('srcset');
  if (currentSrcset !== nextSrcset) {
    if (nextSrcset === null) {
      avatarImg.removeAttribute('srcset');
    } else {
      avatarImg.setAttribute('srcset', nextSrcset);
    }
  }
}

/**
 * Initialize avatar injector observer.
 * Injects avatar URLs into message elements so they can be used in CSS.
 * @returns {function} Function to manually trigger avatar updates.
 */
export function initAvatarInjector() {
  function updateMessage(mes, preferOriginal) {
    const avatarImg = mes.querySelector?.('.avatar img');
    if (!avatarImg) return;
    applyAvatarSources(mes, avatarImg, preferOriginal);
  }

  function preferOriginalAvatars() {
    const context = SillyTavern.getContext();
    const settings = getExtensionSettings(context) || {};
    return (
      settings.useOriginalAvatarImages === true || document.body.classList.contains('ripplestyle')
    );
  }

  function updateAvatars() {
    const preferOriginal = preferOriginalAvatars();
    document.querySelectorAll('.mes').forEach((mes) => updateMessage(mes, preferOriginal));
  }

  // Правка Lorekeeper (A2-01): обновляем только добавленные узлы, а не весь чат
  // на каждую пачку мутаций — стрим добавляет/переписывает сообщения целиком,
  // и прежний rescan всего `.mes` гонял dataset/стили по всему чату.
  function messagesFrom(nodes) {
    const preferOriginal = preferOriginalAvatars();
    const seen = new Set();
    for (const node of nodes) {
      if (node.nodeType !== 1) continue;
      const own = node.matches?.('.mes') ? node : node.closest?.('.mes');
      if (own && !seen.has(own)) {
        seen.add(own);
        updateMessage(own, preferOriginal);
      }
      node.querySelectorAll?.('.mes').forEach((mes) => {
        if (!seen.has(mes)) {
          seen.add(mes);
          updateMessage(mes, preferOriginal);
        }
      });
    }
  }

  let debounceTimer;
  let pendingNodes = [];
  const observerCallback = (mutations) => {
    for (const mutation of mutations) {
      pendingNodes.push(...mutation.addedNodes);
    }
    clearTimeout(debounceTimer);
    debounceTimer = setTimeout(() => {
      const nodes = pendingNodes;
      pendingNodes = [];
      messagesFrom(nodes);
    }, 100);
  };

  let observer = null;
  function start() {
    if (observer) return;
    updateAvatars();
    const chatContainer = document.getElementById('chat');
    if (chatContainer) {
      observer = new MutationObserver(observerCallback);
      observer.observe(chatContainer, { childList: true, subtree: true });
    }
  }

  function stop() {
    clearTimeout(debounceTimer);
    pendingNodes = [];
    if (observer) {
      observer.disconnect();
      observer = null;
    }
  }

  // Правка Lorekeeper (A2-01): наблюдатель живёт только при включённой теме —
  // dataset/стили аватаров нужны только её CSS, а toggleCss раньше снимал лишь
  // link'и темы и инжектор продолжал работать при enabled=false. Жизненным
  // циклом управляет toggleCss (см. index.js).
  if (themeEnabled()) {
    start();
  }

  window.updateAvatars = updateAvatars;
  // Правка Lorekeeper: контроллер для toggleCss.
  window.moonlitAvatarInjector = { update: updateAvatars, start, stop };
  return updateAvatars;
}

/**
 * Initialize monitoring of #form_sheld height and expose helper controls.
 * @returns {{update: function, start: function, stop: function}} Control helpers.
 */
export function initFormSheldHeightMonitor() {
  let isInitialized = false;
  // Правка Lorekeeper (A2-01): монитор живёт только при включённой теме —
  // переменная `--formSheldHeight` нужна только её CSS, а при выключенной теме
  // наблюдатели и слушатели держали форс-рефлоу (getBoundingClientRect) на
  // каждый тоггл кнопок. Флаг гасит и замеры, и подписки; жизненный цикл
  // ведёт toggleCss через контроллер (см. index.js).
  let active = false;

  function getAccurateHeight(element) {
    if (!element) return 0;
    const rect = element.getBoundingClientRect();
    return rect.height;
  }

  function updateFormSheldHeight() {
    if (!active) return;
    const formSheld = document.getElementById('form_sheld');
    if (formSheld) {
      const height = getAccurateHeight(formSheld);
      if (height > 0) {
        document.documentElement.style.setProperty('--formSheldHeight', `${height}px`);
        isInitialized = true;
      }
    }
  }

  const mutationObserver = new MutationObserver((mutations) => {
    let shouldUpdate = false;

    for (const mutation of mutations) {
      if (mutation.target.id === 'form_sheld' || mutation.target.closest?.('#form_sheld')) {
        shouldUpdate = true;
        break;
      }

      if (mutation.addedNodes.length) {
        for (const node of mutation.addedNodes) {
          if (
            node.id === 'form_sheld' ||
            (node.nodeType === 1 && node.querySelector && node.querySelector('#form_sheld'))
          ) {
            shouldUpdate = true;
            break;
          }
        }
      }
    }

    if (shouldUpdate) {
      setTimeout(updateFormSheldHeight, 0);
    }
  });

  const resizeObserver = new ResizeObserver((entries) => {
    for (const entry of entries) {
      if (entry.target.id === 'form_sheld') {
        const { height } = entry.contentRect;
        if (height > 0) {
          document.documentElement.style.setProperty('--formSheldHeight', `${height}px`);
          isInitialized = true;
        }
      }
    }
  });

  // Правка Lorekeeper (A2-01): stop снимает все подписки, включая bodyObserver,
  // глобальные слушатели и отложенные таймеры — иначе выключенная тема
  // продолжала получать рефлоу.
  function stopObservers() {
    resizeObserver.disconnect();
    mutationObserver.disconnect();
    bodyObserver.disconnect();
    detachGlobalListeners();
    if (uiListenersTimer !== null) {
      clearTimeout(uiListenersTimer);
      uiListenersTimer = null;
    }
    active = false;
  }

  function startObservers() {
    stopObservers();
    active = true;
    attachGlobalListeners();

    const formSheld = document.getElementById('form_sheld');
    if (formSheld) {
      resizeObserver.observe(formSheld);
      mutationObserver.observe(formSheld, {
        childList: true,
        subtree: true,
        attributes: true,
        characterData: true,
      });

      const parent = formSheld.parentElement;
      if (parent) {
        mutationObserver.observe(parent, {
          attributes: true,
          attributeFilter: ['style', 'class'],
        });
      }

      updateFormSheldHeight();
    }
  }

  const bodyObserver = new MutationObserver((mutations) => {
    for (const mutation of mutations) {
      if (mutation.addedNodes.length) {
        for (const node of mutation.addedNodes) {
          if (
            node.id === 'form_sheld' ||
            (node.nodeType === 1 && node.querySelector && node.querySelector('#form_sheld'))
          ) {
            setTimeout(startObservers, 50);
            return;
          }
        }
      }
    }

    const formSheld = document.getElementById('form_sheld');
    if (formSheld && !isInitialized) {
      setTimeout(startObservers, 50);
    }
  });

  function onTextAreaInput() {
    updateFormSheldHeight();
    setTimeout(updateFormSheldHeight, 10);
    setTimeout(updateFormSheldHeight, 100);
  }

  function setupTextAreaListener() {
    const textArea = document.getElementById('send_textarea');
    if (textArea) {
      textArea.removeEventListener('input', onTextAreaInput);
      textArea.addEventListener('input', onTextAreaInput);
    }
  }

  // Правка Lorekeeper (A2-01): глобальные слушатели снимаются в stop — при
  // выключенной теме они давали форс-рефлоу на resize/orientationchange без
  // потребителя переменной.
  let globalListenersAttached = false;
  function onOrientationChange() {
    updateFormSheldHeight();
    setTimeout(updateFormSheldHeight, 100);
    setTimeout(updateFormSheldHeight, 500);
  }
  function attachGlobalListeners() {
    if (globalListenersAttached) return;
    globalListenersAttached = true;
    window.addEventListener('resize', updateFormSheldHeight);
    window.addEventListener('orientationchange', onOrientationChange);
  }
  function detachGlobalListeners() {
    if (!globalListenersAttached) return;
    globalListenersAttached = false;
    window.removeEventListener('resize', updateFormSheldHeight);
    window.removeEventListener('orientationchange', onOrientationChange);
  }

  let uiListenersTimer = null;

  function setupUIListeners() {
    document.querySelectorAll('#qr--bar .qr--option').forEach((button) => {
      button.addEventListener('click', () => {
        setTimeout(updateFormSheldHeight, 10);
        setTimeout(updateFormSheldHeight, 100);
      });
    });

    const optionsButton = document.getElementById('options_button');
    if (optionsButton) {
      optionsButton.addEventListener('click', () => {
        setTimeout(updateFormSheldHeight, 10);
        setTimeout(updateFormSheldHeight, 100);
      });
    }
  }

  function startAll() {
    startObservers();
    // Правка Lorekeeper: `#form_sheld` — прямой ребёнок `#sheld`, его замену видно
    // по детям родителя. Наблюдение за всем body будило колбэк на каждом кадре
    // стрима (ST переписывает сообщение в чате целиком).
    const formSheldParent = document.getElementById('form_sheld')?.parentElement;
    bodyObserver.observe(formSheldParent ?? document.body, {
      childList: true,
      subtree: !formSheldParent,
    });
    setupTextAreaListener();
    updateFormSheldHeight();
    // Кнопки панели рендерятся позже — привязываем их обработчики с задержкой.
    if (uiListenersTimer !== null) {
      clearTimeout(uiListenersTimer);
    }
    uiListenersTimer = setTimeout(setupUIListeners, 1000);
  }

  // Правка Lorekeeper (A2-01): при выключенной теме монитор не поднимается
  // вовсе; после включения темы его запускает toggleCss через контроллер.
  if (themeEnabled()) {
    document.addEventListener('DOMContentLoaded', () => {
      startAll();
      setTimeout(updateFormSheldHeight, 100);
      setTimeout(updateFormSheldHeight, 500);
      setTimeout(updateFormSheldHeight, 1000);
    });

    window.addEventListener('load', () => {
      startAll();
      setTimeout(updateFormSheldHeight, 500);
    });

    startAll();
  }

  return {
    update: updateFormSheldHeight,
    // Правка Lorekeeper: start поднимает монитор целиком (наблюдатели +
    // слушатели — без них toggleCss не смог бы запустить его после включения
    // темы), stop снимает все подписки.
    start: startAll,
    stop: stopObservers,
  };
}
