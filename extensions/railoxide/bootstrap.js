// This module owns only startup, failure reporting and document lifecycle.
// Gateway presentation belongs to Rust/GPUI; the worker owns transport.
import { configuration } from './gateway-config.js';
// Match GPUI's web platform detection; the WASM target cannot identify the host OS.
const isMac = navigator.platform.includes('Mac') || navigator.userAgent.includes('Mac');
const STARTUP_TIMEOUT_MS = 30_000;
const REQUIRED_COMPONENT_ICONS = ['check', 'search', 'close', 'inbox', 'loader', 'loader-circle', 'settings', 'copy',
  'chevron-down', 'chevron-right', 'chevron-left', 'arrow-down', 'triangle-alert', 'circle-check', 'circle-x', 'globe', 'info'];
const startedAt = 0; // performance.timeOrigin is this document navigation start.
const shell = document.querySelector('#startup');
const message = document.querySelector('#startup-message');
const recovery = document.querySelector('#startup-recovery');
const reload = document.querySelector('#reload');
const controller = new AbortController();
let phase = 'local browser code';
let state = 'loading';
let runtime;
let runtimeInitialized = false;
let uiPort;
let stateCallback;
let snapshotCallback;
let connectSnapshot = { locked: true, accounts: [], pending_connects: [], pending_requests: [] };
const isSidePanel = new URL(location.href).search === '?mode=sidepanel';
function sendViewPresence() {
  if ((isSidePanel || isToolbarPopup) && state === 'ready' && uiPort) {
    uiPort.postMessage({ type: isSidePanel ? 'sidepanel_presence' : 'popup_presence', ready: true, visible: document.visibilityState === 'visible' });
  }
}
let openingSnapshotPending = false;
try {
  openingSnapshotPending = !isSidePanel && chrome.extension.getViews({ type: 'popup' }).includes(globalThis);
} catch { /* Missing popup identity must not produce activity. */ }
const isToolbarPopup = openingSnapshotPending;
let isBrowserTab = true;
try {
  isBrowserTab = chrome.extension.getViews({ type: 'tab' }).includes(globalThis);
} catch { /* Missing view identity must not close a possible browser tab. */ }
let popupOpening;
function clearPopupOpening() {
  openingSnapshotPending = false;
  popupOpening = null;
}
function deliverPopupOpening() {
  if (!popupOpening) return;
  if (popupOpening.port !== uiPort || connectSnapshot.locked !== false ||
      popupOpening.generation !== connectSnapshot.generation) {
    clearPopupOpening();
    return;
  }
  if (state !== 'ready') return;
  clearPopupOpening();
  sendActivity();
}
function deliverSnapshot(snapshot) {
  connectSnapshot = snapshot;
  const callback = snapshotCallback;
  queueMicrotask(() => {
    if (callback && callback === snapshotCallback && (state === 'loading' || state === 'ready')) callback(connectSnapshot);
  });
}
let gatewayStatus = 'disconnected';
let gatewayEndpoint = '';
let gatewayPaired = false;
// Mirrors the worker's reported manifest version; a worker that never reports one leaves it blank.
let extensionVersion = '';
let providerPreferences = { takeover: false, metamask: false };
let view = 'popup';
let viewError = false;
let viewNotice = '';
let viewChange;
let nextViewChange = 0;
let hostingWindowId;
let documentWindow;
async function cacheHostingWindow() {
  try {
    documentWindow = await chrome.windows.getCurrent({ populate: true });
    const host = documentWindow.type === 'normal' ? documentWindow
      : await chrome.windows.getLastFocused({ windowTypes: ['normal'] });
    if (host.type === 'normal' && Number.isInteger(host.id)) hostingWindowId = host.id;
  } catch { /* The mode remains usable through the toolbar if no browser window exists. */ }
}
function currentViewChange(change) {
  return viewChange === change && uiPort === change.port && state === 'ready';
}
function cancelViewChange() {
  if (!viewChange) return;
  clearTimeout(viewChange.timer);
  viewChange.saved(false);
  viewChange = null;
  viewNotice = 'View switch interrupted. Try again.';
}
async function closePreviousView(change) {
  if (!currentViewChange(change)) return;
  if (isToolbarPopup || (isSidePanel && !isBrowserTab)) {
    window.close();
  } else if (['?mode=window', '?mode=notification'].includes(new URL(location.href).search) &&
      documentWindow?.type === 'popup' && documentWindow.tabs?.length === 1) {
    const current = await chrome.windows.getCurrent({ populate: true });
    if (currentViewChange(change) && current.id === documentWindow.id && current.type === 'popup' &&
        current.tabs?.length === 1 && current.tabs[0].id === documentWindow.tabs[0].id) window.close();
  }
}
function switchView(value) {
  if (viewChange) return;
  viewNotice = '';
  const change = { id: ++nextViewChange, value, port: uiPort, handoff: isSidePanel && !isBrowserTab && value === 'popup' };
  viewChange = change;
  const saved = new Promise(resolve => { change.saved = resolve; });
  change.timer = setTimeout(() => {
    if (!currentViewChange(change)) return;
    cancelViewChange();
    deliverStatus(gatewayStatus);
  }, 10_000);
  let opened;
  try {
    change.port.postMessage({ type: 'preferences', key: 'view', value, request_id: change.id,
      ...(change.handoff && Number.isSafeInteger(hostingWindowId) && hostingWindowId > 0 ? { handoff_window_id: hostingWindowId } : {}) });
  } catch {
    cancelViewChange();
    deliverStatus(gatewayStatus);
    return;
  }
  try {
    // sidePanel.open must run directly in the click's user gesture, before any await.
    if (value === 'sidepanel' && Number.isInteger(hostingWindowId)) {
      opened = chrome.sidePanel.open({ windowId: hostingWindowId }).then(() => true, () => false);
    }
  } catch { opened = Promise.resolve(false); }
  void (async () => {
    const result = await saved;
    if (!currentViewChange(change)) return;
    if (!result?.success) {
      viewNotice = 'Could not save or apply toolbar mode. Try again.';
      return;
    }
    const manualOpen = async () => {
      viewNotice = `Mode saved. Click the toolbar icon to open ${value === 'popup' ? 'Popup' : 'Side panel'}. Close this view manually if it stays open.`;
      await closePreviousView(change);
    };
    if (change.handoff) {
      if (result.opened !== true) await manualOpen();
      return;
    }
    if (!Number.isInteger(hostingWindowId)) {
      await manualOpen();
      return;
    }
    if (value === 'popup') {
      if (typeof chrome.action.openPopup !== 'function') {
        await manualOpen();
        return;
      }
      opened = chrome.action.openPopup({ windowId: hostingWindowId }).then(() => true, () => false);
    }
    const didOpen = await opened;
    if (!currentViewChange(change)) return;
    if (!didOpen) {
      await manualOpen();
      return;
    }
    try { await closePreviousView(change); }
    catch { viewNotice = 'New view opened. Close this view manually.'; }
  })().catch(() => {
    if (currentViewChange(change)) viewNotice = 'View switch failed. Click the toolbar icon or try again.';
  }).finally(() => {
    if (!currentViewChange(change)) return;
    clearTimeout(change.timer);
    viewChange = null;
    deliverStatus(gatewayStatus);
  });
}
let portRetry;
let hostEpoch = 0;
function deliverStatus(next, endpoint = gatewayEndpoint) {
  gatewayEndpoint = endpoint;
  gatewayStatus = next;
  const callback = stateCallback;
  queueMicrotask(() => {
    if (callback && callback === stateCallback && (state === 'loading' || state === 'ready')) callback({ status: next, endpoint, paired: gatewayPaired,
      takeover: providerPreferences.takeover, metamask: providerPreferences.metamask, view,
      view_notice: viewNotice || (viewError ? 'Could not save or apply toolbar mode. Try again or reload the extension.' : ''),
      ...(extensionVersion ? { version: extensionVersion } : {}) });
  });
}
function attachWorker() {
  if (state !== 'loading' && state !== 'ready') return;
  const port = chrome.runtime.connect({ name: 'gateway-ui-v1' });
  uiPort = port;
  if (Number.isInteger(hostingWindowId)) port.postMessage({ type: 'tab_context', window_id: hostingWindowId });
  port.onMessage.addListener(message => {
    if (uiPort === port && message?.type === 'view_result' && viewChange?.port === port &&
        message.request_id === viewChange.id && typeof message.success === 'boolean') {
      viewChange.saved(message);
    }
    if (uiPort === port && message?.type === 'ui_snapshot') {
      if (openingSnapshotPending) {
        openingSnapshotPending = false;
        if (message.locked === false && Number.isSafeInteger(message.generation)) {
          popupOpening = { port, generation: message.generation };
        }
      }
      deliverSnapshot(message);
      deliverPopupOpening();
    }
    if (uiPort === port && message?.type === 'state' && typeof message.status === 'string') {
      providerPreferences = { takeover: message.takeover === true, metamask: message.metamask === true };
      gatewayPaired = message.paired === true;
      extensionVersion = typeof message.version === 'string' ? message.version : '';
      if (message.view === 'popup' || message.view === 'sidepanel') view = message.view;
      if (typeof message.viewError === 'boolean') viewError = message.viewError;
      if (message.status === 'disconnected') clearPopupOpening();
      deliverStatus(message.status, typeof message.endpoint === 'string' ? message.endpoint : gatewayEndpoint);
    }
  });
  port.onDisconnect.addListener(() => {
    if (uiPort !== port) return;
    cancelViewChange();
    uiPort = null;
    clearPopupOpening();
    deliverSnapshot({ locked: true, accounts: [], pending_connects: [], pending_requests: [] });
    deliverStatus('disconnected');
    portRetry = setTimeout(attachWorker, 1000);
  });
  sendViewPresence();
}
function detachWorker() {
  cancelViewChange();
  clearPopupOpening();
  hostEpoch += 1;
  stateCallback = null;
  snapshotCallback = null;
  connectSnapshot = { locked: true, accounts: [], pending_connects: [], pending_requests: [] };
  clearTimeout(portRetry);
  const port = uiPort;
  uiPort = null;
  port?.disconnect();
}

