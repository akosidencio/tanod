// An h2c origin for `bench/origin-concurrency.py --h2`.
//
// The same witness as the Python origin in that script, speaking prior-
// knowledge HTTP/2 instead of HTTP/1.1, so the vendored Pingora patch's h2
// upstream path is driven by the same scenarios as its h1 path. Node runs one
// thread, so every counter is exact: a render's count falls synchronously
// with its last write, before another stream's handler can run.
//
// Query parameters select the behaviour (see origin-concurrency.py):
// mode=lazy|coop|ignore|detached|hang|reset, chunks, step, before, tail, cc,
// size, at=head, hang, id, once. A reset is RST_STREAM(INTERNAL_ERROR).
//
// Usage: node bench/h2-origin.mjs  -> prints {"port": P, "control": C}
// The control port speaks HTTP/1.1: GET /__stats (counters and events),
// GET /__reset.
import http from 'node:http';
import http2 from 'node:http2';

const t0 = performance.now();
const CHUNK = Buffer.alloc(16384, 'x');
const state = {
  events: [], connections: 0, handlers: 0, renders: 0, peak_renders: 0, peak_handlers: 0,
  started: 0, completed: 0, cancelled: 0, number: 0,
};

function log(req, event, extra = {}) {
  state.events.push({ t_ms: Math.round((performance.now() - t0) * 100) / 100, req, event,
    renders: state.renders, ...extra });
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const seenOnce = new Set();

async function render(stream, rid, params) {
  let mode = params.get('mode') ?? 'lazy';
  // `once=<key>`: misbehave on the first request for the key only, so a
  // retry of it can succeed.
  const once = params.get('once');
  if (once) {
    if (seenOnce.has(once)) mode = 'lazy';
    seenOnce.add(once);
  }
  const chunks = Number(params.get('chunks') ?? 4);
  const step = Number(params.get('step') ?? 50);
  const before = Number(params.get('before') ?? 0);
  const tail = Number(params.get('tail') ?? 0);
  const cc = params.get('cc') ?? 'private, no-store';
  const size = Number(params.get('size') ?? 1);
  state.renders += 1;
  state.started += 1;
  state.peak_renders = Math.max(state.peak_renders, state.renders);
  log(rid, 'render_start', { mode });
  let outcome = 'completed';
  let closedHow = null;
  const closed = new Promise((resolve) => {
    const done = (how) => { closedHow ??= how; resolve(how); };
    stream.once('aborted', () => done('aborted'));
    stream.once('close', () => done('close'));
  });
  // Sleep; a cooperative origin wakes early when its stream closes.
  const pause = async (ms) => {
    if (mode !== 'coop') {
      await sleep(ms);
      return false;
    }
    const how = await Promise.race([closed, sleep(ms).then(() => null)]);
    if (how) {
      state.events.push({ t_ms: Math.round((performance.now() - t0) * 100) / 100,
        event: 'cancellation_seen', how });
      return true;
    }
    return false;
  };
  const gone = () => stream.destroyed || stream.closed || closedHow !== null;
  const write = async (data) => {
    if (gone()) {
      log(rid, 'write_failed', { error: closedHow ?? 'destroyed' });
      return false;
    }
    if (!stream.write(data)) {
      const how = await Promise.race([closed, new Promise((r) => stream.once('drain', () => r(null)))]);
      if (how) {
        log(rid, 'write_failed', { error: how });
        return false;
      }
    }
    return true;
  };
  // `destroy` with an error sends RST_STREAM(INTERNAL_ERROR) at once. Not
  // `close(code)`: once data was written, Node ends the stream (END_STREAM)
  // before its RST, which makes a truncated body look complete.
  const reset = () => {
    stream.destroy(new Error('origin reset'));
    log(rid, 'origin_reset');
  };
  try {
    if (before && await pause(before)) {
      outcome = 'cancelled';
      return;
    }
    if (mode === 'reset' && params.get('at') === 'head') {
      reset();
      return;
    }
    if (gone()) {
      log(rid, 'write_failed', { error: closedHow ?? 'destroyed' });
      outcome = 'cancelled';
      if (mode !== 'ignore') return;
    } else {
      stream.respond({ ':status': 200, 'content-type': 'text/html', 'cache-control': cc,
        'x-render': rid });
      log(rid, 'header_sent');
    }
    for (let i = 0; i < chunks; i += 1) {
      if (i || mode === 'hang') {
        const ms = mode === 'hang' ? Number(params.get('hang') ?? 2) * 1000 : step;
        if (await pause(ms)) {
          outcome = 'cancelled';
          return;
        }
      }
      // After the step, as in the Python origin.
      if (i && mode === 'reset') {
        reset();
        return;
      }
      const body = size === 1 ? CHUNK : Buffer.concat(Array(size).fill(CHUNK));
      if (!await write(body)) {
        outcome = 'cancelled';
        if (mode === 'lazy' || mode === 'coop') return;
      } else {
        log(rid, 'chunk_sent', { n: i });
      }
    }
    if (!gone()) stream.end();
    log(rid, 'response_end');
    if (mode === 'detached') {
      log(rid, 'socket_closed_by_origin');
      await sleep(tail);
    }
  } finally {
    if (outcome === 'cancelled') state.cancelled += 1;
    else state.completed += 1;
    state.renders -= 1;
    log(rid, 'render_end', { outcome });
  }
}

const server = http2.createServer({ settings: { maxConcurrentStreams: 1000 } });
server.on('session', (session) => session.on('error', () => {}));
server.on('stream', async (stream, headers) => {
  stream.on('error', () => {});
  const url = new URL(headers[':path'], 'http://origin');
  if (url.pathname === '/healthz') {
    stream.respond({ ':status': 200 });
    stream.end('ok');
    return;
  }
  // `connections` counts open streams: Tanod keeps its h2 connection pooled,
  // so an idle origin still has a socket, but never an unfinished stream.
  state.connections += 1;
  state.number += 1;
  const rid = url.searchParams.get('id') ?? String(state.number);
  state.handlers += 1;
  state.peak_handlers = Math.max(state.peak_handlers, state.handlers);
  log(rid, 'handler_start', { path: url.pathname });
  stream.once('close', () => { state.connections -= 1; });
  try {
    await render(stream, rid, url.searchParams);
  } finally {
    state.handlers -= 1;
    log(rid, 'handler_end');
  }
});

const control = http.createServer((req, res) => {
  if (req.url === '/__reset') {
    Object.assign(state, { events: [], peak_renders: state.renders, peak_handlers: state.handlers,
      started: 0, completed: 0, cancelled: 0 });
  }
  res.setHeader('content-type', 'application/json');
  res.end(JSON.stringify(state));
});

server.listen(0, '127.0.0.1', () => {
  control.listen(0, '127.0.0.1', () => {
    console.log(JSON.stringify({ port: server.address().port, control: control.address().port }));
  });
});
