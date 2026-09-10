// Shared host glue: endpoint preferences contain no authentication material.
export function configuration(value) {
  if (typeof value !== 'string' || value.length > 512) throw new Error('Invalid endpoint');
  value = value.trim();
  if (!value) return { value: '', endpoint: 'ws://127.0.0.1:43110/', permission: null };
  const manual = value.includes('://');
  const url = new URL(manual ? value : `ws://${value}:43110`);
  if (!['ws:', 'wss:'].includes(url.protocol) || !url.hostname || url.hostname.includes('*') || url.username || url.password ||
      url.pathname !== '/' || url.search || url.hash || !url.port) throw new Error('Invalid endpoint');
  if (!manual && (value.includes('/') || value.includes(':'))) throw new Error('Invalid host');
  const local = url.hostname === '127.0.0.1';
  return {
    value, endpoint: url.href,
    permission: local ? null : `${url.protocol === 'wss:' ? 'https' : 'http'}://${url.hostname}/*`,
  };
}