if (['window', 'notification', 'sidepanel'].includes(new URL(location.href).searchParams.get('mode'))) {
  document.documentElement.classList.add('window-mode');
}

function stopRuntime() {
  if (runtimeInitialized) runtime.stop();
}

function fail(detail) {
  if (state !== 'loading') return;
  state = 'failed';
  detachWorker();
  clearTimeout(deadline);
  controller.abort();
  document.documentElement.classList.add('failed');
  message.textContent = `Startup failed during ${phase}. ${detail}`;
  recovery.textContent = 'Reload this gateway view. If it fails again, rebuild with scripts/build-browser-extension, reload the extension in brave://extensions, and reopen it.';
  recovery.hidden = false;
  reload.hidden = false;
  // A Rust callback may still hold the App borrow. Release it on the next task.
  setTimeout(stopRuntime, 0);
}

const deadline = setTimeout(() => fail('The 30-second startup limit expired.'), STARTUP_TIMEOUT_MS);
// Recovery remains available after the startup fetches have been aborted.
reload.addEventListener('click', () => location.reload());

Object.defineProperty(globalThis, 'railoxideHost', { value: Object.freeze({
  isMac: () => isMac,
  isSidePanel: () => isSidePanel,
  isActive: () => state === 'loading' || state === 'ready',
  canCopyAddress(kind, generation, identity, address) {
    // The GPUI callback submits synchronously after this check. Browser writes cannot be cancelled after submission.
    if (state !== 'ready' || !uiPort || gatewayStatus !== 'unlocked' || connectSnapshot.locked ||
        connectSnapshot.generation !== generation) return false;
    if (kind === 'private') {
      const view = connectSnapshot.private_view;
      return connectSnapshot.private_view_supported === true && view?.selected_wallet === identity &&
        view.receive_address === address;
    }
    return kind === 'public' && connectSnapshot.public_view?.selected_account === identity &&
      connectSnapshot.accounts?.some(account => account.uuid === identity && account.address === address) === true;
  },
  canCopyNetwork(generation, revision, viewId, address) {
    return state === 'ready' && !!uiPort && gatewayStatus === 'unlocked' && !connectSnapshot.locked &&
      connectSnapshot.generation === generation && connectSnapshot.network_view?.context_revision === revision &&
      connectSnapshot.network_popover?.view_id === viewId && connectSnapshot.network_popover.results?.some(result =>
        result.operation === 'query_exit_ip' && result.outcome?.status === 'exit_ip' && result.outcome.ip === address) === true;
  },
  stage(next) {
    if (state !== 'loading') return;
    phase = next;
    message.textContent = `Loading ${phase}...`;
  },
  ready() {
    if (state !== 'loading') return;
    state = 'ready';
    clearTimeout(deadline);
    shell.hidden = true;
    sendViewPresence();
    deliverPopupOpening();
    performance.mark('railoxide-first-interactive');
    const firstInteractiveMs = performance.now() - startedAt;
    console.info('RailOxide gateway first interactive', { firstInteractiveMs });
  },
  fail,
  subscribe(callback) {
    stateCallback = callback;
    deliverStatus(gatewayStatus);
  },
  subscribeConnect(callback) {
    snapshotCallback = callback;
    deliverSnapshot(connectSnapshot);
  },
  resolveConnect(requestId, account, chainId) {
    if (state !== 'ready' || !uiPort) return;
    uiPort.postMessage({ type: 'resolve_connect', request_id: requestId, public_account_uuid: account ?? null, chain_id: chainId });
  },
  command(command, value) {
    if (state !== 'ready' || !uiPort) return;
    if (command === 'public_view' || command === 'private_view' || command === 'network') {
      try {
        uiPort.postMessage({ type: command, generation: connectSnapshot.generation,
          tab_token: connectSnapshot.current_tab_token, command: JSON.parse(value) });
      } catch { /* Only the packaged frontend supplies serialized UI commands. */ }
    } else if (command === 'home_tab') {
      uiPort.postMessage({ type: 'home_tab', generation: connectSnapshot.generation, value });
    } else if (command === 'request_wallet_switch' || command === 'summon_desktop') {
      uiPort.postMessage({ type: command, generation: connectSnapshot.generation,
        ...(command === 'request_wallet_switch' ? { request_id: value } : {}) });
    } else if (command === 'view' && ['popup', 'sidepanel'].includes(value)) {
      switchView(value);
    } else if (command === 'takeover' || command === 'metamask') {
      uiPort.postMessage({ type: 'preferences', key: command, value: value === 'true' });
    } else if (command === 'pair') {
      hostEpoch += 1;
      if (/^\d{6}$/.test(value)) uiPort.postMessage({ type: 'pair', code: value });
    } else if (command === 'unpair') {
      hostEpoch += 1;
      uiPort.postMessage({ type: 'unpair' });
    } else if (command === 'retry') {
      hostEpoch += 1;
      uiPort.postMessage({ type: 'retry' });
    } else if (command === 'configure') {
      let config;
      try { config = configuration(value); } catch { deliverStatus('config_failed'); return; }
      const expected = ++hostEpoch;
      // Invoke permission request directly under the GPUI click's user gesture.
      const permission = config.permission ? chrome.permissions.request({ origins: [config.permission] }) : Promise.resolve(true);
      void permission.then(granted => {
        if (state !== 'ready' || hostEpoch !== expected || !uiPort) return;
        if (granted) uiPort.postMessage({ type: 'configure', endpoint: config.value });
        else deliverStatus('config_failed');
      }).catch(() => { if (hostEpoch === expected) deliverStatus('config_failed'); });
    }
  },
  pair(code, endpoint) {
    if (state !== 'ready' || !uiPort || !/^\d{6}$/.test(code)) return;
    const expected = ++hostEpoch;
    let config;
    try { config = configuration(endpoint); } catch { deliverStatus('config_failed'); return; }
    // Invoke permission request directly under the GPUI click's user gesture.
    const permission = config.permission ? chrome.permissions.request({ origins: [config.permission] }) : Promise.resolve(true);
    void permission.then(granted => {
      if (state !== 'ready' || hostEpoch !== expected || !uiPort) return;
      if (granted) uiPort.postMessage({ type: 'pair', code, endpoint: config.value });
      else deliverStatus('config_failed');
    }).catch(() => { if (hostEpoch === expected) deliverStatus('config_failed'); });
  },
}) });

