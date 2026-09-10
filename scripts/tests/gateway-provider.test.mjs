import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { webcrypto } from 'node:crypto';
import test from 'node:test';
import vm from 'node:vm';

const read = name => readFile(new URL(`../../extensions/railoxide/${name}`, import.meta.url), 'utf8');
const { createPageBridge } = await import(`data:text/javascript,${encodeURIComponent(await read('gateway-page-bridge.js'))}`);
const providerSource = await read('gateway-provider.js');
const contentSource = await read('gateway-content.js');
const flush = () => new Promise(setImmediate);
function event() {
  const listeners = new Set();
  return { addListener: listener => listeners.add(listener), fire: value => { for (const listener of listeners) listener(value); } };
}
function bridgeHarness() {
  let owner = { generation: 1, locked: false };
  let frameHook;
  const frames = new Map();
  const sent = [];
  const navigation = { onCommitted: event(), onHistoryStateUpdated: event(), onReferenceFragmentUpdated: event(),
    async getFrame(query) { await frameHook?.(); return frames.get(query.documentId); } };
  const bridge = createPageBridge({ chrome: { runtime: { id: 'extension' }, webNavigation: navigation }, crypto: webcrypto,
    send: (session, value) => sent.push({ session, ...value }), session: () => owner,
    preferences: () => ({ takeover: false, metamask: false }) });
  function port(url = 'https://example.test/path?query#fragment', frameId = frames.size, id = webcrypto.randomUUID()) {
    const sender = { id: 'extension', tab: { id: 1 }, frameId, documentId: id, url };
    frames.set(id, { documentId: id, documentLifecycle: 'active', url });
    const result = { name: 'gateway-provider-v1', sender, messages: [], onMessage: event(), onDisconnect: event(),
      postMessage(value) { this.messages.push(structuredClone(value)); },
      disconnect() { this.closed = true; this.onDisconnect.fire(); } };
    bridge.connect(result);
    return result;
  }
  return { bridge, sent, navigation, port, frames, get owner() { return owner; },
    reconnect() { bridge.retire(); owner = { generation: 1, locked: false }; bridge.authenticated(); },
    hook(value) { frameHook = value; },
    state(document, document_generation = 0, accounts = [], invalidation_code) {
      bridge.receive(owner, { type: 'provider_state', document, generation: owner.generation, document_generation, invalidation_code, accounts, chain_id: '0x1' });
    } };
}

test('page input cannot dispatch UI methods or replace browser identity and params', async () => {
  const h = bridgeHarness();
  const p = h.port('https://EXAMPLE.test:443/path?query#fragment');
  await flush();
  const registration = h.sent[0];
  assert.equal(registration.url, 'https://example.test');
  for (const method of ['pair', 'configure', 'retry', 'preferences', 'resolve_connect', 'eth_subscribe', 'eth_sendRawTransaction', 'eth_sign', 'debug_traceCall']) {
    p.onMessage.fire({ id: method, method, type: 'pair', code: '123456' });
    assert.equal(p.messages.at(-1).error.code, 4200);
  }
  assert.equal(h.sent.length, 1);
  const params = [{ input: '0x00', future: { unchanged: true } }, { blockHash: '0x01' }];
  p.onMessage.fire({ id: 'page-id', method: 'eth_call', params, type: 'configure', document: 'forged', url: 'https://forged.test', peer: 'forged' });
  await flush();
  const wire = h.sent.at(-1);
  assert.equal(wire.document, registration.document);
  assert.notEqual(wire.request_id, 'page-id');
  assert.equal(wire.type, 'provider_request');
  assert.deepEqual(wire.params, params);
  assert.equal(Object.hasOwn(wire, 'url'), false);
});

