import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';
import vm from 'node:vm';
import { webcrypto } from 'node:crypto';

const source = await readFile(new URL('../../extensions/railoxide/gateway-worker.js', import.meta.url), 'utf8');
const configSource = await readFile(new URL('../../extensions/railoxide/gateway-config.js', import.meta.url), 'utf8');
const { configuration } = await import(`data:text/javascript,${encodeURIComponent(configSource)}`);
const bridgeSource = await readFile(new URL('../../extensions/railoxide/gateway-page-bridge.js', import.meta.url), 'utf8');
const { createPageBridge } = await import(`data:text/javascript,${encodeURIComponent(bridgeSource)}`);
const endpoint = port => `ws://127.0.0.1:${port}/`;
const credential = () => ({ version: 1, peerId: Array(16).fill(7), secret: Array(32).fill(9) });
const flush = async () => { await new Promise(setImmediate); };
function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

// Run the worker with Chrome, transport and WASM boundaries supplied by the host.
// The real endpoint configuration policy is used; no worker internals are exported.
async function worker(initial = {}, initialWindows = [], initialContextsVisible = true) {
  const data = structuredClone(initial);
  const sockets = [], clients = [], states = [], timers = new Map();
  let listener, command, alarm, nextTimer = 0, clockTime = 0, storageHook, createHook, toolbarHook, removed;
  const windows = structuredClone(initialWindows), created = [], badges = [], popups = [], panelBehaviors = [];
  let nextWindow = 100, contextsVisible = initialContextsVisible;
  const closeWindow = id => {
    const index = windows.findIndex(window => window.id === id);
    if (index >= 0) windows.splice(index, 1);
    removed?.(id);
  };
  const event = () => {
    const listeners = [];
    return { addListener(callback) { listeners.push(callback); }, fire(value) { for (const callback of listeners) callback(value); } };
  };
  const activeTabs = new Map([[17, { id: 1 }]]);
  const frames = new Map([[1, { documentId: 'browser-document', documentLifecycle: 'active', url: 'https://dapp.test/' }]]);
  const timer = (callback, delay) => { timers.set(++nextTimer, { callback, delay, due: clockTime + delay }); return nextTimer; };
  class Socket {
    static OPEN = 1;
    constructor(url) { this.url = url; this.readyState = 0; this.bufferedAmount = 0; this.sent = []; sockets.push(this); }
    open() { this.readyState = 1; this.onopen(); }
    message(value) { this.onmessage({ data: typeof value === 'number' ? Uint8Array.of(value).buffer : value }); }
    close() { this.readyState = 3; this.onclose?.(); }
    send(bytes) { assert.equal(this.readyState, 1); this.sent.push(Array.from(bytes)); this.bufferedAmount += bytes.length; }
  }
  class Client {
    static pair(code) { return new Client('pair', [code]); }
    static reconnect(peer, secret) { return new Client('reconnect', [peer, secret]); }
    constructor(mode, inputs) {
      this.mode = mode; this.inputs = inputs; this.copied = inputs.map(value => Array.from(value));
      this.authenticated = false; this.event = 0; clients.push(this);
    }
    isAuthenticated() { assert.ok(!this.freed); return this.authenticated; }
    initialHello() { return Uint8Array.of(10); }
    receiveHandshake(bytes) {
      assert.ok(!this.freed);
      this.event = 0;
      if (bytes[0] === 3) throw 'Incompatible gateway protocol version';
      if (bytes[0] === 4) throw 'Authentication failed';
      if (bytes[0] === 1) this.event = 1;
      if (bytes[0] === 2) this.authenticated = true;
      return new Uint8Array();
    }
    lastEvent() { return this.event; }
    pendingPeerId() { assert.ok(!this.freed); return new Uint8Array(16).fill(7); }
    exportPendingCredentialForStorage() { return new Uint8Array(32).fill(9); }
    acknowledgeCredential() { assert.ok(!this.freed); return Uint8Array.of(11); }
    sealMessage(bytes) { assert.ok(!this.freed); return [bytes]; }
    receiveFrame(bytes) { assert.ok(!this.freed); return bytes.slice(); }
    expireAssembly() { assert.ok(!this.freed); }
    close() {}
    free() { this.freed = true; }
  }
  const chrome = {
    action: {
      async setBadgeText(value) { badges.push(value.text); },
      async setPopup(value) { await toolbarHook?.('popup', value); popups.push(value.popup); },
    },
    sidePanel: { async setPanelBehavior(value) { await toolbarHook?.('panel', value); panelBehaviors.push(value.openPanelOnActionClick); } },
    windows: {
      async get(id) {
        const window = windows.find(window => window.id === id);
        if (!window) throw new Error('window unavailable');
        return { id, tabs: window.tabs.map(tab => ({ id: tab.id })) };
      },
      async create(options) {
        created.push(options);
        await createHook?.();
        const window = { id: ++nextWindow, tabs: [{ id: nextWindow * 10, url: options.url }] };
        windows.push(window);
        return { id: window.id, tabs: window.tabs.map(tab => ({ id: tab.id })) };
      },
      async remove(id) { closeWindow(id); },
      onRemoved: { addListener(callback) { removed = callback; } },
    },
    storage: { local: {
      async setAccessLevel(value) { assert.equal(value.accessLevel, 'TRUSTED_CONTEXTS'); },
      async get(keys) {
        const snapshot = structuredClone(data);
        await storageHook?.('get', keys);
        return snapshot;
      },
      async set(value) {
        const snapshot = structuredClone(value);
        await storageHook?.('set', snapshot);
        Object.assign(data, snapshot);
      },
      async remove(keys) {
        await storageHook?.('remove', keys);
        for (const key of [keys].flat()) delete data[key];
      },
    } },
    tabs: { async query({ windowId }) { return [activeTabs.get(windowId)].filter(Boolean); }, onActivated: event(), onRemoved: event() },
    webNavigation: { onCommitted: event(), onHistoryStateUpdated: event(), onReferenceFragmentUpdated: event(),
      async getFrame(query) { return frames.get(query.tabId); } },
    permissions: { async contains() { return true; } },
    alarms: { create() {}, clear() {}, onAlarm: { addListener(value) { alarm = value; } } },
    runtime: { id: 'test', getURL: value => `chrome-extension://test/${value}`, onConnect: { addListener(value) { listener = value; } },
      async getContexts(filter) {
        if (!contextsVisible) return [];
        return windows.flatMap(window => window.tabs
          .filter(tab => filter.documentUrls.includes(tab.url))
          .map(tab => ({ documentUrl: tab.url, windowId: window.id, tabId: tab.id })));
      },
      onStartup: event(), onInstalled: event() },
  };
  vm.runInNewContext(source.replace(/^import .*;\n/gm, ''), {
    init: async () => {}, GatewayClient: Client, configuration, chrome, createPageBridge, crypto: webcrypto,
    WebSocket: Socket, TextEncoder, TextDecoder, URL, Uint8Array, ArrayBuffer, performance,
    setTimeout: timer, clearTimeout: id => timers.delete(id), setInterval: timer, clearInterval: id => timers.delete(id),
  }, { filename: 'gateway-worker.js' });
  listener({ name: 'gateway-ui-v1', sender: { id: 'test', url: 'chrome-extension://test/index.html' },
    postMessage: value => states.push(value), onDisconnect: event(),
    onMessage: { addListener(value) { command = value; } }, disconnect() { assert.fail('trusted port disconnected'); } });
  await flush();
  return { chrome, data, sockets, clients, states, command, timers, windows, created, badges, closeWindow, popups, panelBehaviors, frames, activeTabs,
    toolbarHook(value) { toolbarHook = value; },
    createHook(value) { createHook = value; },
    contextsVisible(value) { contextsVisible = value; },
    ui(url = 'chrome-extension://test/index.html?mode=notification', name = 'gateway-ui-v1') {
      let request, disconnected;
      const port = { name, sender: { id: 'test', url }, messages: [],
        postMessage(value) { this.messages.push(value); }, onDisconnect: { addListener(value) { disconnected = value; } },
        onMessage: { addListener(value) { request = value; } }, disconnect() { this.closed = true; disconnected?.(); } };
      listener(port);
      return { port, request };
    },
    provider() {
      let request;
      const port = { name: 'gateway-provider-v1',
        sender: { id: 'test', tab: { id: 1 }, frameId: 0, documentId: 'browser-document', url: 'https://dapp.test/' },
        messages: [], postMessage(value) { this.messages.push(value); }, onDisconnect: event(),
        onMessage: { addListener(value) { request = value; } }, disconnect() { this.closed = true; } };
      listener(port);
      return { port, request };
    },
    async alarm() { alarm({ name: 'gateway-reconnect' }); await flush(); },
    hook(value) { storageHook = value; },
    async advance(milliseconds) {
      const until = clockTime + milliseconds;
      for (;;) {
        const entry = [...timers].filter(([, value]) => value.due <= until)
          .sort(([, left], [, right]) => left.due - right.due)[0];
        if (!entry) break;
        clockTime = entry[1].due;
        timers.delete(entry[0]); entry[1].callback(); await flush();
      }
      clockTime = until;
    },
    async timeout(delay) {
      const entry = [...timers].find(([, value]) => value.delay === delay);
      assert.ok(entry, `missing ${delay}ms timer`);
      timers.delete(entry[0]); entry[1].callback(); await flush();
    },
  };
}

