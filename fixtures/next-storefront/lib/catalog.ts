// A CPU-bound stand-in for an expensive public page: build a synthetic catalog,
// facet it, rank it, then let React render the top of it.
//
// Unlike `delay()`, this costs CPU. The work is a fixed amount per request, not a
// fixed duration, so when renders overlap they share the process's CPU and each
// one takes longer, which is how real render work saturates. The work yields
// between chunks, as data fetching and streaming renders do, so several renders
// can be in progress at once and the fixture's `active` counter means something.

export type CatalogItem = {
  id: number;
  name: string;
  category: string;
  price: number;
  rating: number;
  score: number;
};

export type Catalog = {
  items: number;
  checksum: number;
  facets: [string, number][];
  priceBands: number[];
  top: CatalogItem[];
};

const CATEGORIES = ["trail", "road", "track", "court", "hike", "walk", "kids", "sale"];
const ADJECTIVES = ["Atlas", "Tempo", "Cloud", "Ridge", "Swift", "Nova", "Summit", "Drift"];

export function catalogUnits(fallback = 40_000): number {
  const parsed = Number(process.env.CATALOG_WORK_UNITS);
  return Number.isInteger(parsed) && parsed > 0 ? parsed : fallback;
}

function seed(text: string): number {
  let hash = 2166136261;
  for (let index = 0; index < text.length; index += 1) {
    hash = Math.imul(hash ^ text.charCodeAt(index), 16777619);
  }
  return hash >>> 0;
}

function random(state: { value: number }): number {
  // mulberry32: deterministic per slug, so every arm renders identical work.
  state.value = (state.value + 0x6d2b79f5) >>> 0;
  let t = state.value;
  t = Math.imul(t ^ (t >>> 15), t | 1);
  t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
}

function yieldToEventLoop(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

export async function buildCatalog(slug: string, units: number, chunks = 16): Promise<Catalog> {
  const state = { value: seed(slug) };
  const items: CatalogItem[] = [];
  const facetCounts = new Map<string, number>();
  const priceBands = new Array<number>(10).fill(0);
  let checksum = 0;
  const perChunk = Math.max(1, Math.ceil(units / chunks));

  for (let id = 0; id < units; id += 1) {
    const category = CATEGORIES[Math.floor(random(state) * CATEGORIES.length)];
    const price = Math.round((20 + random(state) * 280) * 100) / 100;
    const rating = Math.round(random(state) * 50) / 10;
    const name = `${ADJECTIVES[id % ADJECTIVES.length]} ${category} ${id.toString(36)}`;
    const score = rating * 20 - price / 10 + (name.length % 7);
    items.push({ id, name, category, price, rating, score });
    facetCounts.set(category, (facetCounts.get(category) ?? 0) + 1);
    priceBands[Math.min(9, Math.floor(price / 30))] += 1;
    checksum = (checksum + seed(name)) >>> 0;
    if ((id + 1) % perChunk === 0) {
      await yieldToEventLoop();
    }
  }

  items.sort((left, right) => right.score - left.score || left.id - right.id);
  return {
    items: units,
    checksum,
    facets: [...facetCounts.entries()].sort((left, right) => right[1] - left[1]),
    priceBands,
    top: items.slice(0, 48),
  };
}
