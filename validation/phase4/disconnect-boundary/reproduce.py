import asyncio, json, os, re, signal, socket, struct, subprocess, tempfile
from pathlib import Path
ROOT=Path(__file__).resolve().parents[3]
BINARY=Path(os.environ.get('TANOD_BIN',str(ROOT/'target/release/tanod')))
OUT=ROOT/'benchmark-results/phase4/disconnect-boundary';OUT.mkdir(parents=True,exist_ok=True)
def port():
 with socket.socket() as s:s.bind(('127.0.0.1',0));return s.getsockname()[1]
async def get(p,path='/metrics'):
 r,w=await asyncio.open_connection('127.0.0.1',p);w.write(f'GET {path} HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n'.encode());await w.drain();data=await r.read();w.close();await w.wait_closed();return data
async def scenario(name,cached,before_headers,pressure=False):
 listen,metrics,origin=port(),port(),port();accepted=asyncio.Event();finish=asyncio.Event();active=0;peak=0;number=0
 async def serve(r,w):
  nonlocal active,peak,number
  try:
   await r.readuntil(b'\r\n\r\n');number+=1;index=number;active+=1;peak=max(peak,active)
   if index==1:
    accepted.set()
    if before_headers:await asyncio.sleep(.15)
    w.write(b'HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nCache-Control: public,max-age=60\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nfirst\r\n');await w.drain()
    await asyncio.sleep(.1)
    try:w.write(b'6\r\nsecond\r\n');await w.drain()
    except (ConnectionError,OSError):pass
    # Deliberately model backend work that does not stop on a closed socket.
    await finish.wait()
   else:w.write(b'HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok');await w.drain()
  except (ConnectionError,OSError,asyncio.IncompleteReadError):pass
  finally:
   active-=1;w.close()
 server=await asyncio.start_server(serve,'127.0.0.1',origin)
 cfg=OUT/f'{name}.yaml'
 cfg.write_text(f'''version: 1
server:
  listen: "127.0.0.1:{listen}"
  threads: 1
  graceful:
    drain_period: 0s
    shutdown_timeout: 1s
    pid_file: "{OUT}/{name}.pid"
    upgrade_socket: "{OUT}/{name}.sock"
origin:
  upstreams: ["127.0.0.1:{origin}"]
  concurrency:
    max: 1
    queue: {{max: 4, timeout: 1s}}
cache: {{enabled: {str(cached).lower()}, max_memory: {"1B" if pressure else "8MiB"}, max_body_size: {"1B" if pressure else "1MiB"}}}
telemetry:
  prometheus: {{listen: "127.0.0.1:{metrics}"}}
routes:
  - id: fixture
    match: "/**"
    class: {'public_ssr' if cached else 'private_dynamic'}
''')
 log=(OUT/f'{name}.log').open('wb');process=subprocess.Popen([str(BINARY),'run','--config',str(cfg)],stdout=log,stderr=log,env={**os.environ,'RUST_LOG':'info'})
 try:
  for _ in range(100):
   try:await get(metrics);break
   except OSError:await asyncio.sleep(.02)
  else:raise RuntimeError('process not ready')
  r,w=await asyncio.open_connection('127.0.0.1',listen);w.write(b'GET /first HTTP/1.1\r\nHost: fixture\r\n\r\n');await w.drain();await accepted.wait()
  if not before_headers:await asyncio.wait_for(r.readuntil(b'first'),2)
  w.get_extra_info('socket').setsockopt(socket.SOL_SOCKET,socket.SO_LINGER,struct.pack('ii',1,0));w.transport.abort()
  released=False
  for _ in range(100):
   raw=await get(metrics);m=re.search(rb'tanod_origin_in_flight\{limiter="global"\} (\d+)',raw)
   if m and int(m[1])==0:released=True;break
   await asyncio.sleep(.01)
  still_active=active
  if released:await asyncio.wait_for(get(listen,'/second'),2)
  result=dict(scenario=name,cached=cached,disconnect_before_headers=before_headers,limit=1,permit_released_before_backend_finished=released,backend_active_at_release=still_active,origin_peak=peak)
  print(json.dumps(result),flush=True);return result
 finally:
  finish.set();await asyncio.sleep(.1);process.send_signal(signal.SIGTERM)
  await asyncio.to_thread(process.wait,5);log.close();server.close();await server.wait_closed()
async def main():
 rows=[]
 for args in [('private-after-headers',False,False),('cached-after-headers',True,False),('cached-before-headers',True,True),('cached-pressure-after-headers',True,False,True)]:rows.append(await scenario(*args))
 (OUT/'result.json').write_text(json.dumps(rows,indent=2)+'\n')
asyncio.run(main())