for (const mode of ['pair', 'reconnect']) {
  test(`${mode} uses one default endpoint and keeps authentication failures visible`, async () => {
    for (const [message, status] of [[3, 'version_failed'], [4, 'auth_failed']]) {
      const h = await worker(mode === 'reconnect' ? { gatewayCredential: credential() } : {});
      if (mode === 'pair') { h.command({ type: 'pair', code: '123456' }); await flush(); }
      const socket = h.sockets[0];
      assert.equal(socket.url, endpoint(43110));
      socket.open(); socket.message(message); await flush();
      assert.equal(socket.readyState, 3);
      assert.equal(h.states.at(-1).status, status);
      assert.ok(h.clients[0].freed);
      assert.ok(h.clients[0].inputs.every(input => input.every(byte => byte === 0)));
      socket.onclose(); socket.message(2); await h.advance(30000);
      assert.equal(h.states.at(-1).status, status);
      assert.equal(h.sockets.length, 1);
    }
  });
}

test('desktop restart reconnects when the browser delays WebSocket opening', async () => {
  const saved = credential();
  const h = await worker({ gatewayCredential: saved });
  const first = h.sockets[0]; first.open(); first.message(2); await flush();
  assert.equal(h.states.at(-1).status, 'paired');

  first.close(); await flush();
  await h.advance(1000);
  const restarted = h.sockets[1];
  assert.equal(restarted.url, endpoint(43110));
  await h.advance(5000);
  assert.equal(restarted.readyState, 0, 'browser backoff must not cancel the pending connection');
  assert.equal(h.sockets.length, 2);

  restarted.open(); restarted.message(2); await flush();
  assert.equal(h.states.at(-1).status, 'paired');
  assert.equal(h.clients.length, 2);
  assert.ok(h.clients[0].freed);
  assert.equal(h.clients[1].mode, 'reconnect');
  assert.deepEqual(h.clients[1].copied, [saved.peerId, saved.secret]);
  assert.deepEqual(h.data.gatewayCredential, saved);
});

test('opening failure retries the same endpoint without reusing pairing codes', async () => {
  for (const recovery of ['timer', 'alarm', 'pair']) {
    const pairing = recovery === 'pair';
    const h = await worker(pairing ? {} : { gatewayCredential: credential() });
    assert.equal(h.states.filter(message => message.type === 'state').at(-1).paired, !pairing,
      'a stored credential decides the reported pairing before any socket opens');
    if (pairing) { h.command({ type: 'pair', code: '123456' }); await flush(); }
    const first = h.sockets[0];
    await h.timeout(10000);
    assert.equal(first.readyState, 3);
    assert.equal(h.states.at(-1).status, 'disconnected');
    assert.equal(h.sockets.length, 1, 'opening failure must not probe another port');
    first.onclose(); await flush();
    if (pairing) {
      assert.equal(h.timers.size, 0, 'failed pairing schedules no retry');
      await h.alarm();
      assert.equal(h.sockets.length, 1, 'an alarm cannot reuse the pairing code');
      continue;
    }
    if (recovery === 'timer') await h.timeout(1000);
    else await h.alarm();
    const desktop = h.sockets[1]; desktop.open(); desktop.message(2); await flush();
    assert.deepEqual(h.sockets.map(socket => socket.url), [endpoint(43110), endpoint(43110)]);
    assert.equal(h.states.at(-1).status, 'paired');
    assert.equal(h.clients.at(-1).mode, 'reconnect');
    first.onclose(); first.message(3); await h.alarm();
    assert.equal(h.states.at(-1).status, 'paired');
    assert.equal(h.sockets.length, 2, 'stale callbacks and alarms leave the recovered session alone');
  }
});

test('authentication timeout and malformed frames stop the configured attempt', async () => {
  for (const failure of ['timeout', 'malformed', 'closed']) {
    const h = await worker({ gatewayCredential: credential() });
    const socket = h.sockets[0]; socket.open();
    if (failure === 'timeout') await h.timeout(10000);
    else if (failure === 'malformed') { socket.message('not binary'); await flush(); }
    else { socket.close(); await flush(); }
    assert.equal(h.states.at(-1).status, 'auth_failed');
    assert.equal(h.sockets.length, 1);
    assert.ok(h.clients[0].freed);
  }
});

test('explicit URLs and bare hosts connect only to their selected endpoint', async () => {
  for (const [value, expected] of [[endpoint(44000), endpoint(44000)], ['desktop.local', 'ws://desktop.local:43110/']]) {
    const h = await worker({ gatewayCredential: credential(), gatewayPreferences: { endpoint: value } });
    h.sockets[0].open(); h.sockets[0].message(4); await flush();
    assert.deepEqual(h.sockets.map(socket => socket.url), [expected]);
    assert.equal(h.states.at(-1).status, 'auth_failed');
  }
});

test('configure and retry retire pending connections, and pairing replaces the configured attempt', async () => {
  const h = await worker({ gatewayCredential: credential() });
  const first = h.sockets[0]; first.open();
  h.command({ type: 'configure', endpoint: endpoint(44000) }); await flush();
  assert.equal(first.readyState, 3);
  const configured = h.sockets[1]; assert.equal(configured.url, endpoint(44000));
  configured.open();
  h.command({ type: 'retry' }); await flush();
  assert.equal(configured.readyState, 3);
  const retried = h.sockets[2]; retried.open();
  h.command({ type: 'pair', code: '654321' }); await flush();
  assert.equal(retried.readyState, 3);
  const paired = h.sockets[3]; paired.open();
  first.onclose(); configured.message(3); retried.onclose(); await flush();
  paired.message(1); await flush(); paired.message(2); await flush();
  assert.equal(h.states.at(-1).status, 'paired');
  assert.equal(h.clients.at(-1).mode, 'pair');
});

test('unpair forgets the stored credential and leaves nothing to reconnect', async () => {
  const h = await worker({ gatewayCredential: credential() });
  const socket = h.sockets[0]; socket.open(); socket.message(2); await flush();
  const state = () => h.states.filter(message => message.type === 'state').at(-1);
  assert.equal(state().paired, true);
  h.command({ type: 'unpair' }); await flush();
  assert.equal(socket.readyState, 3);
  assert.equal(h.data.gatewayCredential, undefined);
  assert.equal(state().status, 'disconnected');
  assert.equal(state().paired, false);
  await h.alarm();
  assert.equal(h.sockets.length, 1, 'unpair leaves no credential for a reconnect');
  assert.equal(state().paired, false);
});

