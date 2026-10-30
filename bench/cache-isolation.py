#!/usr/bin/env python3
"""Cross-user response isolation, through a real Tanod process.

An origin in this process renders a body naming the identity it saw (the
`session` cookie, an `Authorization` value, or `anon`) and the variant it
rendered, so a response delivered to the wrong user is unambiguous: its body
names somebody else. Every check compares the bytes each client received with
the identity that client sent.

Routes cover each sharing contract Tanod offers:

  /u/**      no class: inferred per request (cookies -> private)
  /pub/**    class public_ssr, no override
  /priv/**   class private_dynamic
  /co/**     class public_ssr, coalesce.override_origin
  /cache/**  class public_ssr, cache.override_origin (ttl.max 60s)

and each origin answer that decides shareability (`/<route>/<answer>/<page>`):

  pmax       Cache-Control: private, max-age=60
  nsmax      Cache-Control: no-store, max-age=60
  nextdyn    what Next sends on a dynamic route (private ... max-age=0)
  public     Cache-Control: public, s-maxage=60
  setcookie  public, plus Set-Cookie
  varycookie public, plus Vary: Cookie
  varyx      public, plus Vary on a header the key does not carry
  stream     nextdyn, body streamed in four chunks

Sharing between two users is a failure unless the route's declared contract
permits it: `cache.override_origin` permits storing and `coalesce.override_origin`
permits one flight, on a route the operator declared public. Even there,
Set-Cookie, unsupported Vary, Authorization, draft mode, Server Actions and the
RSC/HTML variant split must isolate. Permitted sharing is still counted, so a
run that never exercised the shared path cannot pass vacuously.

Usage: bench/cache-isolation.py [--tanod PATH] [--policy clock|tinyufo]
       [--json OUT] [--next-origin HOST:PORT]

`--next-origin` additionally runs the personalised `/account` page of the
Next.js fixture (fixtures/next-storefront) behind Tanod, as concurrent and
sequential pairs of different sessions, plain and RSC.
"""
import argparse
import asyncio
import json
import os
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.parse
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RENDER_MS = 400

ANSWERS = {
    'pmax': ('private, max-age=60', ''),
    'nsmax': ('no-store, max-age=60', ''),
    'nextdyn': ('private, no-cache, no-store, max-age=0, must-revalidate', ''),
    'public': ('public, s-maxage=60', ''),
    'setcookie': ('public, s-maxage=60', 'SETCOOKIE'),
    'varycookie': ('public, s-maxage=60', 'Vary: Cookie\r\n'),
    'varyx': ('public, s-maxage=60', 'Vary: X-Unkeyed\r\n'),
    'stream': ('private, no-cache, no-store, max-age=0, must-revalidate', ''),
}
# Answers that a declared-public override route may share at all.
OVERRIDABLE = {'pmax', 'nsmax', 'nextdyn', 'public', 'stream'}


def free_port():
    with socket.socket() as sock:
        sock.bind(('127.0.0.1', 0))
        return sock.getsockname()[1]


def header(head, name):
    for line in head.split('\r\n')[1:]:
        key, _, value = line.partition(':')
        if key.strip().lower() == name:
            yield value.strip()


