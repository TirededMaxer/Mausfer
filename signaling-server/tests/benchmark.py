"""Local POSIX benchmark: python3 tests/benchmark.py target/mausfer-signaling.jar.

128 clients, heartbeat every 3 seconds for 15 seconds, then 6,400 relays of
16 KiB SDP. Only generated payloads over loopback. The child JVM uses
-Xms16m -Xmx128m -XX:ActiveProcessorCount=2. RSS is sampled every 200 ms;
CPU percent is relative to one CPU core and can exceed 100 percent.
Pre-encoded frames keep the Python client from dominating the measurement.
Short runs are not sustained capacity or a production memory guarantee.
"""
import asyncio, base64, hashlib, json, os, struct, subprocess, time, tempfile, shutil, sys
from pathlib import Path
JAVA = str(Path(os.environ['JAVA_HOME']) / 'bin/java') if 'JAVA_HOME' in os.environ else 'java'
traffic = [0, 0]

async def connect(port):
    r, w = await asyncio.open_connection('127.0.0.1', port)
    key = base64.b64encode(os.urandom(16)).decode()
    w.write(f'GET / HTTP/1.1\r\nHost: localhost:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n'.encode())
    await w.drain()
    h = await r.readuntil(b'\r\n\r\n')
    assert b' 101 ' in h
    return (r, w)
from functools import lru_cache

@lru_cache(maxsize=256)
def frame_for(text, opcode):
    data = text.encode()
    n = len(data)
    m = b'1234'
    head = bytes([128 | opcode, 128 | n]) if n < 126 else bytes([128 | opcode, 254]) + struct.pack('>H', n)
    return head + m + bytes((b ^ m[i % 4] for i, b in enumerate(data)))

async def send(c, obj, opcode=1):
    text = obj if isinstance(obj, str) else json.dumps(obj, separators=(',', ':'))
    frame = frame_for(text, opcode)
    c[1].write(frame)
    await c[1].drain()
    traffic[0] += len(frame)

async def recv(c):
    while True:
        h = await asyncio.wait_for(c[0].readexactly(2), 10)
        n = h[1] & 127
        header_bytes = len(h)
        if n == 126:
            n = struct.unpack('>H', await c[0].readexactly(2))[0]
            header_bytes += 2
        if n == 127:
            n = struct.unpack('>Q', await c[0].readexactly(8))[0]
            header_bytes += 8
        b = await c[0].readexactly(n)
        traffic[1] += header_bytes + n
        if h[0] & 15 == 9:
            await send(c, b.decode(), 10)
            continue
        assert h[0] & 15 == 1, (h, b)
        return b.decode()

def sample(pid):
    s = subprocess.check_output(['ps', '-o', 'rss=,time=', '-p', str(pid)], text=True).split()
    parts = s[1].split(':')
    return (int(s[0]) / 1024, sum((float(v) * 60 ** i for i, v in enumerate(reversed(parts)))))

async def run(jar):
    import socket
    with tempfile.TemporaryDirectory(prefix='mausfer-bench-') as d:
        p = Path(d)
        shutil.copy2(jar, p / 'server.jar')
        sock = socket.socket()
        sock.bind(('127.0.0.1', 0))
        port = sock.getsockname()[1]
        sock.close()
        (p / 'config.json').write_text(json.dumps({'port': port}))
        with open(p / 'server.log', 'w') as log:
            proc = subprocess.Popen([JAVA, '-Xms16m', '-Xmx128m', '-XX:ActiveProcessorCount=2', '-jar', str(p / 'server.jar')], stdout=log, stderr=log)
            clients = []
            try:
                for _ in range(100):
                    try:
                        c = await connect(port)
                        clients.append(c)
                        break
                    except OSError:
                        await asyncio.sleep(0.1)
                clients[0][1].close()
                await clients[0][1].wait_closed()
                clients = []
                for i in range(128):
                    clients.append(await connect(port))
                for i, c in enumerate(clients):
                    code = 'AAAAAAAAAAAA' + ''.join(('ABCDEFGH'[i // 2 >> shift & 7] for shift in [9, 6, 3, 0]))
                    await send(c, {'type': 'join', 'room': code, 'device': {'id': str(i), 'name': 'load-test', 'version': '0.1.0', 'port': 43110}})
                    if i % 2:
                        assert json.loads(await recv(clients[i - 1]))['type'] == 'peer_joined'
                        assert json.loads(await recv(c))['type'] == 'peer_joined'
                rss0, cpu0 = sample(proc.pid)
                t = time.monotonic()
                for _ in range(5):
                    await asyncio.gather(*(send(c, {'type': 'ping'}) for c in clients))
                    assert all((json.loads(x)['type'] == 'pong' for x in await asyncio.gather(*(recv(c) for c in clients))))
                    await asyncio.sleep(3)
                rss1, cpu1 = sample(proc.pid)
                idlecpu = (cpu1 - cpu0) / (time.monotonic() - t) * 100
                payload = json.dumps({'type': 'offer', 'sdp': {'kind': 'offer', 'sdp': 'v=0\r\n' + 'x' * 16384}}, separators=(',', ':'))
                lat = []
                rss = [rss1]
                active = True

                async def monitor():
                    while active:
                        rss.append(sample(proc.pid)[0])
                        await asyncio.sleep(0.2)
                monitor_task = asyncio.create_task(monitor())
                cpu0 = sample(proc.pid)[1]
                t = time.monotonic()
                start_bytes = traffic.copy()

                async def pair(a, b):
                    for _ in range(100):
                        begin = time.monotonic()
                        await send(a, payload)
                        assert await recv(b) == payload
                        lat.append((time.monotonic() - begin) * 1000)
                await asyncio.gather(*(pair(clients[i], clients[i + 1]) for i in range(0, 128, 2)))
                elapsed = time.monotonic() - t
                cpu = sample(proc.pid)[1] - cpu0
                active = False
                await monitor_task
                for c in clients:
                    c[1].close()
                await asyncio.gather(*(c[1].wait_closed() for c in clients))
                await asyncio.sleep(2)
                lat.sort()
                print(json.dumps({'jar': str(jar), 'clients': 128, 'relay_messages': 6400, 'seconds': round(elapsed, 3), 'messages_per_second': round(6400 / elapsed), 'cpu_percent_one_core': round(cpu / elapsed * 100, 2), 'idle_cpu_percent': round(idlecpu, 2), 'rss_idle_MiB': round(rss1, 1), 'rss_peak_MiB': round(max(rss), 1), 'rss_after_disconnect_MiB': round(sample(proc.pid)[0], 1), 'p95_ms': round(lat[int(len(lat) * 0.95)], 2), 'wire_tx_bytes': traffic[0] - start_bytes[0], 'wire_rx_bytes': traffic[1] - start_bytes[1]}), flush=True)
            finally:
                for c in clients:
                    c[1].close()
                proc.terminate()
                proc.wait(timeout=5)
if __name__ == '__main__':
    asyncio.run(run(Path(sys.argv[1])))