test('pairing with an endpoint saves it before opening that socket', async () => {
  const h = await worker();
  let socketsWhenSaved;
  h.hook((operation, value) => {
    if (operation === 'set' && value.gatewayPreferences) socketsWhenSaved = h.sockets.length;
  });
  h.command({ type: 'pair', code: '123456', endpoint: endpoint(44000) }); await flush();
  assert.equal(socketsWhenSaved, 0, 'the endpoint is saved before the attempt');
  assert.deepEqual(h.data.gatewayPreferences, { endpoint: endpoint(44000) });
  assert.deepEqual(h.sockets.map(socket => socket.url), [endpoint(44000)]);
  const socket = h.sockets[0]; socket.open(); socket.message(1); await flush();
  socket.message(2); await flush();
  assert.equal(h.clients.at(-1).mode, 'pair');
  const state = h.states.filter(message => message.type === 'state').at(-1);
  assert.equal(state.status, 'paired');
  assert.equal(state.paired, true);
});

test('retry during pending credential storage cannot acknowledge or revive the retired candidate', async () => {
  const h = await worker();
  h.command({ type: 'pair', code: '123456' }); await flush();
  const first = h.sockets[0]; first.open();
  const blocked = deferred();
  h.hook((operation, value) => operation === 'set' && value.gatewayCredential ? blocked.promise : undefined);
  first.message(1); await flush();
  assert.equal(first.sent.length, 1, 'acknowledgement waits for persistence');
  h.command({ type: 'retry' }); await flush();
  assert.equal(first.readyState, 3);
  assert.ok(h.clients[0].freed);
  h.hook(null); blocked.resolve(); await flush();
  assert.equal(first.sent.length, 1);
  assert.equal(h.sockets.length, 2);
  const second = h.sockets[1]; second.open(); second.message(2); await flush();
  assert.equal(h.clients[1].mode, 'reconnect', 'retry loads the serialized pending credential');
  assert.equal(h.states.at(-1).status, 'paired');
});

test('authentication timeout fences pending credential storage before retry', async () => {
  const h = await worker();
  h.command({ type: 'pair', code: '123456' }); await flush();
  const stale = h.sockets[0]; stale.open();
  const blocked = deferred();
  h.hook((operation, value) => operation === 'set' && value.gatewayCredential ? blocked.promise : undefined);
  stale.message(1); await flush();
  await h.timeout(10000);
  assert.equal(h.states.at(-1).status, 'auth_failed');
  assert.equal(stale.readyState, 3);
  h.command({ type: 'retry' }); await flush();
  h.hook(null); blocked.resolve(); await flush();
  assert.equal(stale.sent.length, 1, 'expired pairing cannot acknowledge stored credentials');
  const next = h.sockets[1]; next.open(); next.message(2); await flush();
  assert.equal(h.states.at(-1).status, 'paired');
  assert.deepEqual(h.data.gatewayCredential, credential());
});

test('credential persistence failures terminate pairing', async () => {
  const h = await worker();
  h.command({ type: 'pair', code: '123456' }); await flush();
  h.hook(operation => { if (operation === 'set') throw new Error('storage unavailable'); });
  h.sockets[0].open(); h.sockets[0].message(1); await flush();
  assert.equal(h.states.at(-1).status, 'storage_failed');
  assert.equal(h.sockets.length, 1);
  assert.equal(h.sockets[0].sent.length, 1);
});

test('fragmented 128 KiB calldata fits the bounded socket buffer and capacity failure retires the session', async () => {
  const h = await worker({ gatewayCredential: credential() });
  const socket = h.sockets[0]; socket.open(); socket.message(2); await flush();
  // Model protocol fragments and a socket that cannot drain within the send task.
  h.clients[0].sealMessage = bytes => {
    const frames = [];
    for (let offset = 0; offset < bytes.length; offset += 65000) frames.push(bytes.slice(offset, offset + 65000));
    return frames;
  };
  const provider = h.provider(); await flush();
  const before = socket.sent.length;
  const data = `0x${'ab'.repeat(128 * 1024)}`;
  provider.request({ id: 'large', method: 'eth_call', params: [{ data }] }); await flush();
  const frames = socket.sent.slice(before);
  assert.ok(frames.length > 1);
  assert.ok(frames.every(frame => frame.length <= 65535));
  assert.equal(JSON.parse(new TextDecoder().decode(new Uint8Array(frames.flat()))).params[0].data, data);
  assert.ok(socket.bufferedAmount > 262144);
  assert.equal(socket.readyState, 1);
  assert.equal(provider.port.closed, undefined);
  socket.bufferedAmount = 34 * 1024 * 1024 - 65000;
  const sentBeforeFailure = socket.sent.length;
  provider.request({ id: 'full', method: 'eth_call', params: [{ data }] }); await flush();
  assert.equal(socket.readyState, 3);
  assert.equal(h.clients[0].freed, true);
  assert.equal(socket.sent.length, sentBeforeFailure + 1, 'only the first fragment fits; teardown sends no further records');
  assert.ok(provider.port.messages.some(message => message.id === 'large' && message.error?.code === 4900));
});

async function established(h, locked = false) {
  const socket = h.sockets[0];
  socket.open(); socket.message(2); await flush();
  const send = async message => {
    socket.message(new TextEncoder().encode(JSON.stringify(message)).buffer);
    await flush();
  };
  await send({ type: 'state', version: 1, generation: 1, locked });
  return { socket, send, snapshot: (requests, connects = [], generation = 1, locked = false) => send({
    type: 'ui_snapshot', version: 1, generation, locked, accounts: [], pending_requests: requests, pending_connects: connects,
  }) };
}
const pending = id => ({ request_id: id, url: 'https://dapp.test/', needs_unlock: false, summary: 'Desktop review summary' });
const connectPrompt = (wrong = true) => ({ request_id: 'connect', wrong_wallet: wrong, accounts: [], chains: [{ id: 1, name: 'Ethereum' }] });

