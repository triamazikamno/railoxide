// Stateless validation and presentation of gateway traffic for the service worker.
// Keep it import-free: tests load it from a data: URL, which cannot resolve relative imports.
const encoder = new TextEncoder();
const jsonBytes = value => encoder.encode(JSON.stringify(value)).length;
export const hasExtraKeys = (value, keys) => Object.keys(value).some(key => !keys.includes(key));

const unlockPhases = new Set(['password', 'busy', 'opening', 'passphrase', 'unknown', 'desktop', 'complete', 'failed', 'unavailable', 'cancelled', 'rate_limited']);
export const unlockTerminal = phase => ['desktop', 'complete', 'failed', 'unavailable', 'cancelled', 'rate_limited'].includes(phase);
export const invalidUnlockView = next => !next || typeof next.allowed !== 'boolean' ||
  !unlockPhases.has(next.phase) || (next.attempt_id !== null && (typeof next.attempt_id !== 'string' || next.attempt_id.length > 128));
// Secrets are bounded by their UTF-8 size.
const boundedSecret = value => typeof value === 'string' && value.length && encoder.encode(value).length <= 4096;
export const acceptsPassword = (input, phase) =>
  ['password', 'failed', 'unavailable', 'cancelled', 'rate_limited'].includes(phase) && boundedSecret(input.password);
// The follow-up an owned attempt may send in its current phase, or null.
export function unlockFollowUp(input, phase) {
  if (input.action === 'passphrase' && phase === 'passphrase' && boundedSecret(input.passphrase)) return { action: 'passphrase', passphrase: input.passphrase };
  if (input.action === 'standard' && phase === 'passphrase') return { action: 'standard' };
  if (input.action === 'retry' && phase === 'unknown') return { action: 'retry' };
  if (input.action === 'desktop' && ['passphrase', 'unknown'].includes(phase)) return { action: 'desktop' };
  return null;
}

const networkOperations = ['new_tor_session', 'query_exit_ip', 'quit_and_reset'];
const networkErrors = new Set([
  'Network control is unavailable. Reopen the popover and try again.',
  'Could not start a new Tor session. Try again.',
  'Started a new Tor session, but the broadcaster network could not restart. Try again later.',
  'Could not query the exit IP through Tor. Try again.',
  'Could not request the Tor state reset. The wallet remains open.',
]);
export const boundedNetworkText = (value, limit) => typeof value === 'string' && value.length > 0 && value.length <= limit;
const networkInteger = value => Number.isSafeInteger(value) && value >= 0;
const boundedNativeQuote = quote => !!quote && typeof quote.microUsd === 'string' &&
  /^[1-9][0-9]{0,77}$/.test(quote.microUsd) && BigInt(quote.microUsd) <= (1n << 256n) - 1n &&
  networkInteger(quote.obtainedAt) && Number.isInteger(quote.feedDecimals) &&
  quote.feedDecimals >= 0 && quote.feedDecimals <= 255;
export function nativeUsdPresentation(message) {
  const statuses = message.native_usd_pricing ?? {};
  if (!statuses || typeof statuses !== 'object' || Array.isArray(statuses) || Object.keys(statuses).length > 68) return null;
  if (message.locked && Object.keys(statuses).length) return null;
  for (const [chain, status] of Object.entries(statuses)) {
    if (!/^[1-9][0-9]{0,19}$/.test(chain) || BigInt(chain) > 18446744073709551615n ||
        !status || !['unconfigured', 'pending', 'available', 'failed'].includes(status.state) ||
        (status.reason !== null && !boundedNetworkText(status.reason, 512))) return null;
    const quote = status.quote;
    if (quote !== null && !boundedNativeQuote(quote)) return null;
    if ((status.state === 'available' && quote === null) || (status.state === 'unconfigured' && quote !== null)) return null;
  }
  return statuses;
}

function networkPresentation(message) {
  const value = message.network_view;
  if (message.locked || message.network_control_supported !== true || !value ||
      !boundedNetworkText(value.context_revision, 128) ||
      !['tor_ready', 'tor_reconnecting', 'tor_degraded', 'proxy', 'direct'].includes(value.status) ||
      typeof value.detail !== 'string' || value.detail.length > 512 || typeof value.runtime_warning !== 'boolean' ||
      (value.download_rate != null && !networkInteger(value.download_rate)) ||
      (value.direct_rpc_endpoints != null && !networkInteger(value.direct_rpc_endpoints))) return null;
  const rotation = value.rotation ?? null;
  if (rotation !== null && !(rotation.state === 'in_progress' ||
      (rotation.state === 'failed' && networkErrors.has(rotation.error)))) return null;
  let activity = null;
  if (value.activity != null) {
    if (!value.status.startsWith('tor_')) return null;
    const source = value.activity;
    activity = {};
    for (const name of ['generation', 'session_duration_ms', 'downloaded_bytes', 'recent_connection_sample_count',
      'recent_successful_sample_count', 'successful_connections', 'failed_connections']) {
      if (!networkInteger(source[name])) return null;
      activity[name] = source[name];
    }
    for (const name of ['median_setup_duration_ms', 'last_activity_age_ms']) {
      if (source[name] != null && !networkInteger(source[name])) return null;
      activity[name] = source[name] ?? null;
    }
  }
  return { context_revision: value.context_revision, status: value.status, detail: value.detail,
    runtime_warning: value.runtime_warning, activity, download_rate: value.download_rate ?? null,
    direct_rpc_endpoints: value.direct_rpc_endpoints ?? 0,
    rotation: rotation && (rotation.state === 'failed' ? { state: 'failed', error: rotation.error } : { state: 'in_progress' }) };
}
export const invalidNetworkRun = (input, status) => input.action !== 'run' || !networkOperations.includes(input.operation) ||
  !status.startsWith('tor_') || !boundedNetworkText(input.request_id, 128) ||
  hasExtraKeys(input, ['action', 'context_revision', 'view_id', 'request_id', 'operation']);
