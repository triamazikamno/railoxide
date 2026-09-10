(() => {
  const channel = 'railoxide-provider-v1';
  const pending = new Set();
  let port;
  let stopped = false;
  let retry;
  let latestPreferences;
  let latestState;
  const post = value => window.postMessage({ channel, direction: 'provider', ...value }, '*');
  function reset() {
    pending.clear();
    latestState = { type: 'reset', error: { code: 4900, message: 'Desktop provider disconnected.' } };
    post(latestState);
  }
  function attach() {
    if (stopped || port) return;
    try {
      const next = chrome.runtime.connect({ name: 'gateway-provider-v1' });
      port = next;
      next.onMessage.addListener(message => {
        if (port !== next) return;
        if (message.type === 'response') {
          if (!pending.delete(message.id)) return;
        } else if (!['state', 'reset', 'preferences'].includes(message.type)) return;
        if (message.type === 'reset') pending.clear();
        if (message.type === 'preferences') latestPreferences = message;
        else if (message.type === 'state' || message.type === 'reset') latestState = message;
        post(message);
      });
      next.onDisconnect.addListener(() => {
        if (port !== next) return;
        port = null;
        reset();
        retry = setTimeout(attach, 1000);
      });
    } catch { reset(); retry = setTimeout(attach, 1000); }
  }
  window.addEventListener('message', event => {
    const message = event.data;
    if (event.source !== window || message?.channel !== channel || message.direction !== 'request') return;
    if (message.type === 'ready') {
      if (latestPreferences) post(latestPreferences);
      if (latestState) post(latestState);
      clearTimeout(retry);
      attach();
      return;
    }
    if (typeof message.id !== 'string' || message.id.length > 128 || pending.has(message.id)) return;
    if (!port || pending.size >= 128) {
      post({ type: 'response', id: message.id, error: { code: port ? -32005 : 4900,
        message: port ? 'Provider request capacity exceeded.' : 'Desktop provider disconnected.' } });
      return;
    }
    pending.add(message.id);
    // This channel is controlled by the page. Never pass its command or identity fields.
    try { port.postMessage({ id: message.id, method: message.method, params: message.params }); }
    catch { pending.delete(message.id); reset(); }
  });
  window.addEventListener('pagehide', () => {
    stopped = true;
    clearTimeout(retry);
    const previous = port;
    port = null;
    previous?.disconnect();
    reset();
  });
  window.addEventListener('pageshow', event => { if (event.persisted) { stopped = false; attach(); } });
  attach();
})();
