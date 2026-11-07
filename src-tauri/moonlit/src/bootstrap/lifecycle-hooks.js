const domReadyHandlers = new Set();
let hasRunDomReadyHandlers = false;

function defer(callback) {
  if (typeof queueMicrotask === 'function') {
    queueMicrotask(callback);
  } else {
    setTimeout(callback, 0);
  }
}

function invokeDomReadyHandler(handler) {
  try {
    handler();
  } catch (error) {
    console.error('Moonlit Echoes DOM ready handler failed', error);
  }
}

/**
 * Register a handler to run once the DOM is ready.
 * If the DOM is already ready, the handler runs on the next tick so module
 * initialization can finish before any startup work executes.
 *
 * @param {Function} handler - Function to invoke when DOM is ready.
 */
export function registerDomReadyHandler(handler) {
  if (typeof handler !== 'function') {
    return;
  }

  if (document.readyState === 'loading') {
    domReadyHandlers.add(handler);
  } else {
    defer(() => invokeDomReadyHandler(handler));
  }
}

function runDomReadyHandlers() {
  if (hasRunDomReadyHandlers) {
    return;
  }

  hasRunDomReadyHandlers = true;
  domReadyHandlers.forEach((handler) => {
    invokeDomReadyHandler(handler);
  });
  domReadyHandlers.clear();
}

if (document.readyState === 'loading') {
  document.addEventListener('DOMContentLoaded', runDomReadyHandlers, { once: true });
} else {
  defer(runDomReadyHandlers);
}