test('navigation preserves matching documents but retires changed origins and old descendants', async () => {
  const h = bridgeHarness();
  const old = h.port(); const child = h.port('https://child.test/');
  await flush();
  const oldDocuments = h.sent.map(value => value.document);
  const p = h.port(old.sender.url, 0);
  await flush();
  const document = h.sent.at(-1).document;
  h.state(document);
  p.onMessage.fire({ id: 'sent', method: 'eth_blockNumber' });
  await flush();
  const request = h.sent.at(-1);
  const navigation = { tabId: 1, frameId: 0, documentId: p.sender.documentId,
    documentLifecycle: 'active', url: p.sender.url };
  h.navigation.onCommitted.fire(navigation);
  assert.equal(old.closed, true);
  assert.equal(child.closed, true);
  assert.equal(p.closed, undefined);
  assert.deepEqual(h.sent.filter(value => value.type === 'unregister_document').map(value => value.document), oldDocuments);
  h.navigation.onHistoryStateUpdated.fire(navigation);
  h.navigation.onReferenceFragmentUpdated.fire(navigation);
  h.navigation.onHistoryStateUpdated.fire({ ...navigation, documentId: old.sender.documentId, url: 'https://example.test/old' });
  assert.equal(p.closed, undefined);
  assert.equal(p.messages.some(value => value.type === 'response'), false);
  let release;
  h.hook(() => new Promise(resolve => { release = resolve; }));
  p.onMessage.fire({ id: 'attesting', method: 'eth_getBalance' });
  h.navigation.onReferenceFragmentUpdated.fire({ ...navigation, url: 'https://other.test/path' });
  assert.equal(p.closed, true);
  assert.equal(h.sent.at(-1).type, 'unregister_document');
  assert.equal(h.sent.at(-1).document, document);
  assert.deepEqual(p.messages.filter(value => value.type === 'response').map(value => value.error.code), [4900, 4900]);
  const before = p.messages.length;
  release(); await flush();
  h.bridge.receive(h.owner, { type: 'provider_response', document, request_id: request.request_id,
    generation: 1, document_generation: 0, result: 'stale' });
  assert.equal(p.messages.length, before);
  assert.equal(h.sent.filter(value => value.type === 'provider_request').length, 1);
});

test('fresh ports attest current origins and preserve provider promises across hash and history navigation', async () => {
  const h = bridgeHarness();
  const initialUrl = 'https://swap.cow.fi/';
  const p = h.port(initialUrl, 0);
  const frame = h.frames.get(p.sender.documentId);
  frame.url = `${initialUrl}#/1/swap/WETH/USDC`;
  await flush();
  assert.equal(p.closed, undefined);
  const registration = h.sent.at(-1);
  assert.equal(registration.url, 'https://swap.cow.fi');
  const provider = page();
  let delivered = 0;
  const deliver = () => { for (; delivered < p.messages.length; delivered += 1) provider.deliver(p.messages[delivered]); };
  deliver();
  h.state(registration.document, 0, ['account']);
  deliver();
  const events = [];
  for (const name of ['accountsChanged', 'chainChanged', 'disconnect']) provider.provider.on(name, () => events.push(name));
  const pending = ['eth_requestAccounts', 'eth_call'].map(method => {
    const promise = provider.provider.request({ method });
    p.onMessage.fire(provider.outbound.at(-1));
    return promise;
  });
  const result = Promise.all(pending);
  // Attach a rejection handler before exercising navigation, including failing regressions.
  result.catch(() => {});
  await flush();
  const requests = h.sent.filter(value => value.type === 'provider_request');
  for (const [event, url] of [
    [h.navigation.onReferenceFragmentUpdated, `${initialUrl}#/1/swap/USDC/WETH`],
    [h.navigation.onHistoryStateUpdated, `${initialUrl}trade?chain=1#review`],
  ]) {
    frame.url = url;
    event.fire({ tabId: 1, frameId: 0, ...frame });
    deliver();
    assert.equal(p.closed, undefined);
    assert.equal(provider.provider.isConnected(), true);
    h.state(registration.document, 0, ['account']);
    deliver();
    assert.deepEqual(events, []);
    assert.equal(p.messages.some(value => value.type === 'response'), false);
    p.onMessage.fire({ id: url, method: 'eth_accounts' });
    await flush();
    assert.equal(h.sent.at(-1).type, 'provider_request');
    assert.equal(h.sent.at(-1).document, registration.document);
  }
  assert.equal(h.sent.filter(value => value.type === 'register_document').length, 1);
  assert.equal(h.sent.some(value => value.type === 'unregister_document'), false);
  for (const request of requests) h.bridge.receive(h.owner, {
    type: 'provider_response', document: registration.document, request_id: request.request_id,
    generation: 1, document_generation: 0, result: request.method === 'eth_call' ? '0x01' : ['account'],
  });
  deliver();
  assert.deepEqual(await result, [['account'], '0x01']);
});

