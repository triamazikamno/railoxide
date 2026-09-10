(() => {
  const channel = 'railoxide-provider-v1';
  // MAIN scripts cannot import the worker module. Keep this policy aligned with gateway-page-bridge.js.
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
  const listeners = new Map();
  const pending = new Map();
  let sequence = 0;
  let accounts = [];
  let chainId = null;
  let connected = false;
  let configured = false;
  let bridgeReady = false;
  const emit = (name, value) => {
    for (const listener of [...(listeners.get(name) ?? [])]) {
      try { listener(value); } catch { /* Dapp listeners do not own delivery. */ }
    }
  };
  function rpcError(value) {
    // Keep every source-bearing error field, including arbitrary data and extensions.
    const error = new Error(value.message);
    Object.defineProperties(error, Object.fromEntries(Object.entries(value).map(([key, field]) =>
      [key, { value: field, writable: true, enumerable: true, configurable: true }])));
    return error;
  }
  const provider = {
    request(args) {
      if (!args || typeof args.method !== 'string') return Promise.reject(rpcError({ code: -32602, message: 'Invalid provider request.' }));
      if (!supported(args.method)) return Promise.reject(rpcError({ code: 4200, message: 'Unsupported provider method.' }));
      if (!bridgeReady) return Promise.reject(rpcError({ code: 4900, message: 'Desktop provider disconnected.' }));
      if (pending.size >= 128) return Promise.reject(rpcError({ code: -32005, message: 'Provider request capacity exceeded.' }));
      const id = String(++sequence);
      const traceConnection = args.method === 'eth_requestAccounts' || args.method === 'wallet_switchEthereumChain';
      if (traceConnection) console.log('[RailOxide gateway] gateway connection request started', args.method);
      return new Promise((resolve, reject) => {
        pending.set(id, { resolve, reject, method: args.method, traceConnection });
        try { window.postMessage({ channel, direction: 'request', id, method: args.method, params: args.params }, '*'); }
        catch {
          if (traceConnection) console.log('[RailOxide gateway] gateway connection request failed', args.method, -32602);
          pending.delete(id); reject(rpcError({ code: -32602, message: 'Invalid provider request.' }));
        }
      });
    },
    on(name, listener) {
      if (typeof listener !== 'function') throw new TypeError('Listener must be a function.');
      if (!listeners.has(name)) listeners.set(name, new Set());
      listeners.get(name).add(listener);
      return provider;
    },
    removeListener(name, listener) { listeners.get(name)?.delete(listener); return provider; },
    isConnected() { return connected; },
  };
  const info = Object.freeze({ uuid: crypto.randomUUID(), name: 'RailOxide', rdns: 'org.railoxide',
    // Existing wallet logo; attribution remains in bins/wallet/assets/icons/SOURCES.md.
    icon: 'data:image/svg+xml,%3Csvg%20xmlns%3D%22http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%22%20viewBox%3D%220%200%201024%201024%22%20width%3D%2264%22%20height%3D%2264%22%3E%3Cpolygon%20points%3D%22844%2C759%20597%2C892%20293%2C854%20464%2C683%22%20fill%3D%22%231F1812%22%2F%3E%3Cpolygon%20points%3D%22920%2C474%20844%2C759%20464%2C683%20692%2C550%22%20fill%3D%22%2344322A%22%2F%3E%3Cpolygon%20points%3D%22293%2C854%20103%2C607%20464%2C683%22%20fill%3D%22%235C4B40%22%2F%3E%3Cpolygon%20points%3D%22787%2C189%20920%2C474%20692%2C550%22%20fill%3D%22%239D6440%22%2F%3E%3Cpolygon%20points%3D%22445%2C436%20692%2C550%20464%2C683%22%20fill%3D%22%2374665C%22%2F%3E%3Cpolygon%20points%3D%22445%2C132%20787%2C189%20692%2C550%20445%2C436%22%20fill%3D%22%23A89888%22%2F%3E%3Cpolygon%20points%3D%22103%2C607%20164%2C284%20445%2C436%20464%2C683%22%20fill%3D%22%23948070%22%2F%3E%3Cpolygon%20points%3D%22164%2C284%20445%2C132%20445%2C436%22%20fill%3D%22%23C2B5A5%22%2F%3E%3C%2Fsvg%3E' });
  const detail = Object.freeze({ info, provider });
  const announce = () => window.dispatchEvent(new CustomEvent('eip6963:announceProvider', { detail }));
  function installPreferences(preferences) {
    if (configured) return;
    configured = true;
    if (preferences.metamask === true) Object.defineProperty(provider, 'isMetaMask', { value: true });
    if (preferences.takeover === true) {
      const descriptor = Object.getOwnPropertyDescriptor(window, 'ethereum');
      if (!descriptor || descriptor.configurable) {
        try { Object.defineProperty(window, 'ethereum', { value: provider, configurable: true, writable: true }); } catch { /* Reload may be needed for another wallet's property. */ }
      }
    }
    // Never replace another wallet's legacy object or provider by default.
    if (window.ethereum === provider || window.ethereum === undefined) {
      try {
        if (window.web3 === undefined) window.web3 = {};
        if (window.web3 && window.web3.currentProvider === undefined) {
          Object.defineProperty(window.web3, 'currentProvider', { configurable: true, get() {
            console.warn('window.web3.currentProvider is deprecated. Discover RailOxide with EIP-6963.');
            return provider;
          } });
        }
      } catch { /* Other providers may own frozen legacy objects. */ }
    }
    announce();
  }
  function transition(nextAccounts, nextChainId, error, reset = false) {
    const accountsChanged = JSON.stringify(accounts) !== JSON.stringify(nextAccounts);
    const chainChanged = nextChainId !== null && chainId !== nextChainId;
    const wasConnected = connected;
    accounts = nextAccounts;
    chainId = nextChainId;
    connected = chainId !== null;
    if (accountsChanged || reset) emit('accountsChanged', accounts.slice());
    if (chainChanged) emit('chainChanged', chainId);
    if (!wasConnected && connected) emit('connect', { chainId });
    if (wasConnected && !connected) emit('disconnect', rpcError(error ?? {
      code: 4900, message: 'Desktop provider disconnected.',
    }));
  }
  window.addEventListener('message', event => {
    const message = event.data;
    if (event.source !== window || message?.channel !== channel || message.direction !== 'provider') return;
    bridgeReady = true;
    if (message.type === 'preferences') { installPreferences(message); return; }
    if (message.type === 'response') {
      const request = pending.get(message.id);
      if (!request) return;
      pending.delete(message.id);
      if (request.traceConnection) {
        if (Object.hasOwn(message, 'error')) {
          console.log('[RailOxide gateway] gateway connection request failed', request.method,
            typeof message.error?.code === 'number' ? message.error.code : null);
        } else console.log('[RailOxide gateway] gateway connection request succeeded', request.method);
      }
      if (Object.hasOwn(message, 'error')) request.reject(rpcError(message.error));
      else request.resolve(message.result);
    } else if (message.type === 'reset') {
      for (const request of pending.values()) {
        if (request.traceConnection) console.log('[RailOxide gateway] gateway connection request reset', request.method,
          typeof message.error?.code === 'number' ? message.error.code : null);
        request.reject(rpcError(message.error));
      }
      pending.clear();
      transition([], null, message.error, true);
    } else if (message.type === 'state') {
      transition(message.accounts, message.chainId);
    }
  });
  window.addEventListener('eip6963:requestProvider', announce);
  announce();
  window.postMessage({ channel, direction: 'request', type: 'ready' }, '*');
})();
