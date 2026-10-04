const { createHash, randomUUID } = require('node:crypto');
const { mkdir, open, readFile, rename, stat, unlink, writeFile } = require('node:fs/promises');
const path = require('node:path');
const { deserialize, serialize } = require('node:v8');

/**
 * A small shared-filesystem Incremental Cache handler for the reference stack.
 * The mounted directory is the coordination point between Next.js origins.
 * Atomic renames keep readers from observing partial cache or tag files.
 */
module.exports = class SharedCacheHandler {
  constructor() {
    this.root = process.env.NEXT_SHARED_CACHE_DIR || path.join(process.cwd(), '.next/cache/tanod');
    this.entries = path.join(this.root, 'entries');
    this.tagsFile = path.join(this.root, 'tags.json');
    this.tagsLock = path.join(this.root, 'tags.lock');
  }

  resetRequestCache() {}

  fileFor(key) {
    return path.join(/* turbopackIgnore: true */ this.entries, createHash('sha256').update(key).digest('hex'));
  }

  async tags() {
    try {
      return JSON.parse(await readFile(/* turbopackIgnore: true */ this.tagsFile, 'utf8'));
    } catch (error) {
      if (error.code === 'ENOENT') return {};
      throw error;
    }
  }

  async get(key, context = {}) {
    let entry;
    try {
      entry = deserialize(await readFile(/* turbopackIgnore: true */ this.fileFor(key)));
    } catch (error) {
      if (error.code === 'ENOENT') return null;
      throw error;
    }
    const invalidated = await this.tags();
    const requestedTags = [...(context.tags || []), ...(context.softTags || [])];
    const storedTags = entry.tags || [];
    if ([...requestedTags, ...storedTags].some((tag) => (invalidated[tag] || 0) >= entry.lastModified)) {
      return null;
    }
    return { lastModified: entry.lastModified, value: entry.value };
  }

  async set(key, value, context = {}) {
    await mkdir(this.entries, { recursive: true });
    const file = this.fileFor(key);
    const temporary = `${file}.${process.pid}.${randomUUID()}.tmp`;
    const entry = {
      lastModified: Date.now(),
      value,
      tags: [...new Set([...(context.tags || []), ...(context.softTags || [])])],
    };
    await writeFile(temporary, serialize(entry), { flag: 'wx' });
    await rename(temporary, file);
  }

  async revalidateTag(tags) {
    const names = Array.isArray(tags) ? tags : [tags];
    if (names.length === 0) return;
    await mkdir(this.root, { recursive: true });
    let lock;
    for (let attempt = 0; attempt < 500; attempt += 1) {
      try {
        lock = await open(/* turbopackIgnore: true */ this.tagsLock, 'wx');
        break;
      } catch (error) {
        if (error.code !== 'EEXIST') throw error;
        const age = await stat(/* turbopackIgnore: true */ this.tagsLock)
          .then((value) => Date.now() - value.mtimeMs)
          .catch(() => 0);
        if (age > 30_000) {
          await unlink(/* turbopackIgnore: true */ this.tagsLock).catch(() => {});
          continue;
        }
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
    }
    if (!lock) throw new Error('timed out waiting for the shared cache tag lock');

    try {
      const current = await this.tags();
      const now = Date.now();
      for (const tag of names) current[tag] = now;
      const temporary = `${this.tagsFile}.${process.pid}.${randomUUID()}.tmp`;
      await writeFile(temporary, `${JSON.stringify(current)}\n`, { flag: 'wx' });
      await rename(temporary, this.tagsFile);
    } finally {
      await lock.close();
      await unlink(/* turbopackIgnore: true */ this.tagsLock).catch((error) => {
        if (error.code !== 'ENOENT') throw error;
      });
    }
  }
};
