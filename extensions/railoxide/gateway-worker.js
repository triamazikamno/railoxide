import init, { GatewayClient } from './dapp_gateway_protocol.js';
import { configuration } from './gateway-config.js';
import { createPageBridge } from './gateway-page-bridge.js';

const ports = new Set();
const tabContexts = new Map();
const encoder = new TextEncoder();
const decoder = new TextDecoder('utf-8', { fatal: true });
const now = () => BigInt(Math.floor(performance.now()));
// Protocol MAX_MESSAGE_LEN is 32 MiB. Synchronous fragment sends cannot drain
// bufferedAmount, so allow one complete message plus framing, with a fixed bound.
const MAX_BUFFERED_BYTES = 34 * 1024 * 1024;
// Chromium can delay opening by up to five seconds after failed WebSockets.
// Allow that backoff to finish instead of cancelling another healthy attempt.
const WEBSOCKET_OPEN_TIMEOUT_MS = 10_000;
let epoch = 0;
let owner;
let status = 'disconnected';
let configuredEndpoint = '';
let providerPreferences = { takeover: false, metamask: false };
let view = 'popup';
let viewError = false;
let paired = false;
const visibleViews = new Set();
function walletViewVisible() {
  return [...visibleViews].some(port => ports.has(port));
}
async function applyView(value) {
  await chrome.action.setPopup({ popup: value === 'sidepanel' ? '' : 'index.html' });
  await chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: value === 'sidepanel' });
}
const pageBridge = createPageBridge({ chrome, crypto, send: command,
  session: () => owner?.candidate?.established && current(owner.candidate) ? owner.candidate : null,
  preferences: () => preferencesReady.then(() => providerPreferences),
});
let backoff = 1;
let retryTimer;
let storage = Promise.resolve();
let commandPending = false;
let uiSnapshot = { type: 'ui_snapshot', version: 1, generation: 0, locked: true, accounts: [], pending_connects: [], pending_requests: [] };
const notificationUrl = chrome.runtime.getURL('index.html?mode=notification');
let notificationWindow;
let notificationToken = 0;
let notificationQueue = Promise.resolve();
let notificationSeen = new Set();
let notificationDiscovered = false;
// State messages purge presentation before the authoritative pending snapshot arrives.
// Retain only identities so redaction neither closes the window nor resets dismissal.
let notificationPending = [];
function wantsNotification() {
  return notificationPending.length > 0 && !walletViewVisible();
}
async function removeNotification(window) {
  const contexts = await chrome.runtime.getContexts({ documentUrls: [notificationUrl] });
  const current = await chrome.windows.get(window.id, { populate: true }).catch(() => null);
  // Relinquish ownership if the tab moved; never close its former window's other tabs.
  if (current?.tabs?.length !== 1 || current.tabs[0].id !== window.tabId) return true;
  if (!contexts.some(context => context.documentUrl === notificationUrl &&
      context.windowId === window.id && context.tabId === window.tabId)) return false;
  return chrome.windows.remove(window.id).then(() => true, () => false);
}
function updateNotification(rediscover = false, authoritative = false) {
  const count = uiSnapshot.pending_connects.length + uiSnapshot.pending_requests.length;
  void chrome.action.setBadgeText({ text: count ? String(count) : '' }).catch(() => {});
  if (authoritative) notificationPending = [
    ...uiSnapshot.pending_connects.map(request => `connect:${request.request_id}`),
    ...uiSnapshot.pending_requests.map(request => `request:${request.request_id}`),
  ];
  const ids = notificationPending;
  const shouldOpen = wantsNotification() && ids.some(id => !notificationSeen.has(id));
  notificationSeen = new Set(ids);
  if (!wantsNotification()) notificationToken += 1;
  const token = notificationToken;
  notificationQueue = notificationQueue.then(async () => {
    if (rediscover || shouldOpen) notificationDiscovered = false;
    if (!notificationDiscovered) {
      // Tab URLs are redacted without tabs permission, including our extension's own URLs.
      const contexts = await chrome.runtime.getContexts({ documentUrls: [notificationUrl] });
      const matches = [];
      for (const context of contexts) {
        if (context.documentUrl !== notificationUrl || !Number.isInteger(context.windowId) || context.windowId < 0 ||
            !Number.isInteger(context.tabId) || context.tabId < 0 || matches.some(window => window.id === context.windowId)) continue;
        const window = await chrome.windows.get(context.windowId, { populate: true }).catch(() => null);
        if (window?.tabs?.length === 1 && window.tabs[0].id === context.tabId) {
          matches.push({ id: context.windowId, tabId: context.tabId });
        }
      }
      // A new request may replace a handle whose tab navigated away. Never close
      // that page. A delayed notification port will reconcile any startup duplicate.
      notificationWindow = matches.find(window => window.id === notificationWindow?.id && window.tabId === notificationWindow?.tabId)
        ?? matches[0] ?? (shouldOpen ? undefined : notificationWindow);
      for (const duplicate of matches) {
        if (duplicate !== notificationWindow) await removeNotification(duplicate);
      }
      notificationDiscovered = true;
    }
    if (!wantsNotification() || token !== notificationToken) {
      if (notificationWindow) {
        const window = notificationWindow;
        if (await removeNotification(window) && notificationWindow === window) notificationWindow = undefined;
      }
      return;
    }
    if (!shouldOpen || notificationWindow) return;
    const window = await chrome.windows.create({ url: notificationUrl, type: 'popup', width: 440, height: 600 });
    if (window?.id === undefined || window.tabs?.length !== 1 || window.tabs[0].id === undefined) return;
    const created = { id: window.id, tabId: window.tabs[0].id };
    // The document context may not exist yet when create resolves. Keep ownership
    // until a later snapshot or the loaded notification's port can finish cleanup.
    notificationWindow = created;
    if (token !== notificationToken || !wantsNotification()) {
      if (await removeNotification(created) && notificationWindow === created) notificationWindow = undefined;
    }
  }).catch(() => {}); // The badge and manual desktop control remain usable.
}
chrome.windows.onRemoved.addListener(id => {
  if (notificationWindow?.id === id) notificationWindow = undefined;
});
function publishSnapshot(snapshot, authoritative = true) {
  uiSnapshot = snapshot;
  updateNotification(false, authoritative);
  for (const port of ports) {
    try { port.postMessage(snapshotFor(port)); } catch { ports.delete(port); tabContexts.delete(port); }
  }
}
function snapshotFor(port) {
  const tab = uiSnapshot.locked ? null : tabContexts.get(port)?.tab;
  return { ...uiSnapshot, current_tab_origin: tab ? `${tab.origin}/` : null, current_tab_token: tab?.token ?? null };
}
async function updateTab(port) {
  const context = tabContexts.get(port);
  if (!context) return;
  const revision = ++context.revision;
  context.tab = null;
  port.postMessage(snapshotFor(port));
  try {
    const [tab] = await chrome.tabs.query({ active: true, windowId: context.windowId });
    if (!Number.isInteger(tab?.id)) return;
    const frame = await chrome.webNavigation.getFrame({ tabId: tab.id, frameId: 0 });
    if (tabContexts.get(port) !== context || context.revision !== revision ||
        frame?.documentLifecycle !== 'active' || !frame.documentId) return;
    const url = new URL(frame.url);
    if (!['http:', 'https:'].includes(url.protocol)) return;
    context.tab = { id: tab.id, documentId: frame.documentId, origin: url.origin, token: crypto.randomUUID() };
    port.postMessage(snapshotFor(port));
  } catch { /* A closed tab or a browser page has no connect action. */ }
}
function updateTabs() {
  for (const port of tabContexts.keys()) void updateTab(port);
}
chrome.tabs?.onActivated?.addListener(updateTabs);
chrome.webNavigation.onCommitted.addListener(event => { if (event.frameId === 0) updateTabs(); });
chrome.tabs?.onRemoved?.addListener(updateTabs);
function purgeSnapshot(authoritative = true) {
  publishSnapshot({ type: 'ui_snapshot', version: 1, generation: 0, locked: true, accounts: [], pending_connects: [], pending_requests: [] }, authoritative);
}
const ready = Promise.all([
  chrome.storage.local.setAccessLevel({ accessLevel: 'TRUSTED_CONTEXTS' }),
  init({ module_or_path: chrome.runtime.getURL('dapp_gateway_protocol_bg.wasm') }),
]);
const preferencesReady = ready.then(() => stored(async () => {
  const saved = await chrome.storage.local.get('gatewayPreferences');
  providerPreferences = { takeover: saved.gatewayPreferences?.takeover === true, metamask: saved.gatewayPreferences?.metamask === true };
  const savedView = saved.gatewayPreferences?.view === 'sidepanel' ? 'sidepanel' : 'popup';
  try {
    await applyView(savedView);
    view = savedView;
  } catch { viewError = true; }
  publish(status);
}));
void preferencesReady.catch(() => { viewError = true; publish(status); });
// Queue storage operations across owners. A new owner's load cannot race an old write.
function stored(action) {
  const result = storage.then(action);
  storage = result.catch(() => {});
  return result;
}
function current(session) {
  const discovery = session.discovery ?? session;
  return owner === discovery && discovery.epoch === epoch &&
    (!session.discovery || discovery.candidate === session);
}
function dispose(session, result = 'cancelled') {
  if (!session) return;
  if (session.discovery.candidate === session) session.discovery.candidate = null;
  clearTimeout(session.openDeadline);
  clearTimeout(session.deadline);
  clearInterval(session.heartbeat);
  clearInterval(session.assembly);
  session.finish(result); // Settle discovery even when receive is awaiting storage.
  session.client?.close();
  session.client?.free();
  session.client = null;
  session.socket?.close();
}
function clearCredentials(discovery) {
  discovery.code?.fill(0);
  discovery.code = null;
  wipe(discovery.record);
  discovery.record = null;
}
function stateMessage(reported = status) {
  return { type: 'state', status: reported, endpoint: configuredEndpoint, paired, ...providerPreferences, view, viewError };
}
function publish(next) {
  status = next;
  for (const port of ports) {
    try { port.postMessage(stateMessage()); } catch { ports.delete(port); }
  }
}
function retire() {
  pageBridge.retire();
  purgeSnapshot();
  epoch += 1;
  clearTimeout(retryTimer);
  if (owner) {
    clearCredentials(owner);
    dispose(owner.candidate);
    owner = null;
  }
  publish('disconnected'); // Purge all wallet-derived state on every session change.
}
function scheduleRetry() {
  const expected = epoch;
  const delay = backoff;
  backoff = Math.min(backoff * 2, 30);
  retryTimer = setTimeout(() => { if (epoch === expected) void connect(); }, delay * 1000);
  // Survives worker termination. Chrome 120 supports the 30-second minimum.
  void chrome.alarms.create('gateway-reconnect', { delayInMinutes: 0.5 });
}
function failed(session, reason, retry = false) {
  if (!current(session)) return;
  if (session.discovery && !session.established) {
    dispose(session, reason);
    return;
  }
  retire();
  publish(reason);
  if (retry) scheduleRetry();
}
function send(session, bytes) {
  if (!current(session) || session.sendFailed || session.socket.readyState !== WebSocket.OPEN ||
      session.socket.bufferedAmount + bytes.length > MAX_BUFFERED_BYTES) throw new Error('Gateway send failed');
  session.socket.send(bytes);
}
function command(session, message) {
  if (session.sendFailed) throw new Error('Gateway send failed');
  try {
    const value = typeof message === 'string' ? { type: message, version: 1 } : message;
    for (const frame of session.client.sealMessage(encoder.encode(JSON.stringify(value)))) {
      send(session, frame);
    }
  } catch {
    // Sealing advances record counters. Never continue this session after a partial send,
    // including unregister attempts during teardown.
    session.sendFailed = true;
    failed(session, 'disconnected', true);
    throw new Error('Gateway send failed');
  }
}
function isCredential(record) {
  const bytes = (value, size) => Array.isArray(value) && value.length === size &&
    value.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255);
  return record && record.version === 1 && bytes(record.peerId, 16) && bytes(record.secret, 32);
}
function wipe(record) {
  record?.peerId?.fill?.(0);
  record?.secret?.fill?.(0);
}
async function receive(session, bytes) {
  if (!current(session)) return;
  try {
    if (!session.client.isAuthenticated()) {
      const response = session.client.receiveHandshake(bytes);
      if (response.length) send(session, response);
      if (session.client.lastEvent() === 1) {
        const peerId = session.client.pendingPeerId();
        const secret = session.client.exportPendingCredentialForStorage();
        const record = { version: 1, peerId: Array.from(peerId), secret: Array.from(secret) };
        try {
          await stored(async () => {
            if (!current(session)) return;
            await chrome.storage.local.set({ gatewayCredential: record });
            paired = true;
          });
        } catch {
          if (current(session)) failed(session.discovery, 'storage_failed');
          return;
        } finally {
          peerId.fill(0);
          secret.fill(0);
          wipe(record);
        }
        if (!current(session)) return;
        send(session, session.client.acknowledgeCredential());
      }
      if (session.client.isAuthenticated()) {
        clearTimeout(session.deadline);
        backoff = 1;
        void chrome.alarms.create('gateway-reconnect', { periodInMinutes: 0.5 });
        command(session, 'get_state');
        publish('paired');
        session.lastReceived = performance.now();
        session.heartbeat = setInterval(() => {
          if (!current(session)) return;
          if (performance.now() - session.lastReceived > 60000) {
            failed(session, 'disconnected', true);
            return;
          }
          try { command(session, 'heartbeat'); } catch { failed(session, 'disconnected', true); }
        }, 20000);
        session.established = true;
        pageBridge.authenticated();
        clearCredentials(session.discovery);
        session.finish('authenticated');
        session.assembly = setInterval(() => {
          if (!current(session)) return;
          try { session.client.expireAssembly(now()); } catch { failed(session, 'disconnected', true); }
        }, 1000);
      }
      return;
    }
    const plain = session.client.receiveFrame(bytes, now());
    if (!plain) return;
    let message;
    try { message = JSON.parse(decoder.decode(plain)); } finally { plain.fill(0); }
    if (!current(session)) return;
    if (message.version !== 1 || message.type === 'unsupported') {
      failed(session, 'version_failed');
      return;
    }
    session.lastReceived = performance.now();
    if (message.type === 'heartbeat') return;
    if (message.type === 'ui_snapshot') {
      if (message.generation !== session.generation || message.locked !== session.locked || !Array.isArray(message.accounts) ||
          !Array.isArray(message.pending_connects) || !Array.isArray(message.pending_requests)) return;
      if (message.locked && (message.accounts.length ||
          message.pending_connects.some(prompt => prompt.accounts?.length) ||
          message.public_view?.selected_account || message.public_view?.selected_chain || message.public_view?.balances?.length || message.public_view?.drafts?.length || message.permissions?.length)) return;
      publishSnapshot({ ...message, public_view: message.locked ? null : message.public_view,
        permissions: message.locked ? [] : (message.permissions ?? []),
        ui_error: message.locked ? null : message.ui_error, pending_requests: message.pending_requests.map(request => ({
        request_id: request.request_id, url: request.url,
        needs_unlock: message.locked || request.needs_unlock,
        summary: message.locked || request.needs_unlock ? null : request.summary,
      })) });
      return;
    }
    if (message.type === 'provider_state' || message.type === 'provider_response') {
      pageBridge.receive(session, message);
      return;
    }
    if (message.type !== 'state' || typeof message.locked !== 'boolean' ||
        !Number.isSafeInteger(message.generation) || message.generation < 0 ||
        message.generation < session.generation ||
        (message.generation === session.generation && session.locked !== message.locked)) {
      failed(session, 'disconnected');
      return;
    }
    const generationChanged = session.generation !== message.generation;
    if (generationChanged) purgeSnapshot(false);
    session.generation = message.generation;
    session.locked = message.locked;
    pageBridge.lockState(message.locked, generationChanged);
    publish(message.locked ? 'locked' : 'unlocked');
  } catch (error) {
    // WASM exposes fixed errors. Never stringify a library error or received payload.
    failed(session, error === 'Incompatible gateway protocol version' ? 'version_failed' : 'auth_failed');
  } finally { bytes.fill(0); }
}
function attempt(discovery, endpoint) {
  return new Promise(resolve => {
    const session = { discovery, client: null, socket: null, established: false,
      generation: -1, locked: null, queued: 0, chain: Promise.resolve(), finish: resolve };
    discovery.candidate = session;
    try {
      const socket = new WebSocket(endpoint);
      session.socket = socket;
      socket.binaryType = 'arraybuffer';
      session.openDeadline = setTimeout(() => {
        if (current(session)) failed(session, 'disconnected');
      }, WEBSOCKET_OPEN_TIMEOUT_MS);
      socket.onopen = () => {
        if (!current(session)) return;
        clearTimeout(session.openDeadline);
        try {
          if (discovery.code) {
            const code = discovery.code.slice();
            try { session.client = GatewayClient.pair(code); } finally { code.fill(0); }
          } else {
            const peer = new Uint8Array(discovery.record.peerId);
            const secret = new Uint8Array(discovery.record.secret);
            try { session.client = GatewayClient.reconnect(peer, secret); }
            finally { peer.fill(0); secret.fill(0); }
          }
          session.deadline = setTimeout(() => {
            if (current(session)) failed(session, 'auth_failed');
          }, 10000);
          send(session, session.client.initialHello());
        } catch { failed(session, 'auth_failed'); }
      };
      socket.onerror = socket.onclose = () => {
        if (current(session)) failed(session, session.established ? 'disconnected' :
          session.client ? 'auth_failed' : 'disconnected', session.established);
      };
      socket.onmessage = event => {
        if (!current(session)) return;
        if (!session.client || !(event.data instanceof ArrayBuffer) || event.data.byteLength > 65535 ||
            (!session.client.isAuthenticated() && event.data.byteLength > 1024) || ++session.queued > 64) {
          failed(session, 'auth_failed');
          return;
        }
        const bytes = new Uint8Array(event.data);
        session.chain = session.chain.then(() => receive(session, bytes)).finally(() => { bytes.fill(0); session.queued -= 1; });
      };
    } catch { failed(session, 'disconnected'); }
  });
}
async function connect(code = null) {
  retire();
  const discovery = { epoch, code, record: null, candidate: null };
  owner = discovery;
  publish('connecting');
  let record;
  try {
    let saved;
    try {
      await ready;
      if (!current(discovery)) return;
      saved = await stored(() => current(discovery) ?
        chrome.storage.local.get(['gatewayCredential', 'gatewayPreferences']) : {});
    } catch { failed(discovery, 'storage_failed'); return; }
    record = saved.gatewayCredential;
    paired = isCredential(record) === true; // A missing record must report a boolean.
    if (!current(discovery)) return;
    discovery.record = record;
    let config;
    try {
      config = configuration(saved.gatewayPreferences?.endpoint ?? '');
      configuredEndpoint = config.value;
      providerPreferences = { takeover: saved.gatewayPreferences?.takeover === true, metamask: saved.gatewayPreferences?.metamask === true };
      publish('connecting');
      if (config.permission && !await chrome.permissions.contains({ origins: [config.permission] })) {
        failed(discovery, 'config_failed');
        return;
      }
    } catch { failed(discovery, 'config_failed'); return; }
    if (!current(discovery)) return;
    if (!code && !isCredential(record)) { failed(discovery, 'disconnected'); return; }
    const result = await attempt(discovery, config.endpoint);
    if (!current(discovery) || result === 'authenticated') return;
    failed(discovery, result, !code && result === 'disconnected');
  } catch { failed(discovery, 'disconnected', !code); }
  finally { wipe(record); clearCredentials(discovery); }
}
// Saving an endpoint retires the session; pairing codes wait for the saved value.
function saveEndpoint(config, code = null) {
  commandPending = true;
  retire();
  const expected = epoch;
  void (async () => {
    await ready;
    if (epoch !== expected) return;
    if (config.permission && !await chrome.permissions.contains({ origins: [config.permission] })) {
      if (epoch === expected) publish('config_failed');
      return;
    }
    await stored(async () => {
      if (epoch !== expected) return;
      const saved = await chrome.storage.local.get('gatewayPreferences');
      if (epoch === expected) await chrome.storage.local.set({ gatewayPreferences: { ...saved.gatewayPreferences, endpoint: config.value } });
    });
    commandPending = false;
    if (epoch === expected) void connect(code);
  })().catch(() => { if (epoch === expected) publish('storage_failed'); }).finally(() => { if (epoch === expected) commandPending = false; });
}