test('browser attestation rejects opaque and non-web origins before registration', async () => {
  const h = bridgeHarness();
  const rejected = ['data:text/html,hello', 'about:blank', 'file:///tmp/page.html', 'ftp://example.test/page', 'blob:null/id', 'not a URL'].map(url => h.port(url));
  await flush();
  assert.ok(rejected.every(port => port.closed));
  assert.deepEqual(h.sent, []);
  const blob = h.port('blob:https://example.test/document');
  await flush();
  assert.equal(blob.closed, undefined);
  assert.equal(h.sent.at(-1).url, 'https://example.test');
});

test('document generations isolate origins and preserve complete remote errors only for live owners', async () => {
  const h = bridgeHarness();
  const p = h.port(); const q = h.port('https://other.test/'); await flush();
  const [a, b] = h.sent;
  h.state(a.document); h.state(b.document);
  p.onMessage.fire({ id: 'a', method: 'eth_call' }); q.onMessage.fire({ id: 'b', method: 'eth_call' }); await flush();
  const request = h.sent.at(-1);
  h.state(a.document, 1, [], 4100);
  assert.equal(p.messages.at(-2).error.code, 4100);
  p.onMessage.fire({ id: 'chain', method: 'eth_blockNumber' }); await flush();
  h.state(a.document, 2, [], 4901);
  assert.equal(p.messages.at(-2).error.code, 4901);
  const error = { code: -32000, message: 'endpoint error', data: { nested: ['0xdead'] }, unknown: { retained: null } };
  h.bridge.receive(h.owner, { type: 'provider_response', document: b.document, request_id: request.request_id,
    generation: 1, document_generation: 0, error });
  assert.deepEqual(q.messages.at(-1).error, error);
  assert.equal(p.messages.some(value => value.error?.message === error.message), false);
  h.owner.generation = 2; h.owner.locked = true; h.bridge.lockState(true);
  h.bridge.receive(h.owner, { type: 'provider_state', document: b.document, generation: 1, document_generation: 0, accounts: ['leak'], chain_id: '0x1' });
  assert.deepEqual(q.messages.at(-1).accounts, []);
});

function page(preferences, ethereum) {
  const listeners = new Map(); const announcements = []; const outbound = [];
  const window = { ethereum, addEventListener(name, listener) { if (!listeners.has(name)) listeners.set(name, []); listeners.get(name).push(listener); },
    dispatchEvent(value) { if (value.type === 'eip6963:announceProvider') announcements.push(value.detail); for (const listener of listeners.get(value.type) ?? []) listener(value); },
    postMessage(value) { outbound.push(structuredClone(value)); } };
  const deliver = value => window.dispatchEvent({ type: 'message', source: window, data: { channel: 'railoxide-provider-v1', direction: 'provider', ...value } });
  vm.runInNewContext(providerSource, { window, crypto: webcrypto, console: { warn() {}, log() {} }, CustomEvent: class { constructor(type, values) { this.type = type; Object.assign(this, values); } } });
  if (preferences !== undefined) deliver({ type: 'preferences', ...preferences });
  return { window, announcements, outbound, deliver, provider: announcements[0].provider };
}

