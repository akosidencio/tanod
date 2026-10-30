#!/usr/bin/env python3
"""Deterministic origin-concurrency lifecycle scenarios.

Every scenario runs a real Tanod process against an origin that lives in this
process, on one asyncio loop, so its counters are exact rather than sampled:

* connections  — TCP connections the origin has open
* handlers     — requests whose headers were read and whose handler runs
* renders      — application work in progress, counted by an RAII scope that
                 begins before the response header and ends when the work
                 really stops, which may be after the socket has gone

Each render records timestamped events (accepted, render start, header, chunk,
write failure, cancellation seen, response end, socket closed, render end), so
a failed assertion names the request and lifecycle step that overlapped.

The origin behaviours are what the contract is stated against:

* ``lazy``     notices a disconnect only when its next write fails, the way a
               streaming SSR server does between suspense boundaries
* ``coop``     stops as soon as its connection reports EOF or an error
* ``ignore``   keeps rendering to the end whatever happens to the socket
* ``detached`` finishes and closes its response, then keeps working: the case
               no proxy can observe
* ``hang``     sends a header and then nothing until it is cancelled or its own
               timer ends, which exercises Tanod's origin deadline
* ``reset``    fails: resets its connection before the header (``at=head``) or
               after the first chunk, while the client is still connected

Usage: bench/origin-concurrency.py [--tanod PATH] [--policy clock|tinyufo]
       [--only NAME ...] [--json OUT]
       bench/origin-concurrency.py --stress [--seconds N] ...

``--stress`` replaces the deterministic scenarios with sustained concurrent
traffic against the same exact witness: uncached and cache-pressure
disconnects, a mixed workload, coalesced streams, and reloads, purges, origin
deadlines and a graceful shutdown under load. A render overlap is only
accepted when one of the overlapping renders is a request deliberately sent
past its origin deadline, which is the documented limitation.
"""
import argparse
import asyncio
import json
import os
import random
import re
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CHUNK = b'x' * 16384


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