class Origin:
    """Renders a body that names who asked for it."""

    def __init__(self):
        self.renders = {}
        self.server = None
        self.port = None

    async def start(self):
        self.server = await asyncio.start_server(self.accept, '127.0.0.1', 0, backlog=512)
        self.port = self.server.sockets[0].getsockname()[1]

    async def accept(self, reader, writer):
        try:
            head = (await reader.readuntil(b'\r\n\r\n')).decode('latin-1')
        except (asyncio.IncompleteReadError, ConnectionError, OSError):
            writer.close()
            return
        try:
            await self.respond(reader, writer, head)
        except (ConnectionError, OSError):
            pass
        finally:
            try:
                writer.close()
            except (ConnectionError, OSError):
                pass

    async def respond(self, reader, writer, head):
        method, path = head.split(' ', 2)[:2]
        if path == '/healthz':
            writer.write(b'HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok')
            await writer.drain()
            return
        length = next((int(v) for v in header(head, 'content-length')), 0)
        if length:
            await reader.readexactly(length)
        name = path.split('?', 1)[0]
        parts = name.strip('/').split('/')
        answer = parts[1] if len(parts) > 1 else 'nextdyn'
        cache_control, extra = ANSWERS.get(answer, ANSWERS['nextdyn'])
        cookies = ';'.join(header(head, 'cookie'))
        session = re.search(r'(?:^|;)\s*session=([^;]*)', cookies)
        auth = next(header(head, 'authorization'), None)
        identity = session[1] if session else (f'auth:{auth}' if auth else 'anon')
        variant = 'rsc' if any(True for _ in header(head, 'rsc')) else 'html'
        n = self.renders[name] = self.renders.get(name, 0) + 1
        if extra == 'SETCOOKIE':
            extra = f'Set-Cookie: session=minted-{n}; Path=/\r\n'
        query = urllib.parse.parse_qs(path.partition('?')[2])
        searched = ''.join(query.get('q', []))
        body = f'identity={identity};variant={variant};method={method};render={n};q={searched};'
        await asyncio.sleep(RENDER_MS / 1000)
        if answer == 'stream':
            writer.write((f'HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n'
                          f'Cache-Control: {cache_control}\r\n{extra}'
                          f'Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n').encode())
            for i in range(4):
                piece = f'{body}chunk={i};'.encode()
                writer.write(b'%x\r\n' % len(piece) + piece + b'\r\n')
                await writer.drain()
                await asyncio.sleep(.08)
            writer.write(b'0\r\n\r\n')
        else:
            data = body.encode()
            writer.write((f'HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n'
                          f'Cache-Control: {cache_control}\r\n{extra}'
                          f'Content-Length: {len(data)}\r\nConnection: close\r\n\r\n').encode()
                         + data)
        await writer.drain()


ROUTES = '''routes:
  - id: unclassified
    match: "/u/**"
  - id: public
    match: "/pub/**"
    class: public_ssr
  - id: private
    match: "/priv/**"
    class: private_dynamic
  - id: coalesce
    match: "/co/**"
    class: public_ssr
    coalesce: {override_origin: true}
  - id: cached
    match: "/cache/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl: {max: 60s}
  - id: search
    match: "/search/**"
    class: public_ssr
    cache:
      override_origin: true
      ttl: {max: 60s}
      query: {mode: include, keys: [q]}
  - id: assets
    match: "/_next/static/**"
'''


class Tanod:
    def __init__(self, binary, folder, upstream, policy, routes=ROUTES):
        self.binary = binary
        self.folder = Path(folder)
        self.listen, self.admin = free_port(), free_port()
        self.config = self.folder / 'tanod.yaml'
        self.upstream, self.policy = upstream, policy
        self.write_config(routes)
        self.log = (self.folder / 'tanod.log').open('ab')
        self.process = subprocess.Popen([str(binary), 'run', '--config', str(self.config)],
                                        stdout=self.log, stderr=self.log,
                                        env={**os.environ, 'RUST_LOG': 'info'})

    def write_config(self, routes):
        self.config.write_text(f'''version: 1
server:
  listen: "127.0.0.1:{self.listen}"
  threads: 2
  graceful:
    drain_period: 0s
    shutdown_timeout: 2s
    pid_file: "{self.folder}/tanod.pid"
    upgrade_socket: "{self.folder}/upgrade.sock"
origin:
  upstreams: ["{self.upstream}"]
  concurrency:
    max: 64
    queue: {{max: 256, timeout: 20s}}
cache: {{enabled: true, eviction: {self.policy}, max_memory: 16MiB}}
coalesce: {{enabled: true}}
timeouts:
  origin: 15s
  first_byte: 15s
  idle: 15s
debug_headers: true
telemetry:
  admin: {{listen: "127.0.0.1:{self.admin}"}}
{routes}''')

    async def ready(self):
        for _ in range(400):
            try:
                status, _, _ = await request(self.admin, 'GET', '/health/ready', {}, timeout=2)
                if status == 200:
                    return
            except (OSError, asyncio.TimeoutError, ValueError, IndexError):
                pass
            await asyncio.sleep(.025)
        raise AssertionError('tanod did not become ready')

    async def reload(self, routes, generation_before):
        self.write_config(routes)
        self.process.send_signal(signal.SIGHUP)
        for _ in range(200):
            _, _, body = await request(self.admin, 'GET', '/status', {}, timeout=2)
            match = re.search(rb'"generation":\s*(\d+)', body)
            if match and int(match[1]) > generation_before:
                return int(match[1])
            await asyncio.sleep(.025)
        raise AssertionError('reload was not applied')

    async def generation(self):
        _, _, body = await request(self.admin, 'GET', '/status', {}, timeout=2)
        return int(re.search(rb'"generation":\s*(\d+)', body)[1])

    async def stop(self):
        self.process.send_signal(signal.SIGTERM)
        try:
            await asyncio.to_thread(self.process.wait, 10)
        except subprocess.TimeoutExpired:
            self.process.kill()
            await asyncio.to_thread(self.process.wait, 5)
        self.log.close()