test('notifications share connect and request lifecycles across dismissal, redaction and expiry', async () => {
  const h = await worker({ gatewayCredential: credential() });
  const session = await established(h);
  await session.snapshot([], [connectPrompt()]);
  assert.equal(h.created.length, 1, 'connect prompts open notifications');
  assert.equal(h.badges.at(-1), '1');
  await session.snapshot([pending('one')], [connectPrompt()]);
  await session.snapshot([pending('one')], [connectPrompt()]);
  assert.equal(h.created.length, 1);
  assert.equal(h.badges.at(-1), '2');
  const notification = h.ui();
  assert.equal(notification.port.closed, undefined);
  assert.equal(notification.port.messages[0].pending_requests[0].summary, 'Desktop review summary');
  h.closeWindow(h.windows[0].id);
  await session.snapshot([pending('one')]);
  assert.equal(h.created.length, 1, 'same request must not reopen a dismissed window');
  await session.snapshot([pending('one'), pending('two')]);
  assert.equal(h.created.length, 2, 'a new request can reopen the notification');
  await session.snapshot([]);
  assert.equal(h.windows.length, 0, 'native expiry closes the owned notification');
  await session.snapshot([pending('three')]);
  await session.send({ type: 'state', version: 1, generation: 2, locked: true });
  await session.snapshot([pending('three')], [], 2, true);
  assert.equal(h.windows.length, 1, 'state redaction preserves the owned notification');
  assert.equal(h.badges.at(-1), '1', 'locked pending requests remain counted');
  const snapshot = notification.port.messages.filter(message => message.type === 'ui_snapshot').at(-1);
  assert.equal(snapshot.pending_requests[0].summary, null);
  assert.equal(snapshot.pending_requests[0].needs_unlock, true);
  const sentBeforeDuplicate = session.socket.sent.length;
  await session.send({ type: 'state', version: 1, generation: 2, locked: true });
  const duplicate = notification.port.messages.filter(message => message.type === 'ui_snapshot').at(-1);
  assert.deepEqual(duplicate, snapshot, 'duplicate locked state retains the authoritative redacted snapshot');
  assert.equal(duplicate.generation, 2);
  assert.equal(h.badges.at(-1), '1', 'duplicate locked state retains the pending badge');
  assert.equal(h.windows.length, 1, 'duplicate locked state retains the notification');
  assert.equal(session.socket.sent.length, sentBeforeDuplicate, 'duplicate state emits no user activity');
  h.closeWindow(h.windows[0].id);
  const creates = h.created.length;
  await session.send({ type: 'state', version: 1, generation: 3, locked: true });
  const redacted = notification.port.messages.filter(message => message.type === 'ui_snapshot').at(-1);
  assert.equal(redacted.pending_requests.length, 0, 'state changes immediately purge presentation data');
  await session.snapshot([pending('three')], [], 3, true);
  assert.equal(h.created.length, creates, 'the same request stays dismissed across state redaction');
  await session.snapshot([pending('three')], [{ ...connectPrompt(), request_id: 'new-connect' }], 3, true);
  assert.equal(h.created.length, creates + 1, 'a new locked connect prompt opens a redacted notification');
  h.closeWindow(h.windows[0].id);
  await session.send({ type: 'state', version: 1, generation: 4, locked: true });
  await session.snapshot([pending('three')], [{ ...connectPrompt(), request_id: 'new-connect' }], 4, true);
  assert.equal(h.created.length, creates + 1, 'a dismissed connect stays dismissed across state redaction');
  await session.snapshot([pending('four')], [], 4, true);
  assert.equal(h.created.length, creates + 2, 'a new locked request opens a redacted notification');
  await session.snapshot([], [], 4, true);
  assert.equal(h.windows.length, 0, 'an authoritative empty snapshot closes the locked notification');
  h.createHook(() => { throw new Error('window unavailable'); });
  await session.send({ type: 'state', version: 1, generation: 5, locked: false });
  await session.snapshot([pending('five')], [], 5);
  assert.equal(h.windows.length, 0);
  assert.equal(h.badges.at(-1), '1', 'failed creation retains the pending badge');
  h.command({ type: 'summon_desktop', generation: 5 });
  const last = JSON.parse(new TextDecoder().decode(new Uint8Array(session.socket.sent.at(-1))));
  assert.equal(last.type, 'summon_desktop', 'manual desktop summon remains usable after opening fails');
});

test('notification creation cannot outlive retirement and restart cleanup touches only the exact owned URL', async () => {
  const other = { id: 1, tabs: [{ id: 10, url: 'chrome-extension://test/index.html?mode=window' }] };
  const stale = { id: 2, tabs: [{ id: 20, url: 'chrome-extension://test/index.html?mode=notification' }] };
  const rehomed = { id: 3, tabs: [{ id: 30, url: stale.tabs[0].url }, { id: 31, url: 'https://unrelated.test/' }] };
  const delayed = await worker({}, [other, stale], false);
  assert.deepEqual(delayed.windows.map(window => window.id), [1, 2]);
  delayed.contextsVisible(true);
  delayed.ui('chrome-extension://test/index.html?mode=window');
  await flush();
  assert.deepEqual(delayed.windows.map(window => window.id), [1, 2], 'ordinary UI connections do not rediscover notification windows');
  delayed.ui();
  await flush();
  assert.deepEqual(delayed.windows.map(window => window.id), [1], 'a loaded notification is rediscovered after its context was hidden at worker startup');
  const h = await worker({ gatewayCredential: credential() }, [other, stale, rehomed]);
  assert.deepEqual(h.windows.map(window => window.id), [1, 3]);
  const session = await established(h);
  const blocked = deferred();
  h.createHook(() => blocked.promise);
  await session.snapshot([pending('one')]);
  await session.snapshot([pending('one')]);
  assert.equal(h.created.length, 1, 'concurrent snapshots share a single creation');
  h.command({ type: 'retry' }); await flush();
  h.contextsVisible(false);
  blocked.resolve(); await flush();
  assert.equal(h.windows.length, 3, 'ownership survives create resolving before the document context exists');
  h.ui('chrome-extension://test/index.html?mode=window');
  await flush();
  assert.equal(h.windows.length, 3, 'other UI ports cannot finish notification startup cleanup');
  h.contextsVisible(true);
  h.ui();
  await flush();
  assert.deepEqual(h.windows.map(window => window.id), [1, 3], 'the loaded notification port finishes retired cleanup without closing other windows');
  assert.equal(h.badges.at(-1), '');

  const active = await worker({ gatewayCredential: credential() });
  const activeSession = await established(active);
  await activeSession.snapshot([pending('move')]);
  const owned = active.windows[0];
  const notificationTab = owned.tabs[0];
  owned.tabs = [{ id: 999, url: 'https://unrelated.test/' }];
  active.windows.push({ id: 500, tabs: [notificationTab, { id: 998, url: 'https://other.test/' }] });
  await activeSession.snapshot([]);
  assert.deepEqual(active.windows.map(window => window.id), [owned.id, 500], 'expiry cannot close unrelated tabs after a user moves the notification');
  await activeSession.snapshot([pending('navigate')]);
  const navigated = active.windows.at(-1);
  navigated.tabs[0].url = 'https://unrelated.test/';
  await activeSession.snapshot([]);
  assert.deepEqual(active.windows.map(window => window.id), [owned.id, 500, navigated.id], 'expiry preserves the same tab after it navigates away from the notification');
  await activeSession.snapshot([pending('after-navigation')]);
  const replacement = active.windows.at(-1);
  assert.notEqual(replacement.id, navigated.id, 'a new request opens a notification after the previous tab navigated away');
  assert.equal(navigated.tabs[0].url, 'https://unrelated.test/');
  assert.equal(active.windows.includes(navigated), true);
  // A previously loading notification may finish after its replacement is already live.
  active.windows.push({ id: 600, tabs: [{ id: 6000, url: stale.tabs[0].url }] });
  active.ui();
  await flush();
  assert.deepEqual(active.windows.map(window => window.id), [owned.id, 500, navigated.id, replacement.id], 'a delayed notification port preserves the current owned window and closes only exact-context duplicates');
});

test('desktop switch, summon and activity require the current privileged UI generation', async () => {
  const h = await worker({ gatewayCredential: credential() });
  const session = await established(h);
  await session.snapshot([], [connectPrompt()]);
  const sent = () => session.socket.sent.slice(1).map(bytes => JSON.parse(new TextDecoder().decode(new Uint8Array(bytes))));
  const ui = h.ui();
  const before = sent().length;
  for (const type of ['request_wallet_switch', 'summon_desktop', 'user_activity']) {
    ui.request({ type, generation: 0, request_id: 'connect' });
  }
  ui.request({ type: 'request_wallet_switch', generation: 1, request_id: 'missing' });
  assert.equal(sent().length, before);
  for (const type of ['request_wallet_switch', 'summon_desktop', 'user_activity']) {
    ui.request({ type, generation: 1, request_id: 'connect', wallet: 'untrusted-target' });
  }
  assert.deepEqual(sent().slice(before), [
    { type: 'request_wallet_switch', version: 1, generation: 1, request_id: 'connect' },
    { type: 'summon_desktop', version: 1, generation: 1 },
    { type: 'user_activity', version: 1, generation: 1 },
  ]);
  await session.snapshot([], [connectPrompt(false)]);
  ui.request({ type: 'request_wallet_switch', generation: 1, request_id: 'connect' });
  assert.equal(sent().length, before + 3);
  for (const url of ['https://dapp.test/', 'chrome-extension://test/index.html?mode=notification&extra=1']) {
    assert.equal(h.ui(url).port.closed, true);
  }
  const provider = h.provider(); await flush();
  const providerBefore = sent().length;
  for (const type of ['request_wallet_switch', 'summon_desktop', 'user_activity']) {
    provider.request({ type, generation: 1, request_id: 'connect' });
  }
  await flush();
  assert.equal(sent().slice(providerBefore).some(message => ['request_wallet_switch', 'summon_desktop', 'user_activity'].includes(message.type)), false);
  h.command({ type: 'retry' }); await flush();
  ui.request({ type: 'user_activity', generation: 1 });
  assert.equal(session.socket.readyState, 3);
});