test('EIP-6963 discovery coexists with another wallet and opt-ins are independent', () => {
  for (const takeover of [false, true]) for (const metamask of [false, true]) {
    const existing = { otherWallet: true };
    const h = page({ takeover, metamask }, existing);
    assert.equal(h.window.ethereum, takeover ? h.provider : existing);
    assert.equal(h.provider.isMetaMask === true, metamask);
    h.window.dispatchEvent({ type: 'eip6963:requestProvider' });
    assert.equal(h.announcements.at(-1), h.announcements[0]);
    assert.equal(Object.isFrozen(h.announcements[0]), true);
    assert.equal(Object.isFrozen(h.announcements[0].info), true);
    assert.match(h.announcements[0].info.icon, /^data:image\//);
    h.deliver({ type: 'preferences', takeover: !takeover, metamask: !metamask });
    assert.equal(h.window.ethereum, takeover ? h.provider : existing);
    assert.equal(h.provider.isMetaMask === true, metamask);
  }
});

test('provider rejects promises with all remote error fields and emits owned state events', async () => {
  const h = page();
  const before = h.outbound.length;
  for (const method of ['resolve_connect', 'eth_subscribe', 'eth_sendRawTransaction', 'unknown']) {
    await assert.rejects(h.provider.request({ method }), value => value.code === 4200);
  }
  assert.equal(h.outbound.length, before);
  h.deliver({ type: 'preferences' });
  const seen = []; const listener = value => seen.push(value);
  h.provider.on('accountsChanged', listener);
  h.deliver({ type: 'state', accounts: ['account'], chainId: '0x1' });
  h.provider.removeListener('accountsChanged', listener);
  h.deliver({ type: 'state', accounts: [], chainId: null });
  assert.deepEqual(seen, [['account']]);
  const request = h.provider.request({ method: 'eth_call', params: [] });
  const error = { code: -32000, message: 'remote', data: ['0x123'], future: { retained: true } };
  h.deliver({ type: 'response', id: h.outbound.at(-1).id, error });
  await assert.rejects(request, value => { assert.deepEqual(JSON.parse(JSON.stringify(value)), error); return true; });
});

test('isolated bridge strips page commands and identity before the runtime port', () => {
  const listeners = new Map(); const sent = [];
  const port = { onMessage: event(), onDisconnect: event(), postMessage: value => sent.push(value), disconnect() {} };
  const window = { addEventListener: (name, callback) => listeners.set(name, callback), postMessage() {} };
  vm.runInNewContext(contentSource, { window, chrome: { runtime: { connect: () => port } }, setTimeout, clearTimeout });
  listeners.get('message')({ source: window, data: { channel: 'railoxide-provider-v1', direction: 'request', id: '1',
    method: 'eth_accounts', params: [], type: 'preferences', url: 'spoof', document: 'spoof', peer: 'spoof' } });
  assert.deepEqual(JSON.parse(JSON.stringify(sent)), [{ id: '1', method: 'eth_accounts', params: [] }]);
});

test('isolated ready replays state without replacing a live port or losing pending RPCs', () => {
  const listeners = new Map(); const ports = []; const delivered = []; const timers = new Set();
  const window = { addEventListener: (name, callback) => listeners.set(name, callback),
    postMessage: value => delivered.push(structuredClone(value)) };
  const runtime = { connect() {
    const port = { sent: [], onMessage: event(), onDisconnect: event(), disconnects: 0,
      postMessage(value) { this.sent.push(value); },
      disconnect() { this.disconnects += 1; this.onDisconnect.fire(); } };
    ports.push(port); return port;
  } };
  vm.runInNewContext(contentSource, { window, chrome: { runtime },
    setTimeout(callback) { timers.add(callback); return callback; }, clearTimeout: callback => timers.delete(callback) });
  const request = value => listeners.get('message')({ source: window,
    data: { channel: 'railoxide-provider-v1', direction: 'request', ...value } });
  const replay = () => {
    delivered.length = 0;
    request({ type: 'ready' });
    return delivered.map(({ channel, direction, ...value }) => value);
  };
  const original = ports[0];
  const preferences = { type: 'preferences', takeover: true, metamask: true };
  const state = { type: 'state', accounts: ['account'], chainId: '0x1' };
  original.onMessage.fire(preferences);
  original.onMessage.fire(state);
  request({ id: 'pending', method: 'eth_accounts' });
  assert.deepEqual(replay(), [preferences, state]);
  assert.deepEqual(replay(), [preferences, state]);
  assert.equal(ports.length, 1);
  assert.equal(original.disconnects, 0);
  assert.equal(original.sent.length, 1);
  original.onMessage.fire({ type: 'response', id: 'pending', result: ['account'] });
  assert.equal(delivered.at(-1).id, 'pending');
  assert.deepEqual(replay(), [preferences, state]);
  const reset = { type: 'reset', error: { code: 4100, message: 'Wallet access unavailable.' } };
  original.onMessage.fire(reset);
  assert.deepEqual(replay(), [preferences, reset]);
  original.onMessage.fire(state);
  original.disconnect();
  const disconnected = replay();
  assert.equal(disconnected[1].type, 'reset');
  assert.equal(disconnected[1].error.code, 4900);
  assert.equal(ports.length, 2);
  assert.equal(timers.size, 0);
  const before = delivered.length;
  original.onMessage.fire(state);
  original.onDisconnect.fire();
  assert.equal(delivered.length, before);
  assert.deepEqual(replay(), disconnected);
  ports[1].onMessage.fire(state);
  listeners.get('pagehide')();
  assert.deepEqual(replay(), disconnected);
  assert.equal(ports.length, 2);
  listeners.get('pageshow')({ persisted: true });
  assert.equal(ports.length, 3);
});

test('attestation waiting is bounded and cannot dispatch after its browser origin changes', async () => {
  const h = bridgeHarness(); const p = h.port(); await flush();
  const releases = [];
  h.hook(() => new Promise(resolve => releases.push(resolve)));
  for (let i = 0; i < 129; i += 1) p.onMessage.fire({ id: String(i), method: 'eth_blockNumber' });
  assert.equal(p.messages.at(-1).error.code, -32005);
  assert.equal(releases.length, 128);
  h.frames.get(p.sender.documentId).url = 'https://example.test:8443/changed';
  for (const release of releases) release();
  await flush();
  assert.equal(p.closed, true);
  assert.equal(h.sent.some(value => value.type === 'provider_request'), false);
});


test('connectivity transitions commit before callbacks and preserve bridge pending policy', async () => {
  const h = bridgeHarness(); const port = h.port(); const p = page(); await flush();
  let delivered = 0;
  const deliver = () => { for (; delivered < port.messages.length; delivered += 1) p.deliver(port.messages[delivered]); };
  deliver();
  const seen = [];
  for (const name of ['accountsChanged', 'chainChanged', 'connect', 'disconnect']) {
    p.provider.on(name, value => seen.push({ name, connected: p.provider.isConnected(), code: value?.code }));
  }
  const document = h.sent[0].document;
  const state = (chain, generation = 1) => {
    h.bridge.receive(h.owner, { type: 'provider_state', document, generation: h.owner.generation,
      document_generation: generation, accounts: chain === null ? [] : ['account'], chain_id: chain });
    deliver();
  };
  state(null); state(null);
  assert.deepEqual(seen, []);
  state('0x1');
  assert.equal(seen.filter(value => value.name === 'connect').length, 1);
  assert.ok(seen.every(value => value.connected));
  const requests = ['eth_requestAccounts', 'eth_accounts', 'eth_call'].map(method => {
    const promise = p.provider.request({ method });
    const wire = p.outbound.at(-1);
    port.onMessage.fire(wire);
    return { method, promise, id: wire.id };
  });
  const ordinaryFailure = assert.rejects(requests[2].promise, error => error.code === -32002);
  await flush();
  h.owner.generation += 1;
  h.bridge.lockState(false, true); deliver();
  assert.equal(p.provider.isConnected(), false);
  assert.deepEqual(seen.filter(value => value.name === 'disconnect'), [{ name: 'disconnect', connected: false, code: 4900 }]);
  await ordinaryFailure;
  assert.equal(port.messages.filter(value => value.type === 'response').length, 1);
  state('0x1');
  assert.equal(seen.filter(value => value.name === 'connect').length, 2);
  for (const request of requests.slice(0, 2)) {
    const wire = h.sent.find(value => value.method === request.method);
    h.bridge.receive(h.owner, { type: 'provider_response', document, generation: h.owner.generation,
      document_generation: 1, request_id: wire.request_id, result: ['account'] });
    deliver(); assert.deepEqual(await request.promise, ['account']);
  }
  state('0xa', 2);
  assert.equal(seen.filter(value => value.name === 'disconnect').length, 1);
  assert.equal(seen.filter(value => value.name === 'connect').length, 2);
  state(null, 3); state(null, 3);
  assert.equal(seen.filter(value => value.name === 'disconnect').length, 2);
  state('0xa', 4);
  h.bridge.retire(); deliver();
  h.bridge.retire(); deliver();
  assert.equal(seen.filter(value => value.name === 'disconnect').length, 3);
  assert.ok(seen.filter(value => value.name === 'connect').every(value => value.connected));
  assert.ok(seen.filter(value => value.name === 'disconnect').every(value => !value.connected && value.code === 4900));
  assert.equal(seen.at(-1).connected, false);
});


test('registration capacity rejection releases pending quota without retiring accepted documents', async () => {
  const h = bridgeHarness();
  const accepted = Array.from({ length: 128 }, () => h.port());
  const rejected = Array.from({ length: 8 }, () => h.port());
  await flush();
  const registrations = h.sent.slice();
  for (const registration of registrations.slice(0, 128)) h.state(registration.document);
  // Fill the worker quota with requests from documents awaiting native registration.
  const releases = [];
  h.hook(() => new Promise(resolve => releases.push(resolve)));
  for (const port of rejected) {
    for (let i = 0; i < 128; i += 1) port.onMessage.fire({ id: String(i), method: 'eth_blockNumber' });
  }
  accepted[0].onMessage.fire({ id: 'full', method: 'eth_blockNumber' });
  assert.equal(accepted[0].messages.at(-1).error.code, -32005);
  const oldOwner = h.owner;
  const rejection = document => ({ type: 'provider_response', document, request_id: '',
    generation: 1, document_generation: 0, error: { code: -32005, message: 'Provider request capacity exceeded.' } });
  for (const registration of registrations.slice(128)) h.bridge.receive(oldOwner, rejection(registration.document));
  for (const port of rejected) {
    assert.equal(port.closed, undefined);
    const replies = port.messages.filter(value => value.type === 'response');
    assert.equal(replies.length, 128);
    assert.ok(replies.every(value => value.error.code === -32005));
    port.onMessage.fire({ id: 'after', method: 'eth_blockNumber' });
    assert.equal(port.messages.at(-1).error.code, -32005);
  }
  for (const release of releases) release();
  h.hook(null); await flush();
  assert.equal(h.sent.filter(value => value.type === 'provider_request').length, 0);
  // The released quota is available to an accepted document immediately.
  accepted[0].onMessage.fire({ id: 'live', method: 'eth_blockNumber' });
  await flush();
  const wire = h.sent.at(-1);
  assert.equal(wire.type, 'provider_request');
  h.bridge.receive(h.owner, { ...wire, type: 'provider_response', generation: 1, document_generation: 0, result: '0x1' });
  assert.equal(accepted[0].messages.at(-1).result, '0x1');
  h.reconnect(); await flush();
  const replacement = h.sent.filter(value => value.type === 'register_document').slice(-136)[128];
  h.state(replacement.document);
  // Even an envelope naming the replacement document cannot act for an old session.
  h.bridge.receive(oldOwner, rejection(replacement.document));
  rejected[0].onMessage.fire({ id: 'replacement', method: 'eth_blockNumber' });
  await flush();
  assert.equal(h.sent.at(-1).document, replacement.document);
  assert.equal(h.sent.at(-1).type, 'provider_request');
});