def dechunk(data):
    out = bytearray()
    while data:
        size, _, rest = data.partition(b'\r\n')
        n = int(size.split(b';')[0], 16)
        if n == 0:
            return bytes(out)
        out += rest[:n]
        data = rest[n + 2:]
    raise AssertionError('chunked body ended without its terminator')


async def request(port, method, path, headers, timeout=30, host='fixture', abort_after=None):
    reader, writer = await asyncio.wait_for(asyncio.open_connection('127.0.0.1', port), timeout)
    lines = [f'{method} {path} HTTP/1.1', f'Host: {host}', 'Connection: close']
    lines += [f'{k}: {v}' for k, v in headers.items()]
    if method == 'POST':
        lines.append('Content-Length: 0')
    try:
        writer.write(('\r\n'.join(lines) + '\r\n\r\n').encode())
        await writer.drain()
        if abort_after is not None:
            received = b''
            while abort_after not in received:
                part = await asyncio.wait_for(reader.read(4096), timeout)
                if not part:
                    break
                received += part
            return 0, '', received
        data = await asyncio.wait_for(reader.read(), timeout)
    finally:
        writer.close()
    head, _, body = data.partition(b'\r\n\r\n')
    text = head.decode('latin-1')
    status = int(text.split(' ', 2)[1]) if text else 0
    if 'transfer-encoding: chunked' in text.lower():
        body = dechunk(body)
    return status, text, body


def identity(body):
    match = re.search(rb'identity=([^;]*);variant=([^;]*)', body)
    return (match[1].decode(), match[2].decode()) if match else (None, None)


class Results:
    def __init__(self):
        self.rows = []

    def check(self, name, sent, got, may_share, shared_with=None):
        """`sent` is (identity, variant) a client asked for; `got` its body."""
        received = identity(got)
        own = received == sent
        shared = not own and shared_with is not None and received == shared_with
        ok = own or (shared and may_share)
        self.rows.append({'check': name, 'sent': list(sent), 'received': list(received),
                          'outcome': 'own' if own else ('shared' if shared else 'wrong'),
                          'may_share': may_share, 'pass': ok})
        mark = 'ok  ' if ok else 'FAIL'
        note = 'own' if own else (f'SHARED from {shared_with[0]}' if shared else f'WRONG {received}')
        print(f'  {mark} {name:<60} {note}')
        return ok

    def require(self, name, condition, detail=''):
        self.rows.append({'check': name, 'pass': bool(condition), 'detail': detail})
        print(f"  {'ok  ' if condition else 'FAIL'} {name:<60} {detail}")


def contexts():
    """(label, request headers, method, identity the origin will see, variant)."""
    return {
        'cookie-b': ({'Cookie': 'session=user-b'}, 'GET', 'user-b', 'html'),
        'cookies-b': ({'Cookie': 'theme=dark; session=user-b; consent=1'}, 'GET', 'user-b', 'html'),
        'auth-b': ({'Authorization': 'Bearer user-b'}, 'GET', 'auth:Bearer user-b', 'html'),
        'draft-b': ({'Cookie': 'session=user-b; __prerender_bypass=1'}, 'GET', 'user-b', 'html'),
        'action-b': ({'Cookie': 'session=user-b', 'Next-Action': 'abc123'}, 'POST', 'user-b', 'html'),
        'rsc-b': ({'Cookie': 'session=user-b', 'RSC': '1'}, 'GET', 'user-b', 'rsc'),
    }