test('public presentation is purged on lock and cannot return through late snapshots or a new view', async () => {
  const h = await worker({ gatewayCredential: credential() });
  const session = await established(h);
  const accounts = [{ uuid: 'first', label: 'Spending', address: '0xabc' }];
  const rich = { accounts, public_view: { selected_account: 'first', selected_chain: 1,
    balances: [{ account_uuid: 'first', total: '$12.00', assets: [] }], drafts: [{ draft_id: 'draft', status: 'attention', input: { recipient: 'recipient.eth' } }] },
    permissions: [{ permission_id: 'grant', origin: 'https://dapp.test/', account_uuid: 'first', chain_id: 1 }] };
  const delivered = () => JSON.parse(JSON.stringify(h.states.filter(message => message.type === 'ui_snapshot').at(-1)));
  const snapshot = (generation, locked) => session.send({ type: 'ui_snapshot', version: 1, generation, locked,
    pending_connects: [], pending_requests: [], ...rich });
  await snapshot(1, false);
  assert.deepEqual(delivered().public_view, rich.public_view);
  await session.send({ type: 'state', version: 1, generation: 2, locked: true });
  await snapshot(1, false);
  await snapshot(2, true);
  assert.deepEqual(delivered().accounts, []);
  assert.ok(!delivered().public_view?.balances?.length);
  assert.ok(!h.ui().port.messages[0].permissions?.length);
  assert.ok(!delivered().public_view?.drafts?.length);
  await session.send({ type: 'ui_snapshot', version: 1, generation: 2, locked: true, accounts: [], pending_connects: [], pending_requests: [], public_view: { drafts: rich.public_view.drafts } });
  assert.ok(!delivered().public_view?.drafts?.length);
  await session.send({ type: 'state', version: 1, generation: 3, locked: false });
  await snapshot(3, false);
  assert.deepEqual(delivered().permissions, rich.permissions);
  session.socket.close(); await flush();
  assert.ok(!h.ui().port.messages[0].public_view?.balances?.length);
});

test('current-tab connect is view-scoped and browser-attested across navigation', async () => {
  const h = await worker({ gatewayCredential: credential() });
  const session = await established(h);
  const provider = h.provider(); await flush();
  await session.send({ type: 'ui_snapshot', version: 1, generation: 1, locked: false,
    accounts: [{ uuid: 'first' }], chains: [{ id: 1 }], permissions: [], pending_connects: [], pending_requests: [] });
  const ui = h.ui('chrome-extension://test/index.html');
  ui.request({ type: 'popup_presence', ready: true, visible: true });
  ui.request({ type: 'tab_context', window_id: 17 }); await flush();
  const tab = ui.port.messages.at(-1);
  assert.equal(tab.current_tab_origin, 'https://dapp.test/');
  const sent = () => session.socket.sent.map(bytes => { try { return JSON.parse(new TextDecoder().decode(new Uint8Array(bytes))); } catch { return {}; } }).filter(message => message.type === 'public_view');
  const connect = token => ui.request({ type: 'public_view', generation: 1, tab_token: token, command: { type: 'connect_tab' } });
  connect(tab.current_tab_token); await flush();
  assert.equal(sent().at(-1).command.type, 'connect_tab');
  await session.snapshot([], [connectPrompt(false)]);
  assert.equal(h.created.length, 0, 'a wallet-originated connect stays in the visible popup');
  await session.send({ type: 'ui_snapshot', version: 1, generation: 1, locked: false,
    accounts: [{ uuid: 'first' }], chains: [{ id: 1 }], permissions: [], pending_connects: [], pending_requests: [] });
  const count = sent().length;
  // A different window cannot reuse this view's attestation token.
  h.activeTabs.set(18, { id: 2 });
  const second = h.ui(); second.request({ type: 'tab_context', window_id: 18 }); await flush();
  second.request({ type: 'public_view', generation: 1, tab_token: tab.current_tab_token, command: { type: 'connect_tab' } });
  // The browser can navigate before its event reaches the worker.
  h.frames.set(1, { documentId: 'replacement', documentLifecycle: 'active', url: 'https://new.test/' });
  connect(tab.current_tab_token); await flush();
  assert.equal(sent().length, count);
  h.chrome.webNavigation.onCommitted.fire({ tabId: 1, frameId: 0, ...h.frames.get(1) }); await flush();
  const replacement = ui.port.messages.at(-1);
  assert.equal(replacement.current_tab_origin, 'https://new.test/');
  assert.notEqual(replacement.current_tab_token, tab.current_tab_token);
  connect(tab.current_tab_token); await flush();
  assert.equal(sent().length, count);
  ui.request({ type: 'public_view', generation: 1, command: { type: 'select_account', public_account_uuid: 'first' } });
  assert.equal(sent().at(-1).command.public_account_uuid, 'first');
  const selected = sent().length;
  for (const generation of [0, 2]) ui.request({ type: 'public_view', generation, command: { type: 'refresh_balances' } });
  provider.request({ id: 'privileged', method: 'public_view', params: { type: 'refresh_balances' } });
  await flush();
  assert.equal(sent().length, selected);
});

async function bootstrapView({ popup = false, tab = false, mode = 'notification', ready = true,
  platform = 'Linux x86_64', userAgent = '' } = {}) {
  const bootstrap = await readFile(new URL('../../extensions/railoxide/bootstrap.js', import.meta.url), 'utf8');
  const domEvents = new Map(), windowEvents = new Map(), sent = [], viewCalls = [];
  let time = 0;
  const ports = [], timers = new Map();
  const node = () => ({ hidden: false, textContent: '', addEventListener() {} });
  function connect() {
    const port = {
      onMessage: { addListener(callback) { port.incoming = callback; } },
      onDisconnect: { addListener(callback) { port.disconnected = callback; } },
      postMessage(message) { sent.push(message); }, disconnect() {},
    };
    ports.push(port);
    return port;
  }
  const context = {
    configuration, URL, Uint8Array, AbortController, queueMicrotask, TextDecoder,
    navigator: { platform, userAgent },
    console: { info() {} },
    performance: { now: () => time, mark() {} },
    location: { href: `chrome-extension://test/index.html${mode ? `?mode=${mode}` : ''}`, reload() {} },
    document: { visibilityState: 'visible', querySelector: node, documentElement: { classList: { add() {} } },
      addEventListener(type, callback) { domEvents.set(type, callback); } },
    window: { addEventListener(type, callback) { windowEvents.set(type, callback); }, close() { viewCalls.push('close-document'); } },
    chrome: { extension: { getViews: ({ type }) => (type === 'popup' ? popup : tab) ? [vm.runInContext('globalThis', context)] : [] },
      runtime: { connect, getURL: path => `chrome-extension://test/${path}` },
      windows: {
        async getCurrent() { return { id: 17, type: 'normal', tabs: [{ id: 170 }] }; },
        async getLastFocused() { return { id: 17, type: 'normal' }; },
      },
      sidePanel: {
        async open(options) { assert.equal(options.windowId, 17); viewCalls.push('open-panel'); },
        async close(options) { assert.equal(options.windowId, 17); viewCalls.push('close-panel'); },
      },
      action: { async openPopup(options) { assert.equal(options.windowId, 17); viewCalls.push('open-popup'); } },
    },
    setTimeout(callback, delay) { timers.set(delay, callback); return delay; },
    clearTimeout(delay) { timers.delete(delay); },
    fetch: async url => ({ ok: true, arrayBuffer: async () => url.endsWith('WALLET-ASSETS.json') ? new TextEncoder().encode('[]').buffer : Uint8Array.of(1).buffer }),
    browserRuntime: { async default() {}, run() { if (ready) context.railoxideHost.ready(); }, stop() {} },
  };
  vm.createContext(context);
  vm.runInContext(bootstrap.replace(/^import .*;\n/gm, '')
    .replace("runtime = await import('./browser_frontend.js');", 'runtime = browserRuntime;'), context);
  await flush();
  return { context, domEvents, windowEvents, sent, ports, timers, viewCalls,
    incoming: message => ports.at(-1).incoming(message),
    disconnected: () => ports.at(-1).disconnected(),
    setTime(value) { time = value; } };
}