// Genuine document input and opening the toolbar popup may extend inactivity.
// Host commands, focus and transport reconnection never produce activity.
let lastActivity = -Infinity;
function sendActivity() {
  if (state !== 'ready' || !uiPort || connectSnapshot.locked ||
      !Number.isSafeInteger(connectSnapshot.generation)) return;
  const time = performance.now();
  if (time - lastActivity < 1000) return;
  lastActivity = time;
  uiPort.postMessage({ type: 'user_activity', generation: connectSnapshot.generation });
}
for (const type of ['pointerdown', 'keydown', 'wheel']) {
  document.addEventListener(type, event => {
    if (event.isTrusted) sendActivity();
  }, { capture: true, passive: true, signal: controller.signal });
}

document.addEventListener('visibilitychange', sendViewPresence, { signal: controller.signal });

window.addEventListener('pagehide', () => {
  state = 'closed';
  detachWorker();
  clearTimeout(deadline);
  controller.abort();
  stopRuntime();
}, { once: true });
window.addEventListener('error', () => fail('A browser runtime error occurred. See the extension console.'), { signal: controller.signal });
window.addEventListener('unhandledrejection', () => fail('Browser initialization rejected. See the extension console.'), { signal: controller.signal });

async function localBytes(path) {
  const response = await fetch(chrome.runtime.getURL(path), { signal: controller.signal });
  if (!response.ok) throw new Error(`Missing packaged resource: ${path}`);
  const bytes = new Uint8Array(await response.arrayBuffer());
  if (bytes.length === 0) throw new Error(`Empty packaged resource: ${path}`);
  return bytes;
}