A = ({'Cookie': 'session=user-a'}, ('user-a', 'html'))


async def pair(port, path, b_headers, b_method, concurrent):
    """User A, then user B either while A's render runs or after it ended."""
    first = asyncio.ensure_future(request(port, 'GET', path, A[0]))
    if concurrent:
        await asyncio.sleep(RENDER_MS / 1000 / 4)
    else:
        await first
        # Past the flight and past the store's 25 ms transient handoff.
        await asyncio.sleep(.15)
    second = asyncio.ensure_future(request(port, b_method, path, b_headers))
    return await first, await second


def may_share(route, answer, context, concurrent):
    """What the route's declared contract permits user B to receive from A."""
    if context in ('auth-b', 'draft-b', 'action-b', 'rsc-b'):
        return False
    if answer not in OVERRIDABLE:
        return False
    if route == 'cache':
        return True
    if route == 'co':
        return concurrent
    return False


async def matrix(port, results, counters):
    serial = 0
    for concurrent in (False, True):
        mode = 'concurrent' if concurrent else 'sequential'
        for route in ('u', 'pub', 'priv', 'co', 'cache'):
            for answer in ANSWERS:
                labels = ['cookie-b'] if answer != 'nextdyn' else list(contexts())
                for label in labels:
                    b_headers, method, who, variant = contexts()[label]
                    serial += 1
                    path = f'/{route}/{answer}/p{serial}'
                    (sa, _, ba), (sb, _, bb) = await pair(port, path, b_headers, method, concurrent)
                    permitted = may_share(route, answer, label, concurrent)
                    results.check(f'{mode} {path} A then {label}', A[1], ba, False)
                    ok = results.check(f'{mode} {path} {label}', (who, variant), bb, permitted,
                                       shared_with=A[1])
                    if ok and identity(bb) == A[1]:
                        counters[f'{route}:{mode}'] = counters.get(f'{route}:{mode}', 0) + 1


async def reload_during_flight(tanod, results):
    """A coalescing flight in progress when its override is removed."""
    path = '/co/nextdyn/reload'
    generation = await tanod.generation()
    first = asyncio.ensure_future(request(tanod.listen, 'GET', path, A[0]))
    await asyncio.sleep(RENDER_MS / 1000 / 4)
    without = ROUTES.replace('    coalesce: {override_origin: true}\n', '')
    await tanod.reload(without, generation)
    second = asyncio.ensure_future(request(tanod.listen, 'GET', path, {'Cookie': 'session=user-b'}))
    third = asyncio.ensure_future(request(tanod.listen, 'GET', path, {}))
    _, _, ba = await first
    _, _, bb = await second
    _, _, bc = await third
    results.check('reload: leader keeps its own response', A[1], ba, False)
    results.check('reload: cookie request after override removed', ('user-b', 'html'), bb, False,
                  shared_with=A[1])
    results.check('reload: anonymous request in the new namespace', ('anon', 'html'), bc, False,
                  shared_with=A[1])
    await tanod.reload(ROUTES, await tanod.generation())


async def cancelled_stream(port, results, counters):
    """The leader of a shared stream hangs up mid-body."""
    for route, permitted in (('co', True), ('pub', False), ('u', False)):
        path = f'/{route}/stream/cancel'
        leader = asyncio.ensure_future(request(port, 'GET', path, A[0], abort_after=b'chunk=0;'))
        await asyncio.sleep(RENDER_MS / 1000 / 4)
        follower = asyncio.ensure_future(request(port, 'GET', path, {'Cookie': 'session=user-b'}))
        await leader
        _, _, body = await follower
        complete = body.count(b'chunk=') == 4
        results.require(f'cancel {path}: follower body complete', complete,
                        f'{body.count(b"chunk=")} chunks')
        if results.check(f'cancel {path}: follower', ('user-b', 'html'), body, permitted,
                         shared_with=A[1]) and identity(body) == A[1]:
            counters['cancel:shared'] = counters.get('cancel:shared', 0) + 1
        await asyncio.sleep(.15)
        _, _, later = await request(port, 'GET', path, {'Cookie': 'session=user-c'})
        results.check(f'cancel {path}: later request', ('user-c', 'html'), later, False,
                      shared_with=A[1])