test('the GPUI host selects Mac editing from the browser platform with a user-agent fallback', async () => {
  for (const [platform, userAgent, expected] of [
    ['MacIntel', '', true], ['Win32', '', false], ['Linux x86_64', '', false],
    ['', 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)', true],
  ]) {
    const view = await bootstrapView({ platform, userAgent });
    assert.equal(view.context.railoxideHost.isMac(), expected);
  }
});

test('only trusted extension input emits activity, never startup, focus, synthetic input or host commands', async () => {
  const view = await bootstrapView();
  const { context, domEvents, windowEvents, sent, incoming, disconnected } = view;
  incoming({ type: 'ui_snapshot', generation: 7, locked: false, accounts: [], pending_connects: [], pending_requests: [pending('one')] });
  assert.equal(sent.filter(message => message.type === 'user_activity').length, 0, 'automatic startup and snapshot delivery are not activity');
  domEvents.get('pointerdown')({ isTrusted: false });
  domEvents.get('keydown')({ isTrusted: false });
  domEvents.get('wheel')({ isTrusted: false });
  windowEvents.get('focus')?.({ isTrusted: true });
  context.railoxideHost.command('summon_desktop', '');
  assert.equal(sent.some(message => message.type === 'user_activity'), false);
  domEvents.get('pointerdown')({ isTrusted: true });
  domEvents.get('wheel')({ isTrusted: true });
  assert.equal(sent.filter(message => message.type === 'user_activity').length, 1, 'local throttle bounds repeated input');
  view.setTime(1000);
  domEvents.get('keydown')({ isTrusted: true });
  assert.deepEqual(sent.filter(message => message.type === 'user_activity').map(message => message.generation), [7, 7]);
  incoming({ type: 'ui_snapshot', generation: 8, locked: true, accounts: [], pending_connects: [], pending_requests: [] });
  view.setTime(2000);
  domEvents.get('pointerdown')({ isTrusted: true });
  disconnected();
  domEvents.get('keydown')({ isTrusted: true });
  windowEvents.get('pagehide')();
  domEvents.get('wheel')({ isTrusted: true });
  assert.equal(sent.filter(message => message.type === 'user_activity').length, 2, 'locked, disconnected and closed documents cannot extend inactivity');
});


test('toolbar popup opening emits activity once and cannot survive lock, generation or port retirement', async () => {
  const snapshot = (generation = 7, locked = false) => ({
    type: 'ui_snapshot', generation, locked, accounts: [], pending_connects: [], pending_requests: [],
  });
  for (const ready of [false, true]) {
    const view = await bootstrapView({ popup: true, mode: '', ready });
    view.incoming(snapshot());
    assert.equal(view.sent.filter(message => message.type === 'user_activity').length, ready ? 1 : 0);
    view.context.railoxideHost.ready();
    view.incoming(snapshot());
    view.domEvents.get('pointerdown')({ isTrusted: true });
    view.setTime(1000);
    view.incoming(snapshot());
    view.context.railoxideHost.ready();
    assert.deepEqual(view.sent.filter(message => message.type === 'user_activity').map(message => [message.type, message.generation]), [['user_activity', 7]],
      'opening and immediate trusted input share the local throttle');
  }
  for (const mode of ['', 'window', 'notification', 'sidepanel']) {
    const view = await bootstrapView({ mode });
    view.incoming(snapshot());
    assert.equal(view.sent.filter(message => message.type === 'user_activity').length, 0, 'ordinary windows and panels do not qualify even at the action popup URL');
  }
  for (const retirement of ['initial-lock', 'invalid-generation', 'lock', 'generation', 'disconnect', 'initial-disconnect', 'status-disconnect', 'pagehide']) {
    const view = await bootstrapView({ popup: true, mode: '', ready: false });
    if (retirement !== 'initial-disconnect') {
      view.incoming({ ...snapshot(7, retirement === 'initial-lock'),
        generation: retirement === 'invalid-generation' ? undefined : 7 });
    }
    if (retirement === 'lock') view.incoming(snapshot(7, true));
    if (retirement === 'generation') view.incoming(snapshot(8));
    if (retirement === 'status-disconnect') view.incoming({ type: 'state', status: 'disconnected' });
    if (retirement === 'disconnect' || retirement === 'initial-disconnect') {
      const retired = view.ports[0];
      view.disconnected();
      view.timers.get(1000)();
      retired.incoming(snapshot());
    }
    if (retirement === 'pagehide') view.windowEvents.get('pagehide')();
    view.incoming(snapshot());
    view.context.railoxideHost.ready();
    view.incoming(snapshot());
    assert.equal(view.sent.filter(message => message.type === 'user_activity').length, 0, `${retirement} must not replay popup opening`);
  }
});


test('toolbar view persists independently of provider preferences and failed changes stay visible', async () => {
  const h = await worker({ gatewayPreferences: { takeover: true, endpoint: 'saved' } });
  assert.equal(h.popups.at(-1), 'index.html');
  assert.equal(h.panelBehaviors.at(-1), false);
  h.command({ type: 'preferences', key: 'view', value: 'sidepanel', request_id: 1 });
  await flush();
  assert.equal(h.popups.at(-1), '');
  assert.equal(h.panelBehaviors.at(-1), true);
  assert.deepEqual(h.data.gatewayPreferences, { takeover: true, endpoint: 'saved', view: 'sidepanel' });
  assert.equal(h.states.filter(message => message.type === 'state').at(-1).view, 'sidepanel');
  assert.deepEqual(JSON.parse(JSON.stringify(h.states.at(-1))), { type: 'view_result', request_id: 1, success: true });
  const restarted = await worker(h.data);
  assert.equal(restarted.popups.at(-1), '');
  assert.equal(restarted.panelBehaviors.at(-1), true);
  for (const value of ['window', true, null, '?mode=sidepanel']) {
    restarted.command({ type: 'preferences', key: 'view', value });
    await flush();
  }
  assert.equal(restarted.popups.length, 1, 'invalid values cannot change toolbar behavior');
  restarted.toolbarHook((kind, value) => {
    if (kind === 'panel' && value.openPanelOnActionClick === false) throw new Error('browser rejected mode');
  });
  restarted.command({ type: 'preferences', key: 'view', value: 'popup', request_id: 2 });
  await flush();
  assert.equal(restarted.states.filter(message => message.type === 'state').at(-1).view, 'sidepanel');
  assert.equal(restarted.states.filter(message => message.type === 'state').at(-1).viewError, true);
  assert.equal(restarted.data.gatewayPreferences.view, 'sidepanel');
  assert.deepEqual(JSON.parse(JSON.stringify(restarted.states.at(-1))), { type: 'view_result', request_id: 2, success: false });
  assert.equal(restarted.popups.at(-1), '', 'failed partial transition restores the previous popup');
  restarted.toolbarHook(undefined);
  restarted.command({ type: 'preferences', key: 'view', value: 'popup' });
  await flush();
  assert.equal(restarted.states.at(-1).viewError, false);
  assert.equal(restarted.data.gatewayPreferences.view, 'popup');
  const popupRestart = await worker(restarted.data);
  assert.equal(popupRestart.popups.at(-1), 'index.html');
  assert.equal(popupRestart.panelBehaviors.at(-1), false);
});