async function start() {
  await cacheHostingWindow();
  runtime = await import('./browser_frontend.js');
  if (state !== 'loading') return;
  railoxideHost.stage('local WASM');
  await runtime.default({ module_or_path: await localBytes('browser_frontend_bg.wasm') });
  runtimeInitialized = true;
  if (state !== 'loading') return;
  railoxideHost.stage('packaged fonts and component icons');
  const [inter, mono, icons] = await Promise.all([
    localBytes('assets/fonts/InterVariable.ttf'),
    localBytes('assets/fonts/JetBrainsMono-Regular.ttf'),
    Promise.all(REQUIRED_COMPONENT_ICONS.map(async name => {
      const path = `icons/${name}.svg`;
      return [path, await localBytes(`assets/${path}`)];
    })).then(Object.fromEntries),
  ]);
  const walletAssets = JSON.parse(new TextDecoder().decode(await localBytes('assets/WALLET-ASSETS.json')));
  await Promise.all(walletAssets.map(async path => {
    icons[path] = await localBytes(`assets/${path}`);
  }));
  if (state !== 'loading') return;
  attachWorker();
  runtime.run(chrome.runtime.getURL('').replace(/\/$/, ''), inter, mono, icons);
}

start().catch(() => fail('Local initialization failed. Rebuild the extension and reload.'));