async def key_inputs(port, origin, results):
    """Inputs that Tanod and the origin could read differently."""
    # The origin decodes `%71` as `q`. If the key does not, the attacker's
    # page is stored for the query-less URL.
    for spelled in ('%71', 'q'):
        path = f'/search/public/poison-{len(results.rows)}'
        await request(port, 'GET', f'{path}?{spelled}=evil', {})
        await asyncio.sleep(.05)
        _, _, body = await request(port, 'GET', path, {})
        _, _, other = await request(port, 'GET', f'{path}?utm=1', {})
        results.require(f'query name {spelled}=evil does not reach {path}',
                        b';q=;' in body and b';q=;' in other,
                        body.decode(errors='replace')[-24:])
    # Dot segments: the origin resolves them, routes and classes would not.
    before = sum(origin.renders.values())
    for path in ('/cache/public/x/../../priv/nextdyn/p', '/_next/static/../../priv/nextdyn/p',
                 '/cache/public/%2e%2e/%2E%2e/priv/nextdyn/p', '/cache/public/./x', '/cache/public/x/..'):
        status, head, _ = await request(port, 'GET', path, A[0])
        results.require(f'dot segment refused: {path}', status == 400, f'status {status}')
    results.require('dot-segment requests never reached the origin',
                    sum(origin.renders.values()) == before)
    # Not a dot segment: still served.
    status, _, body = await request(port, 'GET', '/u/public/.../x..y', A[0])
    results.require('dots that are not a segment are served', status == 200, f'status {status}')


async def unclassified_override_is_refused(binary, folder, results):
    config = Path(folder) / 'refused.yaml'
    config.write_text(f'''version: 1
server: {{listen: "127.0.0.1:{free_port()}"}}
origin: {{upstreams: ["127.0.0.1:1"]}}
routes:
  - id: dashboard
    match: "/dashboard/**"
    coalesce: {{override_origin: true}}
''')
    check = subprocess.run([str(binary), 'check', '--config', str(config)],
                           capture_output=True, text=True)
    results.require('coalesce.override_origin without a class is refused',
                     check.returncode != 0 and 'without declaring a class' in check.stderr + check.stdout,
                     f'exit {check.returncode}')


# The adapter's generated route for a cookie-reading page, no route at all,
# and a route misdeclared public without an override: the cookie bypass is
# what protects the last two.
NEXT_POLICIES = {
    'generated-private': '''routes:
  - id: account
    match: "/account"
    class: private_dynamic
    cache: {enabled: false}
''',
    'unclassified': '',
    'public-no-override': '''routes:
  - id: account
    match: "/account"
    class: public_ssr
''',
}


RSC_TREE = ('%5B%22%22%2C%7B%22children%22%3A%5B%22__PAGE__%22%2C%7B%7D%2Cnull%2Cnull'
            '%2C4096%5D%7D%2Cnull%2Cnull%2C4112%5D')


async def next_get(port, headers, rsc):
    """GET /account; an RSC request follows Next's redirect to its `_rsc` URL."""
    if rsc:
        headers = {**headers, 'RSC': '1', 'Next-Url': '/', 'Next-Router-State-Tree': RSC_TREE}
    reply = await request(port, 'GET', '/account', headers, host='localhost')
    location = next(header(reply[1], 'location'), None)
    if rsc and reply[0] in (307, 308) and location and location.startswith('/account?_rsc='):
        reply = await request(port, 'GET', location, headers, host='localhost')
    return reply


