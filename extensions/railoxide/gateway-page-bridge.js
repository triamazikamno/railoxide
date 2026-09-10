// Browser identity stays in this worker module. Page messages supply only RPC input.
const remote = new Set([
  'eth_call', 'eth_getBalance', 'eth_blockNumber', 'eth_getCode', 'eth_getStorageAt',
  'eth_getTransactionCount', 'eth_getBlockByHash', 'eth_getBlockByNumber',
  'eth_getBlockTransactionCountByHash', 'eth_getBlockTransactionCountByNumber',
  'eth_getTransactionByHash', 'eth_getTransactionReceipt', 'eth_getTransactionByBlockHashAndIndex',
  'eth_getTransactionByBlockNumberAndIndex', 'eth_getBlockReceipts', 'eth_getLogs',
  'eth_gasPrice', 'eth_maxPriorityFeePerGas', 'eth_feeHistory', 'eth_estimateGas',
]);
const approval = new Set(['eth_requestAccounts', 'personal_sign', 'eth_signTypedData',
  'eth_signTypedData_v4', 'eth_sendTransaction', 'wallet_switchEthereumChain',
  'wallet_addEthereumChain', 'wallet_watchAsset']);
const supported = method => remote.has(method) || approval.has(method) ||
  method === 'eth_accounts' || method === 'eth_chainId';
const failure = code => ({ code, message: ({ 4100: 'Wallet access unavailable.',
  4200: 'Unsupported provider method.', 4900: 'Desktop provider disconnected.',
  4901: 'Requested chain unavailable.',
  '-32005': 'Provider request capacity exceeded.', '-32002': 'Provider state changed.' })[code] });

function webOrigin(url) {
  if (typeof url !== 'string') return null;
  try {
    const origin = new URL(url).origin;
    return origin.startsWith('https://') || origin.startsWith('http://') ? origin : null;
  } catch { return null; }
}

