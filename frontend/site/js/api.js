const BASE = "/api/v1";

async function getJson(path) {
  const res = await fetch(BASE + path);
  if (res.status === 404) return null;
  if (!res.ok) throw new Error(`${path}: HTTP ${res.status}`);
  return res.json();
}

export const api = {
  stats: () => getJson("/stats"),
  blocks: (limit = 25) => getJson(`/blocks?limit=${limit}`),
  blockByHeight: (h) => getJson(`/block/height/${h}`),
  blockByHash: (h) => getJson(`/block/hash/${h}`),
  tx: (txid) => getJson(`/tx/${txid}`),
  address: (addr, page = 1, pageSize = 25) =>
    getJson(`/address/${addr}?page=${page}&page_size=${pageSize}`),
  addressUtxos: (addr) => getJson(`/address/${addr}/utxos`),
  gaps: () => getJson("/gaps"),
  richlist: () => getJson("/richlist"),
  mempool: () => getJson("/mempool"),
  halving: () => getJson("/halving"),
};