async def next_fixture(binary, folder, upstream, policy, results):
    """Next's own cookie-reading page (`/account`) behind Tanod."""
    users = ['next-a', 'next-b', 'next-c', 'next-d']
    for label, routes in NEXT_POLICIES.items():
        sub = Path(folder) / f'next-{label}'
        sub.mkdir()
        tanod = Tanod(binary, sub, upstream, policy, routes=routes)
        try:
            await tanod.ready()
            for rsc in (False, True):
                for concurrent in (True, False):
                    tasks = []
                    for user in users:
                        tasks.append(asyncio.ensure_future(
                            next_get(tanod.listen, {'Cookie': f'session={user}'}, rsc)))
                        if not concurrent:
                            await tasks[-1]
                    replies = await asyncio.gather(*tasks)
                    statuses = [s for s, _, _ in replies]
                    # Each body, HTML or RSC flight, must name its own session
                    # and no other.
                    own = [user.encode() in body for user, (_, _, body) in zip(users, replies)]
                    leaked = [user for user, (_, _, body) in zip(users, replies)
                              if any(o.encode() in body for o in users if o != user)]
                    results.require(
                        f'next /account {label} rsc={rsc} concurrent={concurrent}',
                        all(s == 200 for s in statuses) and all(own) and not leaked,
                        f'status={statuses} own={own} leaked={leaked}')
        finally:
            await tanod.stop()
        log = (sub / 'tanod.log').read_text(errors='replace')
        results.require(f'next {label}: no panic', 'panicked' not in log)


async def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--tanod', default=str(ROOT / 'target/release/tanod'))
    parser.add_argument('--policy', default='clock', choices=['clock', 'tinyufo'])
    parser.add_argument('--json')
    parser.add_argument('--next-origin')
    parser.add_argument('--next-image', help='start this fixture image and use it as --next-origin')
    parser.add_argument('--skip-refusal', action='store_true',
                        help='for comparing a binary that predates the class fence')
    args = parser.parse_args()
    binary = Path(args.tanod).resolve()
    container = None
    if args.next_image:
        port = free_port()
        container = f'tanod-cache-isolation-{os.getpid()}'
        subprocess.run(['docker', 'run', '-d', '--rm', '--name', container, '-p', f'127.0.0.1:{port}:3000',
                        '-e', 'TANOD_BUILD_ID=next-fixture-v1', '-e', 'INSTANCE_ID=isolation',
                        args.next_image], check=True, capture_output=True)
        args.next_origin = f'127.0.0.1:{port}'
        for _ in range(240):
            try:
                await request(port, 'GET', '/healthz', {}, timeout=2)
                break
            except (OSError, asyncio.TimeoutError, ValueError, IndexError):
                await asyncio.sleep(.25)
    try:
        return await run(args, binary)
    finally:
        if container:
            subprocess.run(['docker', 'rm', '-f', container], capture_output=True)


async def run(args, binary):
    results, counters = Results(), {}
    origin = Origin()
    await origin.start()
    with tempfile.TemporaryDirectory(prefix='tanod-iso-', dir=os.environ.get('TMPDIR')) as folder:
        tanod = Tanod(binary, folder, f'127.0.0.1:{origin.port}', args.policy)
        try:
            await tanod.ready()
            print(f'cache isolation, {args.policy}, {binary}')
            await matrix(tanod.listen, results, counters)
            await reload_during_flight(tanod, results)
            await cancelled_stream(tanod.listen, results, counters)
            await key_inputs(tanod.listen, origin, results)
        finally:
            await tanod.stop()
        if not args.skip_refusal:
            await unclassified_override_is_refused(binary, folder, results)
        if args.next_origin:
            await next_fixture(binary, folder, args.next_origin, args.policy, results)
        log = (Path(folder) / 'tanod.log').read_text(errors='replace')
        results.require('no panic in the tanod log', 'panicked' not in log)
    # The permitted-sharing paths must actually have been taken, or the
    # isolation checks above prove nothing about them.
    for key in ('co:concurrent', 'cache:sequential', 'cache:concurrent'):
        results.require(f'permitted sharing exercised ({key})', counters.get(key, 0) > 0,
                        str(counters.get(key, 0)))
    failed = [r for r in results.rows if not r['pass']]
    print(f'\n{len(results.rows) - len(failed)}/{len(results.rows)} checks passed; '
          f'permitted sharing observed: {counters}')
    if args.json:
        Path(args.json).write_text(json.dumps({
            'policy': args.policy, 'binary': str(binary), 'rows': results.rows,
            'permitted_sharing': counters, 'origin_renders': sum(origin.renders.values()),
            'failed': len(failed)}, indent=1))
    return 1 if failed else 0


if __name__ == '__main__':
    sys.exit(asyncio.run(main()))