export function createPageBridge({ chrome, crypto, send, session, preferences }) {
  const documents = new Map();
  let totalPending = 0;
  let attesting = 0;
  const post = (doc, value) => { try { doc.port.postMessage(value); } catch { close(doc); } };
  function failPending(doc, code, ordinaryOnly = false) {
    for (const [id, pending] of doc.pending) {
      if (ordinaryOnly && (approval.has(pending.method) || pending.method === 'eth_accounts')) continue;
      doc.pending.delete(id);
      totalPending -= 1;
      post(doc, { type: 'response', id: pending.id, error: failure(code) });
    }
  }
  function purge(doc, code) {
    failPending(doc, code);
    doc.generation = -1;
    doc.documentGeneration = -1;
    post(doc, { type: 'reset', error: failure(code) });
  }
  function unregister(doc) {
    if (doc.owner && doc.owner === session()) {
      try { send(doc.owner, { type: 'unregister_document', version: 1, document: doc.id }); } catch { /* Transport teardown also retires native ownership. */ }
    }
    doc.owner = null;
  }
  function close(doc) {
    if (!documents.delete(doc.port)) return;
    unregister(doc);
    purge(doc, 4900);
    doc.port.disconnect();
  }
  async function attest(doc, expected) {
    if (attesting >= 1280) throw new Error('Browser attestation capacity exceeded.');
    attesting += 1;
    let frame;
    try {
      frame = await chrome.webNavigation.getFrame({ tabId: doc.tabId, frameId: doc.frameId, documentId: doc.browserDocument });
    } finally { attesting -= 1; }
    if (documents.get(doc.port) !== doc || session() !== expected) return false;
    const origin = webOrigin(frame?.url);
    if (!frame || frame.documentId !== doc.browserDocument || frame.documentLifecycle !== 'active' ||
        origin === null || (doc.url !== null && origin !== doc.url)) {
      close(doc);
      return false;
    }
    // A content port's sender URL can predate same-document navigation.
    if (doc.url === null) doc.url = origin;
    return true;
  }
  async function register(doc) {
    const expected = session();
    if (!expected || doc.registering || doc.owner) return;
    doc.registering = true;
    try {
      if (!await attest(doc, expected)) return;
      doc.registrationFailure = null;
      doc.id = crypto.randomUUID();
      doc.owner = expected;
      doc.generation = -1;
      doc.documentGeneration = -1;
      send(expected, { type: 'register_document', version: 1, document: doc.id, url: doc.url });
    } catch { close(doc); }
    finally {
      doc.registering = false;
      if (documents.get(doc.port) === doc && session() && session() !== expected) void register(doc);
    }
  }
  function connect(port) {
    const sender = port.sender;
    if (port.name !== 'gateway-provider-v1') return false;
    if (sender?.id !== chrome.runtime.id || !Number.isInteger(sender.tab?.id) || sender.tab.id < 0 ||
        !Number.isInteger(sender.frameId) || sender.frameId < 0 || typeof sender.documentId !== 'string' || !sender.documentId ||
        typeof sender.url !== 'string' || !sender.url || documents.size >= 256) { port.disconnect(); return true; }
    const doc = { port, tabId: sender.tab.id, frameId: sender.frameId, browserDocument: sender.documentId,
      url: null, pending: new Map(), owner: null, generation: -1, documentGeneration: -1, sequence: 0, registering: false, registrationFailure: null };
    documents.set(port, doc);
    port.onDisconnect.addListener(() => close(doc));
    port.onMessage.addListener(message => {
      if (documents.get(port) !== doc || !message || typeof message.id !== 'string' || message.id.length > 128) return;
      const reply = code => post(doc, { type: 'response', id: message.id, error: failure(code) });
      if (typeof message.method !== 'string' || !supported(message.method)) { reply(4200); return; }
      if (doc.registrationFailure !== null) { reply(doc.registrationFailure); return; }
      const expected = session();
      if (!expected || doc.owner !== expected) { reply(4900); return; }
      // Includes requests awaiting browser attestation. Desktop admission remains authoritative.
      if (doc.pending.size >= 128 || totalPending >= 1024 || attesting >= 1280) { reply(-32005); return; }
      const requestId = String(++doc.sequence);
      const pending = { id: message.id, method: message.method };
      doc.pending.set(requestId, pending);
      totalPending += 1;
      void (async () => {
        if (!await attest(doc, expected) || doc.owner !== expected || doc.pending.get(requestId) !== pending) return;
        send(expected, { type: 'provider_request', version: 1, document: doc.id, request_id: requestId,
          method: message.method, params: message.params === undefined ? [] : message.params });
      })().catch(() => close(doc));
    });
    void Promise.resolve(preferences()).then(value => {
      if (documents.get(port) === doc) post(doc, { type: 'preferences', ...value });
    }).catch(() => close(doc));
    post(doc, { type: 'reset', error: failure(4900) });
    void register(doc);
    return true;
  }
  function receive(expected, message) {
    if (expected !== session()) return;
    const doc = [...documents.values()].find(value => value.id === message.document && value.owner === expected);
    if (!doc || message.generation !== expected.generation ||
        !Number.isSafeInteger(message.document_generation) || message.document_generation < 0) return;
    if (message.type === 'provider_state') {
      if (message.document_generation < doc.documentGeneration ||
          !Array.isArray(message.accounts) || !message.accounts.every(account => typeof account === 'string') ||
          !(message.chain_id === null || typeof message.chain_id === 'string') ||
          (expected.locked && (message.accounts.length || message.chain_id !== null))) return;
      if (doc.documentGeneration >= 0 && (message.document_generation !== doc.documentGeneration || message.generation !== doc.generation)) {
        const code = [4100, 4900, 4901, -32002].includes(message.invalidation_code) ? message.invalidation_code : -32002;
        failPending(doc, code, true);
      }
      doc.generation = message.generation;
      doc.documentGeneration = message.document_generation;
      post(doc, { type: 'state', accounts: message.accounts, chainId: message.chain_id });
    } else if (message.type === 'provider_response') {
      // Native registration errors have no request owner. Retire this logical
      // document without disconnecting the content port and triggering retries.
      if (message.request_id === '' && message.document_generation === 0 &&
          Object.hasOwn(message, 'error')) {
        const code = [4100, 4900, 4901, -32002, -32005].includes(message.error?.code) ? message.error.code : 4900;
        doc.registrationFailure = code;
        doc.owner = null;
        purge(doc, code);
        return;
      }
      const pending = doc.pending.get(message.request_id);
      if (!pending) return;
      doc.pending.delete(message.request_id);
      totalPending -= 1;
      if (message.generation !== doc.generation || message.document_generation !== doc.documentGeneration) {
        post(doc, { type: 'response', id: pending.id, error: failure(-32002) });
      } else if (Object.hasOwn(message, 'error')) {
        post(doc, { type: 'response', id: pending.id, error: message.error });
      } else if (Object.hasOwn(message, 'result')) {
        post(doc, { type: 'response', id: pending.id, result: message.result });
      } else post(doc, { type: 'response', id: pending.id, error: failure(4900) });
    }
  }
  function navigation(event, committed = false) {
    for (const doc of documents.values()) {
      if (doc.tabId !== event.tabId) continue;
      const sameFrame = doc.frameId === event.frameId;
      if (!sameFrame && !(committed && event.frameId === 0)) continue;
      if (sameFrame) {
        if (!committed && typeof event.documentId === 'string' && event.documentId &&
            event.documentId !== doc.browserDocument) continue;
        if (event.documentId === doc.browserDocument && doc.url !== null && webOrigin(event.url) === doc.url &&
            event.documentLifecycle === 'active') continue;
      }
      close(doc);
    }
  }
  chrome.webNavigation.onCommitted.addListener(event => navigation(event, true));
  chrome.webNavigation.onHistoryStateUpdated.addListener(navigation);
  chrome.webNavigation.onReferenceFragmentUpdated.addListener(navigation);
  return {
    connect, receive,
    authenticated() { for (const doc of documents.values()) void register(doc); },
    retire() { for (const doc of documents.values()) { unregister(doc); purge(doc, 4900); } },
    lockState(locked, generationChanged = false) {
      for (const doc of documents.values()) {
        if (locked || generationChanged) {
          failPending(doc, locked ? 4100 : -32002, true);
          doc.generation = -1;
          post(doc, { type: 'state', accounts: [], chainId: null });
        }
      }
    },
  };
}