test('only ready visible side panels take over notifications, with no dismissal replay', async () => {
  const h = await worker({ gatewayCredential: credential(), gatewayPreferences: { view: 'sidepanel' } });
  const session = await established(h);
  const panel = h.ui('chrome-extension://test/index.html?mode=sidepanel');
  panel.request({ type: 'sidepanel_presence', ready: false, visible: true });
  await session.snapshot([pending('loading')]);
  assert.equal(h.created.length, 1, 'a saved mode and loading panel do not suppress requests');
  panel.request({ type: 'sidepanel_presence', ready: true, visible: true });
  await flush();
  assert.equal(h.windows.length, 0, 'the ready visible panel closes the owned notification');
  await session.snapshot([pending('visible')]);
  assert.equal(h.created.length, 1);
  assert.equal(h.badges.at(-1), '1');
  panel.request({ type: 'sidepanel_presence', ready: true, visible: false });
  await session.snapshot([pending('visible')]);
  assert.equal(h.created.length, 1, 'hiding does not replay a request already shown');
  h.command({ type: 'sidepanel_presence', ready: true, visible: true });
  await session.snapshot([pending('hidden')]);
  assert.equal(h.created.length, 2, 'ordinary UI cannot spoof visible panel presence');
  panel.request({ type: 'sidepanel_presence', ready: true, visible: true });
  await flush();
  panel.port.disconnect();
  await session.snapshot([pending('disconnected')]);
  assert.equal(h.created.length, 3, 'disconnect removes visible presence');
  const impostor = h.ui('chrome-extension://test/index.html?mode=sidepanel&extra=1');
  assert.equal(impostor.port.closed, true);
});

test('side panel readiness, visibility and reconnect report presence without extending inactivity', async () => {
  const view = await bootstrapView({ mode: 'sidepanel', ready: false });
  const snapshot = { type: 'ui_snapshot', generation: 7, locked: false, accounts: [], pending_connects: [], pending_requests: [] };
  view.incoming(snapshot);
  assert.equal(view.sent.filter(message => message.type === 'sidepanel_presence').length, 0);
  view.context.railoxideHost.ready();
  assert.equal(view.sent.at(-1).visible, true);
  view.context.document.visibilityState = 'hidden';
  view.domEvents.get('visibilitychange')();
  assert.equal(view.sent.at(-1).visible, false);
  view.context.document.visibilityState = 'visible';
  view.domEvents.get('visibilitychange')();
  view.windowEvents.get('focus')?.();
  view.disconnected();
  view.timers.get(1000)();
  view.incoming(snapshot);
  assert.equal(view.sent.at(-1).visible, true, 'reconnect resends ready visibility');
  assert.equal(view.sent.some(message => message.type === 'user_activity'), false);
  view.domEvents.get('pointerdown')({ isTrusted: true });
  assert.deepEqual(view.sent.filter(message => message.type === 'user_activity').map(message => message.generation), [7]);
});


test('mode toggle delegates panel teardown and keeps panel opening in the click gesture', async () => {
  for (const target of ['sidepanel', 'popup']) {
    const panel = target === 'popup';
    const view = await bootstrapView({ popup: !panel, mode: panel ? 'sidepanel' : '' });
    const opened = deferred();
    if (panel) view.context.chrome.action.openPopup = () => { view.viewCalls.push('open-popup'); return opened.promise; };
    else view.context.chrome.sidePanel.open = () => { view.viewCalls.push('open-panel'); return opened.promise; };
    view.context.railoxideHost.command('view', target);
    const request = view.sent.find(message => message.type === 'preferences');
    assert.equal(request.value, target);
    assert.equal(request.handoff_window_id, panel ? 17 : undefined);
    assert.deepEqual(view.viewCalls, panel ? [] : ['open-panel'], 'sidePanel.open runs before leaving the click stack');
    view.context.railoxideHost.command('view', target);
    assert.equal(view.sent.filter(message => message.type === 'preferences').length, 1, 'one pending switch per document');
    view.incoming({ type: 'state', status: 'locked', view: target, viewError: false });
    view.incoming({ type: 'view_result', request_id: request.request_id + 1, success: true });
    await flush();
    assert.deepEqual(view.viewCalls, panel ? [] : ['open-panel'], 'broadcasts and unrelated acknowledgements cannot open Popup or close the view');
    view.incoming({ type: 'view_result', request_id: request.request_id, success: true, opened: true });
    await flush();
    assert.deepEqual(view.viewCalls, panel ? [] : ['open-panel']);
    opened.resolve();
    await flush();
    assert.deepEqual(view.viewCalls, panel ? [] : ['open-panel', 'close-document']);
    view.incoming({ type: 'view_result', request_id: request.request_id, success: true, opened: true });
    await flush();
    assert.equal(view.viewCalls.length, panel ? 0 : 2, 'duplicate acknowledgements cannot repeat the switch');
    assert.equal(view.sent.some(message => message.type === 'user_activity'), false, 'host switching does not manufacture activity');
  }
  const tab = await bootstrapView({ mode: 'sidepanel', tab: true });
  tab.context.railoxideHost.command('view', 'sidepanel');
  const request = tab.sent.find(message => message.type === 'preferences');
  tab.incoming({ type: 'view_result', request_id: request.request_id, success: true });
  await flush();
  assert.deepEqual(tab.viewCalls, ['open-panel'], 'switching never closes an ordinary browser tab');
  tab.context.chrome.action.openPopup = async () => { throw new Error('open rejected'); };
  tab.context.railoxideHost.command('view', 'popup');
  assert.equal(tab.sent.at(-1).handoff_window_id, undefined, 'a sidepanel URL opened in a tab cannot request panel teardown');
  tab.incoming({ type: 'view_result', request_id: tab.sent.at(-1).request_id, success: true });
  await flush();
  assert.deepEqual(tab.viewCalls, ['open-panel'], 'manual fallback cannot close an ordinary browser tab');
});

test('saved modes close their own view when automatic opening fails; failed saves and retired requests do not', async () => {
  for (const failure of ['panel-open', 'handoff', 'save', 'disconnect', 'timeout']) {
    const view = await bootstrapView({ mode: failure === 'panel-open' ? '' : 'sidepanel', popup: failure === 'panel-open' });
    let notice = '';
    view.context.railoxideHost.subscribe(state => { notice = state.view_notice; });
    if (failure === 'panel-open') view.context.chrome.sidePanel.open = async () => { throw new Error('open rejected'); };
    view.context.railoxideHost.command('view', failure === 'panel-open' ? 'sidepanel' : 'popup');
    const request = view.sent.find(message => message.type === 'preferences');
    if (failure === 'disconnect') {
      view.disconnected();
      view.timers.get(1000)();
    }
    if (failure === 'timeout') view.timers.get(10_000)();
    view.incoming({ type: 'view_result', request_id: request.request_id, success: failure !== 'save', opened: false });
    await flush();
    assert.deepEqual(view.viewCalls, ['panel-open', 'handoff'].includes(failure) ? ['close-document'] : [], failure);
    assert.ok(notice.length > 0, `${failure} gives visible guidance`);
    if (['panel-open', 'handoff'].includes(failure)) assert.match(notice, /Mode saved.*toolbar icon/);
    if (['disconnect', 'timeout', 'save'].includes(failure)) assert.deepEqual(view.viewCalls, []);
  }
});