class Origin:
    """The witness. Single-threaded, so every counter is exact."""

    def __init__(self, lean=False):
        self.t0 = time.monotonic()
        self.events = []
        # Under sustained load only the two events the overlap check needs.
        self.lean = lean
        self.connections = 0
        self.handlers = 0
        self.renders = 0
        self.peak_renders = 0
        self.peak_handlers = 0
        self.started = 0
        self.completed = 0
        self.cancelled = 0
        self.number = 0
        self.tasks = set()
        self.server = None
        self.port = None

    def log(self, request, event, **extra):
        if self.lean and event not in ('render_start', 'render_end'):
            return
        row = {'t_ms': round((time.monotonic() - self.t0) * 1000, 2), 'req': request,
               'event': event, 'renders': self.renders}
        row.update(extra)
        self.events.append(row)

    async def start(self):
        self.server = await asyncio.start_server(self.accept, '127.0.0.1', 0, backlog=512)
        self.port = self.server.sockets[0].getsockname()[1]

    async def stop(self):
        self.server.close()
        await self.server.wait_closed()

    async def idle(self, seconds=10):
        """Wait until no render, handler or connection is outstanding."""
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            if self.renders == 0 and self.handlers == 0 and self.connections == 0:
                return True
            await asyncio.sleep(.01)
        return False

    async def accept(self, reader, writer):
        task = asyncio.current_task()
        self.tasks.add(task)
        self.connections += 1
        try:
            while True:
                try:
                    head = await reader.readuntil(b'\r\n\r\n')
                except (asyncio.IncompleteReadError, ConnectionError, OSError):
                    return
                keep = await self.request(reader, writer, head.decode('latin-1'))
                if not keep:
                    return
        finally:
            self.connections -= 1
            try:
                writer.close()
            except (ConnectionError, OSError):
                pass
            self.tasks.discard(task)

    async def request(self, reader, writer, head):
        line = head.split('\r\n', 1)[0]
        path = line.split(' ')[1]
        name, _, query = path.partition('?')
        params = dict(p.split('=', 1) for p in query.split('&') if '=' in p)
        if name == '/healthz':
            writer.write(b'HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok')
            await writer.drain()
            return True
        self.number += 1
        rid = params.get('id', str(self.number))
        self.handlers += 1
        self.peak_handlers = max(self.peak_handlers, self.handlers)
        self.log(rid, 'handler_start', path=name)
        try:
            await self.render(rid, reader, writer, name, params)
        finally:
            self.handlers -= 1
            self.log(rid, 'handler_end')
        return False

    async def render(self, rid, reader, writer, path, params):
        mode = params.get('mode', 'lazy')
        chunks = int(params.get('chunks', 4))
        step = int(params.get('step', 50)) / 1000
        before = int(params.get('before', 0)) / 1000
        tail = int(params.get('tail', 0)) / 1000
        cache = params.get('cc', 'private, no-store')
        size = int(params.get('size', 1))
        # The render scope. Opened before anything is written and closed only
        # where the work genuinely stops, so the counter cannot drift.
        self.renders += 1
        self.started += 1
        self.peak_renders = max(self.peak_renders, self.renders)
        self.log(rid, 'render_start', mode=mode)
        outcome = 'completed'
        closed = asyncio.ensure_future(self.watch_close(reader))
        try:
            if before:
                if await self.pause(before, closed, mode):
                    outcome = 'cancelled'
                    return
            if mode == 'reset' and params.get('at') == 'head':
                self.reset(rid, writer)
                return
            head = (f'HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n'
                    f'Cache-Control: {cache}\r\nTransfer-Encoding: chunked\r\n'
                    f'X-Render: {rid}\r\nConnection: close\r\n\r\n').encode()
            if not await self.write(rid, writer, head, mode):
                outcome = 'cancelled'
                if mode not in ('ignore',):
                    return
            self.log(rid, 'header_sent')
            for i in range(chunks):
                if i and mode == 'reset':
                    self.reset(rid, writer)
                    return
                if i or mode == 'hang':
                    if await self.pause(step if mode != 'hang' else float(params.get('hang', 2)),
                                        closed, mode):
                        outcome = 'cancelled'
                        return
                body = CHUNK * size
                frame = b'%x\r\n' % len(body) + body + b'\r\n'
                if not await self.write(rid, writer, frame, mode):
                    outcome = 'cancelled'
                    if mode in ('lazy', 'coop'):
                        return
                else:
                    self.log(rid, 'chunk_sent', n=i)
            # The work is done once the last byte is produced. Writing the
            # terminator is transport, but it stays inside the scope: a proxy
            # cannot see the response end before this write is issued.
            await self.write(rid, writer, b'0\r\n\r\n', mode)
            self.log(rid, 'response_end')
            if mode == 'detached':
                # Close the exchange, then keep working where no proxy can see.
                writer.close()
                self.log(rid, 'socket_closed_by_origin')
                await asyncio.sleep(tail)
        finally:
            closed.cancel()
            if outcome == 'cancelled':
                self.cancelled += 1
            else:
                self.completed += 1
            self.renders -= 1
            self.log(rid, 'render_end', outcome=outcome)

    def reset(self, rid, writer):
        writer.get_extra_info('socket').setsockopt(
            socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii', 1, 0))
        writer.transport.abort()
        self.log(rid, 'origin_reset')

    async def watch_close(self, reader):
        try:
            data = await reader.read(1)
        except (ConnectionError, OSError):
            return 'error'
        return 'eof' if not data else 'data'

    async def pause(self, seconds, closed, mode):
        """Sleep; a cooperative origin wakes early when its connection closes."""
        if mode != 'coop':
            await asyncio.sleep(seconds)
            return False
        done, _ = await asyncio.wait({closed}, timeout=seconds)
        if done:
            self.events.append({'t_ms': round((time.monotonic() - self.t0) * 1000, 2),
                                'event': 'cancellation_seen', 'how': closed.result()})
            return True
        return False

    async def write(self, rid, writer, data, mode):
        try:
            writer.write(data)
            await writer.drain()
            return True
        except (ConnectionError, OSError, RuntimeError) as error:
            self.log(rid, 'write_failed', error=type(error).__name__)
            return False


class Tanod:
    def __init__(self, binary, folder, origin_port, policy, limit, extra=''):
        self.folder = Path(folder)
        self.listen, self.metrics, self.admin = free_port(), free_port(), free_port()
        config = self.folder / 'tanod.yaml'
        config.write_text(f'''version: 1
server:
  listen: "127.0.0.1:{self.listen}"
  threads: 2
  graceful:
    drain_period: 0s
    shutdown_timeout: 2s
    pid_file: "{folder}/tanod.pid"
    upgrade_socket: "{folder}/upgrade.sock"
origin:
  upstreams: ["127.0.0.1:{origin_port}"]
  concurrency:
    max: {limit}
    queue: {{max: 256, timeout: 20s}}
timeouts:
  origin: 3s
  first_byte: 3s
  idle: 3s
telemetry:
  admin: {{listen: "127.0.0.1:{self.admin}"}}
  prometheus: {{listen: "127.0.0.1:{self.metrics}"}}
{extra}''')
        self.log = (self.folder / 'tanod.log').open('wb')
        self.process = subprocess.Popen([str(binary), 'run', '--config', str(config)],
                                        stdout=self.log, stderr=self.log,
                                        env={**os.environ, 'RUST_LOG': 'info'})

    async def ready(self):
        for _ in range(400):
            try:
                await self.metric('tanod_origin_in_flight')
                status = await http_status(self.admin, '/health/ready')
                if status == 200:
                    return
            except OSError:
                pass
            await asyncio.sleep(.025)
        raise AssertionError('tanod did not become ready')

    async def scrape(self):
        _, body = await http_get(self.metrics, '/metrics')
        return body.decode()

    async def metric(self, name, labels='limiter="global"'):
        text = await self.scrape()
        pattern = '^' + re.escape(name) + (r'\{' + re.escape(labels) + r'\}' if labels else '') + r' (\S+)$'
        match = re.search(pattern, text, re.MULTILINE)
        return float(match[1]) if match else None

    async def stop(self):
        self.process.send_signal(signal.SIGTERM)
        try:
            await asyncio.to_thread(self.process.wait, 10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            await asyncio.to_thread(self.process.wait, 5)
        self.log.close()
        return self.process.returncode


async def http_get(port, path, timeout=10):
    reader, writer = await asyncio.wait_for(asyncio.open_connection('127.0.0.1', port), timeout)
    try:
        writer.write(f'GET {path} HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n'.encode())
        await writer.drain()
        data = await asyncio.wait_for(reader.read(), timeout)
    finally:
        writer.close()
    head, _, body = data.partition(b'\r\n\r\n')
    status = int(head.split(b' ', 2)[1]) if head else 0
    if b'transfer-encoding: chunked' in head.lower():
        body = dechunk(body)
    return status, body


async def http_status(port, path):
    status, _ = await http_get(port, path, timeout=2)
    return status


def dechunk(data):
    out = bytearray()
    while data:
        size, _, rest = data.partition(b'\r\n')
        try:
            n = int(size.split(b';')[0], 16)
        except ValueError:
            raise AssertionError('malformed chunked body') from None
        if n == 0:
            return bytes(out)
        out += rest[:n]
        data = rest[n + 2:]
    raise AssertionError('chunked body ended without its terminator')


async def fetch(port, path):
    """A complete request; returns (status, body bytes, complete?)."""
    try:
        status, body = await http_get(port, path, timeout=20)
        return status, len(body), True
    except AssertionError:
        return 0, 0, False


async def disconnect_after(port, path, wait_for, rst=True, seconds=None):
    """Start a request and hang up once `wait_for` bytes (or `seconds`) arrived."""
    reader, writer = await asyncio.open_connection('127.0.0.1', port)
    writer.write(f'GET {path} HTTP/1.1\r\nHost: fixture\r\n\r\n'.encode())
    await writer.drain()
    received = b''
    try:
        if seconds is not None:
            end = time.monotonic() + seconds
            while time.monotonic() < end:
                try:
                    part = await asyncio.wait_for(reader.read(4096), max(end - time.monotonic(), .001))
                except asyncio.TimeoutError:
                    break
                if not part:
                    break
                received += part
        else:
            while wait_for not in received:
                part = await asyncio.wait_for(reader.read(4096), 10)
                if not part:
                    break
                received += part
    finally:
        if rst:
            writer.get_extra_info('socket').setsockopt(
                socket.SOL_SOCKET, socket.SO_LINGER, struct.pack('ii', 1, 0))
            writer.transport.abort()
        else:
            writer.close()
    return received


ROUTES = '''routes:
  - id: private
    match: "/private/**"
    class: private_dynamic
  - id: public
    match: "/public/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl: {max: 60s}
'''


def cache(policy, memory='8MiB', body='4MiB'):
    return f'cache: {{enabled: true, eviction: {policy}, max_memory: {memory}, max_body_size: {body}}}\n'


class Scenario:
    def __init__(self, name, limit, run, extra=None, timeout=60):
        self.name, self.limit, self.run, self.extra, self.timeout = name, limit, run, extra, timeout


async def released(tanod, seconds=10):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        value = await tanod.metric('tanod_origin_in_flight')
        if value == 0:
            return True
        await asyncio.sleep(.005)
    return False


async def drains(t, o, outcome):
    """Drains with this outcome, read once the origin and Tanod have settled."""
    await o.idle()
    await released(t)
    return await t.metric('tanod_origin_disconnect_drains_total', f'outcome="{outcome}"') or 0


async def expect_drains(t, o, facts, **expected):
    for outcome, want in expected.items():
        got = await drains(t, o, outcome)
        ok = got >= 1 if want == 'some' else got == want
        check(facts, ok, f'disconnect drains with outcome {outcome}: expected {want}', got=got)


def t_limit(facts):
    return facts['limit']


def check(facts, ok, claim, **detail):
    facts.setdefault('checks', []).append({'claim': claim, 'ok': bool(ok), **detail})


# ------------------------------------------------------------------ scenarios


async def s_normal(t, o, facts):
    results = await asyncio.gather(*(fetch(t.listen, f'/private/a?id=n{i}&mode=lazy&chunks=3&step=60')
                                     for i in range(3)))
    check(facts, all(r[0] == 200 and r[2] for r in results), 'every request served completely',
          results=results)


async def s_contended(t, o, facts):
    """Many more clients than permits, no disconnects: the limiter alone.

    This witness cannot see a request between the final write of one render
    and the decrement of its count, because both happen on one event loop
    with no await between them. An overlap here would be over-admission.
    """
    async def client(c):
        for n in range(25):
            result = await fetch(t.listen, f'/private/k?id=k{c}-{n}&mode=lazy&chunks=1&before=25')
            if result[0] != 200:
                return result
        return None
    failures = [r for r in await asyncio.gather(*(client(c) for c in range(32))) if r]
    check(facts, not failures, 'every contended request served', failures=failures[:3])
    # A ceiling never reached proves nothing about the ceiling.
    check(facts, o.peak_renders == t_limit(facts), 'the contended workload reached the ceiling',
          peak=o.peak_renders)


async def s_disconnect(t, o, facts, path, before=False):
    """Hang up on an admitted request, then queue a second one behind it."""
    first = asyncio.ensure_future(disconnect_after(
        t.listen, f'{path}?id=first&mode=lazy&chunks=6&step=80' + ('&before=200' if before else ''),
        b'X-Render' if not before else b'', seconds=.1 if before else None))
    while not any(e['event'] == 'render_start' and e['req'] == 'first' for e in o.events):
        await asyncio.sleep(.002)
    await first
    facts['disconnected_at_ms'] = round((time.monotonic() - o.t0) * 1000, 2)
    second = await fetch(t.listen, f'{path}?id=second&mode=lazy&chunks=1')
    check(facts, second[0] == 200 and second[2], 'the queued request is served', result=second)
    await o.idle()
    first_end = next(e['t_ms'] for e in o.events if e['event'] == 'render_end' and e['req'] == 'first')
    second_start = next(e['t_ms'] for e in o.events if e['event'] == 'render_start' and e['req'] == 'second')
    check(facts, second_start >= first_end, 'the queued render began only after the abandoned one ended',
          first_end=first_end, second_start=second_start)


async def s_disconnect_private(t, o, facts):
    await s_disconnect(t, o, facts, '/private/b')
    await expect_drains(t, o, facts, completed='some', deadline=0, upstream_error=0)


async def s_disconnect_private_before_headers(t, o, facts):
    await s_disconnect(t, o, facts, '/private/b', before=True)
    await expect_drains(t, o, facts, completed='some', deadline=0, upstream_error=0)


async def s_disconnect_pressure(t, o, facts):
    await s_disconnect(t, o, facts, '/public/pressure')
    pressure = await t.metric('tanod_cache_memory_pressure_total', 'outcome="uncached_fallback"')
    check(facts, (pressure or 0) >= 1, 'the response took the uncached cache-pressure fallback',
          uncached_fallback=pressure)
    await expect_drains(t, o, facts, completed='some', deadline=0, upstream_error=0)


async def s_disconnect_cached(t, o, facts):
    # Pingora itself keeps a cache fill going after its client leaves; the
    # drain must not take that path over.
    await s_disconnect(t, o, facts, '/public/cached')
    await expect_drains(t, o, facts, completed=0, deadline=0, upstream_error=0)


async def s_cooperative(t, o, facts):
    first = asyncio.ensure_future(disconnect_after(
        t.listen, '/private/c?id=first&mode=coop&chunks=6&step=80', b'X-Render'))
    await first
    second = await fetch(t.listen, '/private/c?id=second&mode=coop&chunks=1')
    check(facts, second[0] == 200, 'the queued request is served', result=second)
    # Tanod holds the exchange open, so a cooperative origin is never told to
    # stop: it finishes, and the permit comes back when it has.
    await expect_drains(t, o, facts, completed='some', deadline=0, upstream_error=0)


async def s_detached(t, o, facts):
    """Work that outlives its own response. Documented as unenforceable."""
    first = await fetch(t.listen, '/private/d?id=first&mode=detached&chunks=1&tail=300')
    second = await fetch(t.listen, '/private/d?id=second&mode=lazy&chunks=1')
    check(facts, first[0] == 200 and second[0] == 200, 'both requests served', results=[first, second])
    await expect_drains(t, o, facts, completed=0, deadline=0, upstream_error=0)
    facts['expect_overlap'] = True


async def s_hang(t, o, facts):
    """An origin that stops talking. Tanod's origin deadline ends the exchange."""
    started = time.monotonic()
    first = asyncio.ensure_future(disconnect_after(
        t.listen, '/private/h?id=first&mode=hang&chunks=1&hang=5', b'X-Render'))
    await first
    second = await fetch(t.listen, '/private/h?id=second&mode=lazy&chunks=1')
    facts['second_waited_ms'] = round((time.monotonic() - started) * 1000)
    check(facts, second[0] == 200, 'the queued request is served after the deadline', result=second)
    # The permit was held to the deadline (timeouts.origin = 3s), and the
    # release was recorded as unconfirmed rather than as a recovery.
    check(facts, facts['second_waited_ms'] >= 2500, 'the permit was held until the drain deadline',
          waited_ms=facts['second_waited_ms'])
    await expect_drains(t, o, facts, completed=0, deadline=1, upstream_error=0)
    facts['expect_overlap'] = True


async def s_origin_fails(t, o, facts):
    """The origin fails while its client is still connected.

    That is an origin error, never a client disconnect: the client must see
    it (a 502 before the header, a truncated body after it), the backend must
    be charged with the failure that prevented a header, and no drain may be
    recorded. A failure after a good header counts as the success it began as
    (`record_origin_outcome`), so only the first is charged.
    """
    async def failures():
        text = await t.scrape()
        return sum(float(m[1]) for m in re.finditer(
            r'^tanod_upstream_failures_total\{[^}]*kind="proxy"[^}]*\} (\S+)$', text, re.MULTILINE))
    before = await failures()
    head = await fetch(t.listen, '/private/r?id=head&mode=reset&at=head')
    check(facts, head[0] == 502, 'an origin reset before the header is a 502 to the waiting client',
          result=head)
    charged = await failures() - before
    check(facts, charged == 1, 'the failure was charged to the backend', recorded=charged)
    body = await fetch(t.listen, '/private/r?id=body&mode=reset&chunks=3')
    check(facts, not body[2], 'an origin reset mid-body is a visible truncation', result=body)
    await expect_drains(t, o, facts, completed=0, deadline=0, upstream_error=0)


async def s_storm(t, o, facts):
    """Admitted requests hang up together while new ones keep arriving."""
    for round_ in range(4):
        cut = [asyncio.ensure_future(disconnect_after(
            t.listen, f'/private/s?id=cut{round_}-{i}&mode=lazy&chunks=5&step=40',
            b'', rst=bool(i % 2), seconds=.03 + .01 * (i % 5))) for i in range(12)]
        fresh = [asyncio.ensure_future(fetch(
            t.listen, f'/private/s?id=new{round_}-{i}&mode=lazy&chunks=2&step=20'))
            for i in range(12)]
        await asyncio.gather(*cut)
        results = await asyncio.gather(*fresh)
        check(facts, all(r[0] == 200 and r[2] for r in results),
              f'round {round_}: every fresh request served completely')
    await expect_drains(t, o, facts, completed='some', deadline=0, upstream_error=0)


async def s_coalesce(t, o, facts, cut):
    path = f'/public/g-{cut}?id=g-{cut}&mode=lazy&chunks=6&step=60&cc=public,max-age=60'
    readers = []
    for i in range(4):
        if (cut == 'leader' and i == 0) or (cut == 'follower' and i == 2) or cut == 'all':
            readers.append(asyncio.ensure_future(disconnect_after(t.listen, path, b'X-Render')))
        else:
            readers.append(asyncio.ensure_future(fetch(t.listen, path)))
        await asyncio.sleep(.02)
    results = await asyncio.gather(*readers)
    complete = [r for r in results if isinstance(r, tuple)]
    check(facts, all(r[0] == 200 and r[1] == 6 * len(CHUNK) for r in complete),
          'every remaining reader received the whole body', results=complete)
    await o.idle()
    renders = sum(1 for e in o.events if e['event'] == 'render_start')
    check(facts, renders == 1, 'one origin render for the coalesced group', renders=renders)
    after = await fetch(t.listen, path)
    renders_after = sum(1 for e in o.events if e['event'] == 'render_start')
    check(facts, after[0] == 200 and after[1] == 6 * len(CHUNK) and renders_after == 1,
          'the completed fill is reused rather than rendered again', renders=renders_after)
    await expect_drains(t, o, facts, completed=0, deadline=0, upstream_error=0)


def scenarios(policy):
    private = ROUTES + cache(policy)
    pressure = ROUTES + cache(policy, memory='64B', body='64B')
    rows = [
        Scenario('A-normal-completion', 1, s_normal, private),
        Scenario('A-contended-no-disconnects', 8, s_contended, private, timeout=120),
        Scenario('B-disconnect-private-after-headers', 1, s_disconnect_private, private),
        Scenario('B-disconnect-private-before-headers', 1, s_disconnect_private_before_headers, private),
        Scenario('C-disconnect-cache-pressure-fallback', 1, s_disconnect_pressure, pressure),
        Scenario('C-disconnect-cached-fill', 1, s_disconnect_cached, private),
        Scenario('D-cooperative-origin', 1, s_cooperative, private),
        Scenario('E-non-cooperative-detached-work', 1, s_detached, private),
        Scenario('E-origin-stops-responding', 1, s_hang, private),
        Scenario('E-origin-fails-client-connected', 1, s_origin_fails, private),
        Scenario('F-disconnect-storm', 4, s_storm, private),
        Scenario('G-coalesced-follower-disconnects', 1, lambda t, o, f: s_coalesce(t, o, f, 'follower'), private),
        Scenario('G-coalesced-leader-disconnects', 1, lambda t, o, f: s_coalesce(t, o, f, 'leader'), private),
        Scenario('G-coalesced-all-disconnect', 1, lambda t, o, f: s_coalesce(t, o, f, 'all'), private),
    ]
    return rows


# --------------------------------------------------------------------- stress


def cpu_seconds(pid):
    """User plus system CPU time a process has used."""
    try:
        with open(f'/proc/{pid}/stat') as stat:
            fields = stat.read().rsplit(')', 1)[1].split()
        return (int(fields[11]) + int(fields[12])) / os.sysconf('SC_CLK_TCK')
    except (OSError, IndexError, ValueError):
        return 0.0


def proc_stats(pid):
    """RSS in KiB and open descriptors of a process, from /proc."""
    try:
        with open(f'/proc/{pid}/status') as status:
            rss = next(int(line.split()[1]) for line in status if line.startswith('VmRSS:'))
        return rss, len(os.listdir(f'/proc/{pid}/fd'))
    except (OSError, StopIteration):
        return 0, 0


async def stress(t, o, facts, mix, workers, seconds, background=None):
    end = time.monotonic() + seconds
    counts = Counter()
    peaks = {'permits': 0, 'rss_kb': 0, 'fds': 0}
    facts['fds_at_start'] = proc_stats(t.process.pid)[1]
    cpu_before = cpu_seconds(t.process.pid)

    async def sampler():
        while time.monotonic() < end:
            try:
                peaks['permits'] = max(peaks['permits'], await t.metric('tanod_origin_in_flight') or 0)
            except OSError:
                pass
            rss, fds = proc_stats(t.process.pid)
            peaks['rss_kb'] = max(peaks['rss_kb'], rss)
            peaks['fds'] = max(peaks['fds'], fds)
            await asyncio.sleep(.05)

    async def worker(w):
        rnd = random.Random(w)
        n = 0
        while time.monotonic() < end:
            n += 1
            kind, path, cut = mix(rnd, w, n)
            try:
                if cut is not None:
                    await disconnect_after(t.listen, path, b'', rst=rnd.random() < .5, seconds=cut)
                    counts[f'{kind}:disconnected'] += 1
                else:
                    status, _, complete = await fetch(t.listen, path)
                    outcome = 'ok' if status == 200 and complete else (
                        'shed' if status == 503 else f'unexpected-{status}-{complete}')
                    # Past its origin deadline, the response is aborted after
                    # its header: visibly incomplete, never presented as whole.
                    if kind == 'deadline' and not complete:
                        outcome = 'aborted-at-deadline'
                    counts[f'{kind}:{outcome}'] += 1
            except (OSError, asyncio.TimeoutError, asyncio.IncompleteReadError, IndexError, ValueError) as error:
                counts[f'{kind}:error-{type(error).__name__}'] += 1

    jobs = [asyncio.ensure_future(worker(w)) for w in range(workers)]
    jobs.append(asyncio.ensure_future(sampler()))
    if background:
        jobs.append(asyncio.ensure_future(background(end)))
    await asyncio.gather(*jobs)
    facts['counts'] = dict(counts)
    facts['requests'] = sum(counts.values())
    facts['tanod_cpu_seconds'] = round(cpu_seconds(t.process.pid) - cpu_before, 2)
    facts['tanod_cpu_us_per_request'] = round(facts['tanod_cpu_seconds'] * 1e6 / max(facts['requests'], 1), 1)
    facts['tanod_permit_peak_sampled'] = peaks['permits']
    facts['tanod_rss_peak_kb'] = peaks['rss_kb']
    facts['tanod_fds_peak'] = peaks['fds']
    unexpected = {k: v for k, v in counts.items() if 'unexpected' in k or 'error' in k}
    check(facts, not unexpected, 'no unexpected client outcome', unexpected=unexpected)
    check(facts, facts['requests'] > 100, 'enough traffic to mean something', requests=facts['requests'])
    check(facts, peaks['permits'] <= facts['limit'], 'sampled permit peak within the limit',
          peak=peaks['permits'])


def lazy(rid, chunks=3, step=15, extra=''):
    return f'id={rid}&mode=lazy&chunks={chunks}&step={step}{extra}'


def mix_uncached(rnd, w, n):
    rid = f'u{w}-{n}'
    if rnd.random() < .5:
        return 'private', f'/private/s?{lazy(rid)}', None
    return 'private-cut', f'/private/s?{lazy(rid, 4)}', rnd.uniform(.005, .05)


def mix_pressure(rnd, w, n):
    rid = f'p{w}-{n}'
    if rnd.random() < .5:
        return 'pressure', f'/public/p{rid}?{lazy(rid)}', None
    return 'pressure-cut', f'/public/p{rid}?{lazy(rid, 4)}', rnd.uniform(.005, .05)


def mix_mixed(rnd, w, n):
    rid = f'm{w}-{n}'
    r = rnd.random()
    if r < .25:
        return 'hot', f'/public/hot-{n % 4}?{lazy("hot", 2)}', None
    if r < .45:
        return 'fill', f'/public/u{rid}?{lazy(rid, 3, 15, "&size=4")}', None
    if r < .65:
        return 'private', f'/private/m?{lazy(rid)}', None
    if r < .80:
        return 'private-cut', f'/private/m?{lazy(rid, 4)}', rnd.uniform(.005, .05)
    if r < .90:
        return 'fill-cut', f'/public/c{rid}?{lazy(rid, 4, 15, "&size=4")}', rnd.uniform(.005, .05)
    return 'coop-cut', f'/private/m?id={rid}&mode=coop&chunks=4&step=15', rnd.uniform(.005, .05)


def mix_coalesce(rnd, w, n):
    # A small hot set with a one-second TTL keeps fills, followers and expiry
    # all happening at once; every third reader hangs up mid-stream.
    key = f'/stream/s-{n % 3}?id=s-{n % 3}&mode=lazy&chunks=5&step=30'
    if rnd.random() < .33:
        return 'stream-cut', key, rnd.uniform(.01, .12)
    return 'stream', key, None


def mix_operations(rnd, w, n):
    rid = f'o{w}-{n}'
    r = rnd.random()
    if r < .04:
        # Deliberately past timeouts.origin (3s): the documented case.
        return 'deadline', f'/private/o?id=dl-{rid}&mode=hang&chunks=1&hang=4', None
    if r < .40:
        return 'tagged', f'/public/t-{n % 16}?{lazy(rid, 2)}', None
    if r < .70:
        return 'private', f'/private/o?{lazy(rid)}', None
    return 'private-cut', f'/private/o?{lazy(rid, 4)}', rnd.uniform(.005, .05)


STREAM_ROUTE = '''  - id: stream
    match: "/stream/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl: {max: 1s}
'''


def stress_scenarios(policy, seconds):
    private = ROUTES + cache(policy)
    pressure = ROUTES + cache(policy, memory='64B', body='64B')
    mixed = ROUTES + cache(policy, memory='2MiB', body='1MiB')
    coalesce = ROUTES + STREAM_ROUTE + cache(policy)
    operations = ROUTES + f'''cache:
  enabled: true
  eviction: {policy}
  max_memory: 8MiB
  max_body_size: 4MiB
  tag_header: "x-render"
  purge: {{token: "stress-purge-token-0123456789abcdef"}}
'''

    async def s_uncached(t, o, f):
        await stress(t, o, f, mix_uncached, 32, seconds)
        await expect_drains(t, o, f, completed='some', deadline=0)

    async def s_pressure(t, o, f):
        await stress(t, o, f, mix_pressure, 32, seconds)
        await expect_drains(t, o, f, completed='some', deadline=0)

    async def s_mixed(t, o, f):
        await stress(t, o, f, mix_mixed, 48, seconds)
        await expect_drains(t, o, f, completed='some', deadline=0)

    async def s_coalesce(t, o, f):
        await stress(t, o, f, mix_coalesce, 48, seconds)
        renders = sum(1 for e in o.events if e['event'] == 'render_start')
        # Three keys, a one-second TTL: roughly three fills a second, not one
        # per reader. A collapse failure would render once per request.
        f['renders'] = renders
        check(f, renders < f['requests'] / 4, 'coalescing held under disconnects',
              renders=renders, requests=f['requests'])

    async def s_operations(t, o, f):
        folder = t.folder / 'tanod.yaml'
        original = folder.read_text()

        async def churn(end):
            flip = False
            while time.monotonic() < end:
                await asyncio.sleep(1)
                # Alternate a cache-sharing policy change (retires the old
                # namespace and its in-progress fills) with a plain reload.
                flip = not flip
                folder.write_text(original.replace('ttl: {max: 60s}', 'ttl: {max: 30s}') if flip else original)
                t.process.send_signal(signal.SIGHUP)
                reader, writer = await asyncio.open_connection('127.0.0.1', t.admin)
                writer.write(b'POST /purge?tag=t-1 HTTP/1.1\r\nHost: admin\r\n'
                             b'Authorization: Bearer stress-purge-token-0123456789abcdef\r\nContent-Length: 0\r\n'
                             b'Connection: close\r\n\r\n')
                await writer.drain()
                await reader.read()
                writer.close()
                f['reloads'] = f.get('reloads', 0) + 1

        await stress(t, o, f, mix_operations, 32, seconds, churn)
        f['allow_overlap_prefix'] = 'dl-'
        # Shut down while traffic and drains are in flight. Exit status and
        # the witness settling are checked by the common teardown.
        tail = [asyncio.ensure_future(fetch(t.listen, f'/private/o?{lazy(f"stop-{i}", 6, 60)}'))
                for i in range(12)]
        await asyncio.sleep(.1)
        t.process.send_signal(signal.SIGTERM)
        results = await asyncio.gather(*tail, return_exceptions=True)
        f['shutdown_tail'] = Counter(type(r).__name__ if isinstance(r, Exception) else r[0]
                                     for r in results)
        f['stopped_by_scenario'] = True

    lean = dict(timeout=seconds + 120)
    return [
        Scenario('S-A-uncached-disconnects', 8, s_uncached, private, **lean),
        Scenario('S-B-cache-pressure-disconnects', 8, s_pressure, pressure, **lean),
        Scenario('S-C-mixed-ssr', 8, s_mixed, mixed, **lean),
        Scenario('S-D-coalescing-streams', 4, s_coalesce, coalesce, **lean),
        Scenario('S-E-reload-purge-deadline-shutdown', 8, s_operations, operations, **lean),
    ]


def peak_excluding(events, prefix):
    active, peak = set(), 0
    for e in events:
        if e['req'].startswith(prefix):
            continue
        if e['event'] == 'render_start':
            active.add(e['req'])
            peak = max(peak, len(active))
        elif e['event'] == 'render_end':
            active.discard(e['req'])
    return peak


def overlaps(events, limit):
    """Every moment render concurrency exceeded the limit, with who was active."""
    active, found = {}, []
    for e in events:
        if e['event'] == 'render_start':
            active[e['req']] = e['t_ms']
            if len(active) > limit:
                found.append({'t_ms': e['t_ms'], 'active': dict(active)})
        elif e['event'] == 'render_end':
            active.pop(e['req'], None)
    return found


async def run_one(binary, policy, scenario, keep, lean=False):
    origin = Origin(lean)
    await origin.start()
    folder = tempfile.mkdtemp(prefix='tanod-oc-', dir=os.environ.get('TMPDIR', '/tmp'))
    tanod = Tanod(binary, folder, origin.port, policy, scenario.limit, scenario.extra)
    facts = {'scenario': scenario.name, 'policy': policy, 'limit': scenario.limit}
    try:
        await tanod.ready()
        await asyncio.wait_for(scenario.run(tanod, origin, facts), scenario.timeout)
        quiet = await origin.idle(15)
        check(facts, quiet, 'origin witness settles: no render, handler or connection outstanding',
              renders=origin.renders, handlers=origin.handlers, connections=origin.connections)
        if not facts.get('stopped_by_scenario'):
            check(facts, await released(tanod), 'every permit returned (tanod_origin_in_flight == 0)')
            check(facts, await tanod.metric('tanod_origin_disconnect_drains_active', '') == 0,
                  'no drain left active')
            _, status = await http_get(tanod.admin, '/status')
            pinned = json.loads(status)['cache'].get('pinned_bytes')
            check(facts, pinned == 0, 'no cache bytes left pinned', pinned=pinned)
            if 'fds_at_start' in facts:
                facts['fds_at_end'] = proc_stats(tanod.process.pid)[1]
                check(facts, facts['fds_at_end'] <= facts['fds_at_start'] + 4,
                      'descriptors back to their starting level',
                      start=facts['fds_at_start'], end=facts['fds_at_end'])
        found = overlaps(origin.events, scenario.limit)
        allowed = facts.get('allow_overlap_prefix')
        if allowed:
            attributed = [x for x in found if any(r.startswith(allowed) for r in x['active'])]
            facts['deadline_attributed_overlaps'] = len(attributed)
            found = [x for x in found if x not in attributed]
            facts['origin_peak_renders_all'] = origin.peak_renders
            facts['origin_peak_excluding_deadline'] = peak_excluding(origin.events, allowed)
        facts['origin_peak_renders'] = origin.peak_renders
        facts['origin_peak_handlers'] = origin.peak_handlers
        facts['renders_started'] = origin.started
        facts['renders_completed'] = origin.completed
        facts['renders_cancelled'] = origin.cancelled
        facts['overlaps'] = found[:5]
        if facts.get('expect_overlap'):
            # Not enforceable by a proxy; the check is that the overlap is the
            # documented one, and that Tanod held its permit for the exchange.
            facts['documented_limitation'] = bool(found)
        else:
            check(facts, not found and (allowed or origin.peak_renders <= scenario.limit),
                  f'origin render peak {origin.peak_renders} within limit {scenario.limit}'
                  + (' outside deadline-attributed overlaps' if allowed else ''),
                  overlaps=found[:3])
        if not facts.get('stopped_by_scenario'):
            text = await tanod.scrape()
            facts['tanod_metrics'] = {line.split(' ')[0]: line.split(' ')[1] for line in text.splitlines()
                                      if line.startswith(('tanod_origin_disconnect_drains', 'tanod_origin_in_flight',
                                                          'tanod_cache_memory_pressure_total'))}
    except Exception as error:  # noqa: BLE001 - recorded as a failed check
        check(facts, False, f'scenario raised {type(error).__name__}: {error}')
    finally:
        code = await tanod.stop()
        check(facts, code == 0, 'tanod exited cleanly', code=code)
        for task in list(origin.tasks):
            task.cancel()
        await origin.stop()
        facts['events'] = origin.events if not lean else []
        if not keep:
            for item in Path(folder).iterdir():
                if item.name != 'tanod.log':
                    item.unlink(missing_ok=True)
        facts['folder'] = folder
    facts['ok'] = all(c['ok'] for c in facts['checks'])
    return facts


async def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--tanod', type=Path, default=ROOT / 'target/release/tanod')
    parser.add_argument('--policy', action='append', choices=['clock', 'tinyufo'])
    parser.add_argument('--only', action='append', default=[])
    parser.add_argument('--json', type=Path)
    parser.add_argument('--keep', action='store_true')
    parser.add_argument('--stress', action='store_true')
    parser.add_argument('--seconds', type=int, default=60)
    args = parser.parse_args()
    rows = []
    for policy in args.policy or ['clock', 'tinyufo']:
        chosen = stress_scenarios(policy, args.seconds) if args.stress else scenarios(policy)
        for scenario in chosen:
            if args.only and not any(o in scenario.name for o in args.only):
                continue
            facts = await run_one(args.tanod, policy, scenario, args.keep, lean=args.stress)
            rows.append(facts)
            mark = 'PASS' if facts['ok'] else 'FAIL'
            extra = ' (documented limitation reproduced)' if facts.get('documented_limitation') else ''
            peak = facts.get('origin_peak_excluding_deadline', facts.get('origin_peak_renders'))
            print(f"{mark} {policy:7} {scenario.name:42} peak={peak}"
                  f"/{scenario.limit}{extra}"
                  + (f" requests={facts.get('requests')} permits<={facts.get('tanod_permit_peak_sampled')}"
                     f" rss<={facts.get('tanod_rss_peak_kb')}KiB fds<={facts.get('tanod_fds_peak')}"
                     if args.stress else ''), flush=True)
            for c in facts['checks']:
                if not c['ok']:
                    print(f"       failed: {c['claim']} {json.dumps({k: v for k, v in c.items() if k not in ('claim', 'ok')})[:600]}",
                          flush=True)
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(rows, indent=1) + '\n')
    failed = [r for r in rows if not r['ok']]
    print(f'{len(rows) - len(failed)}/{len(rows)} scenarios passed')
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(asyncio.run(main()))
