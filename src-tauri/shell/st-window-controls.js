/**
 * Window buttons and dragging on top of the SillyTavern page. The main window has no
 * system frame, and the ST page is foreign: without this it could be neither moved,
 * nor minimized, nor closed. The script is injected by an init script into every page
 * of the window, so it checks itself that it runs on the ST page.
 */
(() => {
  if (window !== window.top || window.__LK_WINDOW_CONTROLS__) return;
  window.__LK_WINDOW_CONTROLS__ = true;

  var ru = window.__LK_SYSTEM_LOCALE__ !== 'en';
  var TEXT = {
    minimize: ru ? 'Свернуть' : 'Minimize',
    maximize: ru ? 'Развернуть' : 'Maximize',
    restore: ru ? 'Восстановить' : 'Restore',
    close: ru ? 'Закрыть' : 'Close',
  };
  var ICON_MAX = '<path d="M1 1h8v8H1z"/>';
  var ICON_RESTORE = '<path d="M3 1h6v6M1 3h6v6H1z"/>';

  function invoke(cmd) {
    var ipc = window.__TAURI_INTERNALS__;
    if (!ipc) return Promise.reject(new Error('IPC недоступен'));
    return ipc.invoke(`plugin:window|${cmd}`, { label: 'main' });
  }

  function mount() {
    // The ST page is recognized by its top bar; the app's own pages
    // (launcher, settings) draw their buttons in WindowControls.svelte.
    if (!document.getElementById('top-settings-holder')) return;

    var style = document.createElement('style');
    style.textContent = `
      #lk-drag-strip { position: fixed; top: 0; left: 0; right: 0; height: var(--topBarBlockSize, 40px); z-index: 3000; }
      #lk-window-controls { position: fixed; top: 0; right: 0; z-index: 2147483000; display: flex; }
      #lk-window-controls button { display: grid; width: 46px; height: 34px; place-items: center; padding: 0; margin: 0;
        border: 0; border-radius: 0; background: transparent; box-shadow: none; color: rgba(255, 255, 255, 0.72); cursor: default; }
      #lk-window-controls button:hover { background: rgba(255, 255, 255, 0.08); color: #fff; }
      #lk-window-controls button.lk-close:hover { background: #e81123; color: #fff; }
      #lk-window-controls svg { fill: none; stroke: currentColor; stroke-width: 1; }
    `;
    document.head.appendChild(style);

    // A strip under ST's top bar: drags the window where the bar is absent (at the sides
    // of the narrow column). The bar itself drags too, but only by empty spots — Tauri
    // starts a drag only when the clicked target itself carries the attribute.
    var strip = document.createElement('div');
    strip.id = 'lk-drag-strip';
    strip.setAttribute('data-tauri-drag-region', '');
    document.body.appendChild(strip);
    for (var id of ['top-bar', 'top-settings-holder']) {
      document.getElementById(id)?.setAttribute('data-tauri-drag-region', '');
    }

    var bar = document.createElement('div');
    bar.id = 'lk-window-controls';
    bar.innerHTML =
      `<button type="button" data-cmd="minimize" title="${TEXT.minimize}" aria-label="${TEXT.minimize}">` +
      '<svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M1 5h8"/></svg></button>' +
      `<button type="button" data-cmd="toggle_maximize" title="${TEXT.maximize}" aria-label="${TEXT.maximize}">` +
      `<svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">${ICON_MAX}</svg></button>` +
      `<button type="button" class="lk-close" data-cmd="close" title="${TEXT.close}" aria-label="${TEXT.close}">` +
      '<svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M1 1l8 8M9 1 1 9"/></svg></button>';
    document.body.appendChild(bar);

    var maxButton = bar.querySelector('[data-cmd="toggle_maximize"]');
    function syncMaximized() {
      invoke('is_maximized')
        .then((maximized) => {
          var label = maximized ? TEXT.restore : TEXT.maximize;
          maxButton.title = label;
          maxButton.setAttribute('aria-label', label);
          maxButton.querySelector('svg').innerHTML = maximized ? ICON_RESTORE : ICON_MAX;
        })
        .catch(() => {});
    }
    bar.addEventListener('click', (event) => {
      var button = event.target.closest('button[data-cmd]');
      if (!button) return;
      invoke(button.dataset.cmd).catch((e) => console.error('[cloufam] кнопка окна:', e));
    });
    window.addEventListener('resize', syncMaximized);
    syncMaximized();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', mount, { once: true });
  } else {
    mount();
  }
})();