const invalidNetworkOutcome = (outcome, operation) => !outcome || !['done', 'exit_ip', 'failed'].includes(outcome.status) ||
  (outcome.status === 'failed' && !networkErrors.has(outcome.error)) ||
  (outcome.status === 'exit_ip' && (operation !== 'query_exit_ip' || !boundedNetworkText(outcome.ip, 45)));
// The outcome of a query reply as views receive it, or null when it is malformed.
export function networkOutcome(result) {
  const outcome = result.outcome;
  if (invalidNetworkOutcome(outcome, result.operation)) return null;
  return { status: outcome.status, ...(outcome.status === 'exit_ip' ? { ip: outcome.ip } : {}),
    ...(outcome.status === 'failed' ? { error: outcome.error } : {}) };
}

export const invalidEditorCommand = input => !input || !boundedNetworkText(input.editor_token, 128);
export const invalidChainEditorRun = input => !boundedNetworkText(input.revision, 128) || !input.command ||
  jsonBytes(input.command) > 65_536 ||
  hasExtraKeys(input, ['action', 'editor_token', 'request_id', 'revision', 'command']);
// A test result carries one quote or one reason, never saved pricing or endpoints.
const invalidProbe = probe => !probe || (probe.message !== null && !boundedNetworkText(probe.message, 4096)) ||
  (probe.quote !== null && !boundedNativeQuote(probe.quote));
const invalidEditorSnapshot = snapshot => !snapshot || !boundedNetworkText(snapshot.revision, 128) ||
  !Array.isArray(snapshot.chains) || snapshot.chains.length > 68;
export const invalidChainEditorOutcome = outcome => !outcome || !['ready', 'failed', 'probed'].includes(outcome.status) ||
  jsonBytes(outcome) > 131_072 ||
  (outcome.status === 'failed' && !boundedNetworkText(outcome.message, 4096)) ||
  (outcome.status === 'probed' && invalidProbe(outcome.probe)) ||
  (outcome.status === 'ready' && invalidEditorSnapshot(outcome.snapshot));

// A locked snapshot carrying wallet-derived data is rejected, not redacted.
const lockedSnapshotExposes = message => message.accounts.length ||
  message.pending_connects.some(prompt => prompt.accounts?.length) ||
  message.public_view?.selected_account || message.public_view?.selected_chain || message.public_view?.balances?.length ||
  message.public_view?.drafts?.length || message.permissions?.length || message.private_view != null ||
  message.network_view != null || message.network_results?.length;
const publicViewPresentation = (publicView, privateActions) => publicView && { ...publicView,
  ...(Array.isArray(publicView.drafts) ? { drafts: publicView.drafts.filter(draft =>
    privateActions || !['private_send', 'unshield'].includes(draft.input?.kind)) } : {}) };
const pendingRequestPresentation = (request, locked) => ({
  request_id: request.request_id, url: request.url,
  needs_unlock: locked || request.needs_unlock,
  summary: locked || request.needs_unlock ? null : request.summary,
});
// Fields a locked snapshot replaces with empty values.
const walletPresentation = (message, publicView) => ({
  private_view: !message.locked && message.private_view_supported === true ? message.private_view : null,
  public_view: message.locked ? null : publicView,
  permissions: message.locked ? [] : (message.permissions ?? []),
  ui_error: message.locked ? null : message.ui_error,
});
export function uiSnapshotPresentation(message, nativeUsd) {
  if (!Array.isArray(message.accounts) || !Array.isArray(message.pending_connects) || !Array.isArray(message.pending_requests)) return null;
  if (message.locked && lockedSnapshotExposes(message)) return null;
  const privateActions = !message.locked && message.private_actions_supported === true;
  const publicView = publicViewPresentation(message.public_view, privateActions);
  const network = networkPresentation(message);
  return { ...message, native_usd_pricing: nativeUsd, network_control_supported: network !== null, network_view: network, private_actions_supported: privateActions,
    private_self_broadcast_supported: privateActions && message.private_self_broadcast_supported === true,
    ...walletPresentation(message, publicView),
    pending_requests: message.pending_requests.map(request => pendingRequestPresentation(request, message.locked)) };
}
export const railgunUnavailable = snapshot =>
  snapshot.chains?.find(chain => chain.id === snapshot.public_view?.selected_chain)?.railgun === false;
// A private picker stays open only while its draft revision is still editable.
export const pickerDraftOpen = (snapshot, picker) => snapshot.public_view?.drafts?.some(draft => draft.draft_id === picker.draft_id &&
  draft.revision === picker.revision && !['attention', 'in_progress', 'done', 'failed'].includes(draft.status));
export const invalidStateMessage = message => message.type !== 'state' || typeof message.locked !== 'boolean' ||
  !Number.isSafeInteger(message.generation) || message.generation < 0;

export function isCredential(record) {
  const bytes = (value, size) => Array.isArray(value) && value.length === size &&
    value.every(byte => Number.isInteger(byte) && byte >= 0 && byte <= 255);
  return record && record.version === 1 && bytes(record.peerId, 16) && bytes(record.secret, 32);
}