chrome.runtime.onConnect.addListener(port => {
  if (pageBridge.connect(port)) return;
  const sender = port.sender;
  let url;
  try { url = new URL(sender?.url); } catch { port.disconnect(); return; }
  if (port.name !== 'gateway-ui-v1' || sender?.id !== chrome.runtime.id ||
      url.protocol !== 'chrome-extension:' || url.hostname !== chrome.runtime.id || url.pathname !== '/index.html' ||
      !['', '?mode=window', '?mode=notification', '?mode=sidepanel'].includes(url.search) || url.hash) { port.disconnect(); return; }
  ports.add(port);
  if (url.search === '?mode=notification') updateNotification(true);
  port.postMessage(snapshotFor(port));
  port.postMessage(stateMessage());
  port.onDisconnect.addListener(() => {
    ports.delete(port); // Documents never own the socket.
    tabContexts.delete(port);
    if (visibleViews.delete(port)) updateNotification();
  });
  port.onMessage.addListener(message => {
    if (!ports.has(port) || !message || typeof message.type !== 'string') return;
    if (message.type === 'sidepanel_presence' || message.type === 'popup_presence') {
      const expectedSearch = message.type === 'sidepanel_presence' ? '?mode=sidepanel' : '';
      if (url.search !== expectedSearch || message.ready !== true || typeof message.visible !== 'boolean') return;
      if (message.visible) visibleViews.add(port);
      else visibleViews.delete(port);
      updateNotification(message.visible);
      return;
    }
    if (message.type === 'tab_context' && Number.isInteger(message.window_id) && message.window_id >= 0) {
      tabContexts.set(port, { windowId: message.window_id, tab: null, revision: 0 });
      void updateTab(port);
      return;
    }
    if (message.type === 'public_view') {
      const session = owner?.candidate;
      if (!session?.established || !current(session) || session.locked ||
          message.generation !== session.generation || uiSnapshot.generation !== session.generation) return;
      const input = message.command;
      if (!input || typeof input.type !== 'string') return;
      let value;
      if (input.type === 'select_account' && uiSnapshot.accounts.some(account => account.uuid === input.public_account_uuid)) {
        value = { type: input.type, public_account_uuid: input.public_account_uuid };
      } else if (input.type === 'select_chain' && uiSnapshot.chains?.some(chain => chain.id === input.chain_id)) {
        value = { type: input.type, chain_id: input.chain_id };
      } else if (input.type === 'refresh_balances') {
        value = { type: input.type };
      } else if (input.type === 'draft') {
        const draft = input.command;
        if (!draft || typeof draft.action !== 'string') return;
        const bounded = (value, length) => typeof value === 'string' && value.length <= length;
        if (['create', 'update'].includes(draft.action)) {
          const data = draft.input;
          if (!data || data.account !== uiSnapshot.public_view?.selected_account || data.chain_id !== uiSnapshot.public_view?.selected_chain ||
              !['send', 'shield'].includes(data.kind) || !bounded(data.asset, 128) || !bounded(data.amount, 100) || !bounded(data.recipient, 1024) ||
              !(data.address_book_entry === null || bounded(data.address_book_entry, 128)) ||
              typeof data.max !== 'boolean' || typeof data.mimic_railway !== 'boolean') return;
          const fee = data.fee;
          if (!fee || !['slow', 'normal', 'fast', 'custom'].includes(fee.mode)) return;
          if (fee.mode === 'custom' && (!bounded(fee.max_fee_gwei, 100) || !bounded(fee.priority_fee_gwei, 100))) return;
          const prepared = { account: data.account, chain_id: data.chain_id, kind: data.kind, asset: data.asset, amount: data.amount,
            recipient: data.recipient, address_book_entry: data.address_book_entry, max: data.max, mimic_railway: data.mimic_railway,
            fee: fee.mode === 'custom' ? { mode: fee.mode, max_fee_gwei: fee.max_fee_gwei, priority_fee_gwei: fee.priority_fee_gwei } : { mode: fee.mode } };
          if (draft.action === 'create') {
            if (!bounded(draft.request_id, 64) || !draft.request_id) return;
            value = { type: 'draft', command: { action: 'create', request_id: draft.request_id, input: prepared } };
          } else {
            if (!uiSnapshot.public_view?.drafts?.some(current => current.draft_id === draft.draft_id) || !Number.isSafeInteger(draft.revision) || draft.revision < 0) return;
            value = { type: 'draft', command: { action: 'update', draft_id: draft.draft_id, revision: draft.revision, input: prepared } };
          }
        } else if (['submit', 'cancel', 'dismiss'].includes(draft.action)) {
          if (!uiSnapshot.public_view?.drafts?.some(current => current.draft_id === draft.draft_id)) return;
          const command = { action: draft.action, draft_id: draft.draft_id };
          if (draft.action === 'submit') {
            if (!Number.isSafeInteger(draft.revision) || draft.revision < 0) return;
            command.revision = draft.revision;
          }
          value = { type: 'draft', command };
        }
      } else if (['revoke_permission', 'reissue_permission'].includes(input.type) &&
          uiSnapshot.permissions?.some(permission => permission.permission_id === input.permission_id)) {
        value = { type: input.type, permission_id: input.permission_id };
        if (input.type === 'reissue_permission') {
          if (!uiSnapshot.accounts.some(account => account.uuid === input.public_account_uuid)) return;
          value.public_account_uuid = input.public_account_uuid;
        }
      } else if (input.type === 'connect_tab') {
        const context = tabContexts.get(port);
        const tab = context?.tab;
        if (!tab || tab.token !== message.tab_token) return;
        const generation = session.generation;
        const stillCurrent = () => ports.has(port) && tabContexts.get(port) === context && context.tab === tab &&
          current(session) && session.generation === generation && !session.locked;
        void chrome.tabs.query({ active: true, windowId: context.windowId }).then(([active]) => {
          if (active?.id !== tab.id || !stillCurrent()) return false;
          return pageBridge.connectTab(tab.id, tab.documentId, tab.origin, stillCurrent);
        }).then(sent => {
          if (!sent && stillCurrent()) {
            port.postMessage({ ...snapshotFor(port), ui_error: 'This tab is not ready to connect. Reload the page and try again.' });
          }
        }).catch(() => {});
        return;
      }
      if (value) {
        try { command(session, { type: 'public_view', version: 1, generation: session.generation, command: value }); }
        catch { failed(session, 'disconnected', true); }
      }
      return;
    }
    const viewRequest = message.type === 'preferences' && message.key === 'view' && ['popup', 'sidepanel'].includes(message.value);
    const replyView = (success, opened) => {
      if (!Number.isSafeInteger(message.request_id) || !ports.has(port)) return;
      try { port.postMessage({ type: 'view_result', request_id: message.request_id, success, ...(opened === undefined ? {} : { opened }) }); } catch { /* The requesting view closed. */ }
    };
    if (commandPending) {
      if (viewRequest) replyView(false);
      return;
    }
    if (viewRequest) {
      commandPending = true;
      let success = false;
      const handoff = message.handoff_window_id !== undefined;
      let opened = handoff ? false : undefined;
      void preferencesReady.then(() => stored(async () => {
        const previous = view;
        try {
          const saved = await chrome.storage.local.get('gatewayPreferences');
          await applyView(message.value);
          await chrome.storage.local.set({ gatewayPreferences: { ...saved.gatewayPreferences, view: message.value } });
          view = message.value;
          viewError = false;
          success = true;
        } catch {
          await applyView(previous).catch(() => {});
          viewError = true;
        }
        publish(status);
      })).then(async () => {
        if (!success || !handoff || !ports.has(port) || url.search !== '?mode=sidepanel' ||
            message.value !== 'popup' || !Number.isSafeInteger(message.request_id) || message.request_id <= 0 ||
            !Number.isSafeInteger(message.handoff_window_id) || message.handoff_window_id <= 0 ||
            typeof chrome.sidePanel?.close !== 'function' || typeof chrome.action.openPopup !== 'function') return;
        try {
          await chrome.sidePanel.close({ windowId: message.handoff_window_id });
          // Closing destroys the requesting document. The worker must finish the handoff.
          await chrome.action.openPopup({ windowId: message.handoff_window_id });
          opened = true;
        } catch { /* The saved mode remains available through the toolbar. */ }
      }).catch(() => { viewError = true; publish(status); }).finally(() => { commandPending = false; replyView(success, opened); });
      return;
    }
    if (['request_wallet_switch', 'summon_desktop', 'user_activity'].includes(message.type)) {
      const session = owner?.candidate;
      if (!session?.established || !current(session) || message.generation !== session.generation ||
          uiSnapshot.generation !== session.generation) return;
      const value = { type: message.type, version: 1, generation: session.generation };
      if (message.type === 'request_wallet_switch') {
        const prompt = uiSnapshot.pending_connects.find(prompt => prompt.request_id === message.request_id);
        if (!prompt?.wrong_wallet) return;
        value.request_id = prompt.request_id;
      }
      try { command(session, value); } catch { failed(session, 'disconnected', true); }
      return;
    }
    if (message.type === 'resolve_connect') {
      console.log('[RailOxide gateway] gateway connection approval received');
      const session = owner?.candidate;
      if (!session?.established || !current(session) || uiSnapshot.generation !== session.generation) {
        console.log('[RailOxide gateway] gateway connection approval ignored: inactive session');
        return;
      }
      const prompt = uiSnapshot.pending_connects.find(value => value.request_id === message.request_id);
      if (!prompt || (message.public_account_uuid !== null && (session.locked ||
          !prompt.accounts.some(account => account.uuid === message.public_account_uuid) ||
          !prompt.chains.some(chain => chain.id === message.chain_id)))) {
        console.log('[RailOxide gateway] gateway connection approval ignored: unavailable prompt or selection');
        return;
      }
      console.log('[RailOxide gateway] gateway connection approval accepted');
      try {
        command(session, { type: 'resolve_connect', version: 1, request_id: prompt.request_id,
          public_account_uuid: message.public_account_uuid, chain_id: message.chain_id });
        console.log('[RailOxide gateway] gateway connection approval sent');
      } catch {
        console.log('[RailOxide gateway] gateway connection approval send failed');
        failed(session, 'disconnected', true);
      }
      return;
    }
    if (message.type === 'retry') { void connect(); return; }
    if (message.type === 'unpair') {
      commandPending = true;
      paired = false; // The forgotten credential can never authenticate another session.
      retire();
      void chrome.alarms.clear('gateway-reconnect'); // Nothing left to reconnect with.
      void (async () => {
        await ready;
        await stored(() => chrome.storage.local.remove('gatewayCredential'));
        publish(status);
      })().catch(() => publish('storage_failed')).finally(() => { commandPending = false; });
      return;
    }
    if (message.type === 'pair' && typeof message.code === 'string' && /^\d{6}$/.test(message.code)) {
      if (message.endpoint === undefined) { void connect(encoder.encode(message.code)); return; }
      let config;
      try { config = configuration(message.endpoint); } catch { port.postMessage(stateMessage('config_failed')); return; }
      saveEndpoint(config, encoder.encode(message.code));
      return;
    }
    if (message.type === 'preferences' && ['takeover', 'metamask'].includes(message.key) && typeof message.value === 'boolean') {
      commandPending = true;
      void ready.then(() => stored(async () => {
        const saved = await chrome.storage.local.get('gatewayPreferences');
        const preferences = { ...saved.gatewayPreferences, [message.key]: message.value };
        await chrome.storage.local.set({ gatewayPreferences: preferences });
        providerPreferences = { takeover: preferences.takeover === true, metamask: preferences.metamask === true };
        publish(status);
      })).catch(() => publish('storage_failed')).finally(() => { commandPending = false; });
      return;
    }
    if (message.type !== 'configure') return;
    let config;
    try { config = configuration(message.endpoint); } catch { port.postMessage(stateMessage('config_failed')); return; }
    saveEndpoint(config);
  });
});
chrome.alarms.onAlarm.addListener(alarm => {
  if (alarm.name === 'gateway-reconnect' && !owner && !commandPending && status === 'disconnected') void connect();
});
chrome.runtime.onStartup.addListener(() => { if (!owner) void connect(); });
chrome.runtime.onInstalled.addListener(() => { if (!owner) void connect(); });
void connect();