test('host state reports the stored pairing and endpoint pairing waits for a valid code', async () => {
  const view = await bootstrapView();
  let last;
  view.context.railoxideHost.subscribe(state => { last = state; });
  await flush();
  assert.equal(last.paired, false);
  view.incoming({ type: 'state', status: 'locked', endpoint: 'desktop.local', paired: true, takeover: true });
  await flush();
  assert.deepEqual(JSON.parse(JSON.stringify(last)), { status: 'locked', endpoint: 'desktop.local', paired: true,
    takeover: true, metamask: false, view: 'popup', view_notice: '' });
  view.disconnected();
  await flush();
  assert.deepEqual([last.status, last.paired], ['disconnected', true], 'a lost port does not forget the stored pairing');
  view.timers.get(1000)();
  view.context.railoxideHost.pair('12345', endpoint(44000));
  view.context.railoxideHost.pair('123456', 'ws://127.0.0.1/');
  await flush();
  assert.equal(last.status, 'config_failed');
  assert.equal(view.sent.some(message => message.type === 'pair'), false);
  view.context.railoxideHost.pair('123456', endpoint(44000));
  await flush();
  assert.deepEqual(JSON.parse(JSON.stringify(view.sent.filter(message => message.type === 'pair'))),
    [{ type: 'pair', code: '123456', endpoint: endpoint(44000) }]);
  view.context.railoxideHost.command('unpair', '');
  assert.deepEqual(JSON.parse(JSON.stringify(view.sent.at(-1))), { type: 'unpair' });
});

test('worker closes the saved side panel before opening Popup and survives its port disconnect', async () => {
  const h = await worker({ gatewayPreferences: { view: 'sidepanel' } });
  const panel = h.ui('chrome-extension://test/index.html?mode=sidepanel');
  const closed = deferred(), opened = deferred(), calls = [];
  h.chrome.sidePanel.close = options => {
    assert.equal(h.data.gatewayPreferences.view, 'popup');
    calls.push(['close', options.windowId]);
    panel.port.disconnect();
    return closed.promise;
  };
  h.chrome.action.openPopup = options => { calls.push(['open', options.windowId]); return opened.promise; };
  panel.request({ type: 'preferences', key: 'view', value: 'popup', request_id: 1, handoff_window_id: 17 });
  await flush();
  assert.deepEqual(calls, [['close', 17]]);
  closed.resolve();
  await flush();
  assert.deepEqual(calls, [['close', 17], ['open', 17]]);
  h.command({ type: 'preferences', key: 'view', value: 'sidepanel', request_id: 2 });
  await flush();
  assert.equal(h.data.gatewayPreferences.view, 'popup', 'handoff retains command serialization until opening settles');
  opened.resolve();
  await flush();
  h.command({ type: 'preferences', key: 'view', value: 'sidepanel', request_id: 3 });
  await flush();
  assert.equal(h.data.gatewayPreferences.view, 'sidepanel');
});

test('worker handoffs require a live side panel and saved mode; API failures preserve the preference', async () => {
  for (const failure of ['save', 'close', 'open', 'unsupported-close', 'unsupported-popup', 'disconnected', 'ordinary-view', 'invalid-window', 'invalid-request']) {
    const h = await worker({ gatewayPreferences: { view: 'sidepanel' } });
    const panel = h.ui(`chrome-extension://test/index.html${failure === 'ordinary-view' ? '' : '?mode=sidepanel'}`);
    const calls = [];
    h.chrome.sidePanel.close = async options => {
      calls.push(['close', options.windowId]);
      if (failure === 'close') throw new Error('No window with id');
    };
    h.chrome.action.openPopup = async options => {
      calls.push(['open', options.windowId]);
      if (failure === 'open') throw new Error('Could not find active browser window');
    };
    if (failure === 'unsupported-close') delete h.chrome.sidePanel.close;
    if (failure === 'unsupported-popup') delete h.chrome.action.openPopup;
    h.hook((operation) => {
      if (operation === 'set' && failure === 'save') throw new Error('storage failed');
      if (operation === 'set' && failure === 'disconnected') panel.port.disconnect();
    });
    panel.request({ type: 'preferences', key: 'view', value: 'popup',
      request_id: failure === 'invalid-request' ? 0 : 1, handoff_window_id: failure === 'invalid-window' ? -2 : 17 });
    await flush();
    assert.deepEqual(calls, failure === 'close' ? [['close', 17]] : failure === 'open' ? [['close', 17], ['open', 17]] : [], failure);
    assert.equal(h.data.gatewayPreferences.view, failure === 'save' ? 'sidepanel' : 'popup', failure);
    if (failure !== 'disconnected') {
      const result = panel.port.messages.at(-1);
      assert.equal(result.type, 'view_result');
      assert.equal(result.success, failure !== 'save');
      assert.equal(result.opened, false);
    }
  }
});


test('draft commands are UI-only, scoped to current desktop selection, and never replay after reconnect', async () => {
  const h = await worker({ gatewayCredential: credential() });
  const session = await established(h);
  const input = { account: 'first', chain_id: 1, kind: 'send', asset: 'native', amount: '1', recipient: 'recipient.eth',
    address_book_entry: null, fee: { mode: 'normal' }, max: false, mimic_railway: false };
  const snapshot = { type: 'ui_snapshot', version: 1, generation: 1, locked: false, accounts: [{ uuid: 'first' }], chains: [{ id: 1 }],
    pending_connects: [], pending_requests: [], public_view: { selected_account: 'first', selected_chain: 1,
      drafts: [{ draft_id: 'draft', revision: 3, input, status: 'ready' }] } };
  await session.send(snapshot);
  const ui = h.ui();
  const sent = () => session.socket.sent.map(bytes => { try { return JSON.parse(new TextDecoder().decode(Uint8Array.from(bytes))); } catch { return null; } }).filter(Boolean);
  const command = draft => ui.request({ type: 'public_view', generation: 1, command: { type: 'draft', command: draft } });
  const before = sent().length;
  for (const altered of [{ ...input, account: 'other' }, { ...input, chain_id: 10 }, { ...input, amount: '1'.repeat(101) }]) {
    command({ action: 'create', request_id: 'new', input: altered });
  }
  command({ action: 'submit', draft_id: 'unknown', revision: 3 });
  assert.equal(sent().length, before);
  command({ action: 'update', draft_id: 'draft', revision: 4, input: { ...input, rpc_origin: 'other', password: 'never-forward' } });
  command({ action: 'submit', draft_id: 'draft', revision: 4, password: 'never-forward' });
  assert.deepEqual(sent().slice(before).map(message => message.command), [
    { type: 'draft', command: { action: 'update', draft_id: 'draft', revision: 4, input } },
    { type: 'draft', command: { action: 'submit', draft_id: 'draft', revision: 4 } },
  ]);
  const provider = h.provider(); await flush();
  provider.request({ id: 'draft-request', method: 'public_view', params: { type: 'draft', command: { action: 'submit', draft_id: 'draft', revision: 4 } } });
  await flush();
  assert.equal(sent().filter(message => message.type === 'public_view').length, 2);
  session.socket.close(); await flush();
  await h.alarm();
  const socket = h.sockets.at(-1);
  socket.open(); socket.message(2); await flush();
  socket.message(new TextEncoder().encode(JSON.stringify({ type: 'state', version: 1, generation: 1, locked: false })).buffer); await flush();
  socket.message(new TextEncoder().encode(JSON.stringify(snapshot)).buffer); await flush();
  const reopened = h.ui(); await flush();
  assert.equal(reopened.port.messages[0].public_view.drafts[0].draft_id, 'draft');
  assert.equal(socket.sent.some(bytes => new TextDecoder().decode(Uint8Array.from(bytes)).includes('"submit"')), false);
});
