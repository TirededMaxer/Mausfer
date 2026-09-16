"""Real WebSocket regression checks. Build the JAR, then run with Python 3.11+."""
import asyncio
import base64
import json
import os
import struct
import subprocess
import tempfile
import shutil
import unittest
from pathlib import Path

async def connect(port):
    r, w = await asyncio.open_connection('127.0.0.1', port)
    key = base64.b64encode(os.urandom(16)).decode()
    w.write(f'GET / HTTP/1.1\r\nHost: localhost:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n'.encode())
    await w.drain()
    h = await asyncio.wait_for(r.readuntil(b'\r\n\r\n'), 5)
    assert b' 101 ' in h
    return (r, w)

async def send(c, obj, opcode=1):
    data = obj if isinstance(obj, bytes) else obj.encode() if isinstance(obj, str) else json.dumps(obj, separators=(',', ':')).encode()
    n = len(data)
    m = b'1234'
    head = bytes([128 | opcode, 128 | n]) if n < 126 else bytes([128 | opcode, 254]) + struct.pack('>H', n) if n < 65536 else bytes([128 | opcode, 255]) + struct.pack('>Q', n)
    frame = head + m + bytes((b ^ m[i % 4] for i, b in enumerate(data)))
    c[1].write(frame)
    await c[1].drain()

async def recv(c):
    while True:
        h = await asyncio.wait_for(c[0].readexactly(2), 10)
        n = h[1] & 127
        if n == 126:
            n = struct.unpack('>H', await c[0].readexactly(2))[0]
        if n == 127:
            n = struct.unpack('>Q', await c[0].readexactly(8))[0]
        b = await c[0].readexactly(n)
        if h[0] & 15 == 9:
            await send(c, b, 10)
            continue
        if h[0] & 15 == 8:
            return {'close': struct.unpack('>H', b[:2])[0]}
        assert h[0] & 15 == 1, (h, b)
        return b.decode()

class ServerTests(unittest.IsolatedAsyncioTestCase):

    async def asyncSetUp(self):
        import socket
        self.temp = tempfile.TemporaryDirectory(prefix='mausfer-signal-test-')
        root = Path(self.temp.name)
        jar = Path(os.environ.get('MAUSFER_TEST_JAR', Path(__file__).resolve().parents[1] / 'target/mausfer-signaling.jar'))
        shutil.copy2(jar, root / 'server.jar')
        s = socket.socket()
        s.bind(('127.0.0.1', 0))
        self.port = s.getsockname()[1]
        s.close()
        (root / 'config.json').write_text(json.dumps({'port': self.port}))
        java = str(Path(os.environ['JAVA_HOME']) / 'bin/java') if 'JAVA_HOME' in os.environ else 'java'
        self.proc = subprocess.Popen([java, '-Xms16m', '-Xmx128m', '-jar', str(root / 'server.jar')], stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        self.clients = []
        for _ in range(100):
            try:
                c = await connect(self.port)
                self.clients.append(c)
                return
            except OSError:
                await asyncio.sleep(0.1)
        self.proc.terminate()
        self.proc.wait(timeout=5)
        self.temp.cleanup()
        self.fail('server did not start')

    async def asyncTearDown(self):
        for c in self.clients:
            c[1].close()
        if self.proc.poll() is None:
            self.proc.kill()
        self.proc.wait(timeout=5)
        self.proc.stdout.close()
        self.temp.cleanup()

    async def client(self):
        c = await connect(self.port)
        self.clients.append(c)
        return c

    async def pair(self):
        a, b = (await self.client(), await self.client())
        for i, c in enumerate([a, b]):
            await send(c, {'type': 'join', 'room': 'ABCD-EFGH-JKLM-NPQR', 'device': {'id': str(i), 'name': 'test', 'version': '0.1.0', 'port': 43110}})
        for c in [a, b]:
            self.assertEqual(json.loads(await recv(c))['type'], 'peer_joined')
        return (a, b)

    async def test_bidirectional_relay_heartbeat_and_room_cleanup(self):
        a, b = await self.pair()
        for kind, x, y in [('offer', a, b), ('answer', b, a)]:
            obj = {'type': kind, 'sdp': {'kind': kind, 'sdp': 'v=0\r\n中文'}}
            raw = json.dumps(obj, ensure_ascii=False, indent=2)
            await send(x, raw)
            self.assertEqual(json.loads(await recv(y)), obj)
        obj = {'type': 'ice', 'candidate': 'candidate:1', 'sdp_mid': '0', 'sdp_mline_index': 0}
        await send(a, obj)
        self.assertEqual(json.loads(await recv(b)), obj)
        b[1].close()
        await b[1].wait_closed()
        self.assertEqual(json.loads(await recv(a))['type'], 'peer_left')
        c = await self.client()
        await send(c, {'type': 'join', 'room': 'ABCDEFGHJKLMNPQR', 'device': {'id': 'new', 'name': 'test', 'version': '0.1.0', 'port': 0}})
        for x in [a, c]:
            self.assertEqual(json.loads(await recv(x))['type'], 'peer_joined')
        await send(a, {'type': 'ping'})
        self.assertEqual(json.loads(await recv(a)), {'type': 'pong'})

    async def test_invalid_binary_and_oversized_messages(self):
        for data, opcode in [('not-json', 1), ('binary', 2)]:
            c = await self.client()
            await send(c, data, opcode)
            self.assertEqual(json.loads(await recv(c))['type'], 'error')
            self.assertEqual((await recv(c))['close'], 1008)
        c = await self.client()
        await send(c, 'x' * 300000)
        self.assertEqual((await recv(c))['close'], 1009)

    async def test_message_rate_limit(self):
        c = await self.client()
        for i in range(129):
            await send(c, {'type': 'ping'})
            reply = json.loads(await recv(c))
            self.assertEqual(reply['type'], 'pong' if i < 128 else 'error')
        self.assertEqual((await recv(c))['close'], 1008)

    async def test_blocked_stdout_does_not_block_signaling(self):
        """Unread stdout and an overflowing log queue must not block other peers."""
        for _ in range(1400):
            c = await connect(self.port)
            c[1].close()
            await c[1].wait_closed()
        c = await self.client()
        await send(c, {'type': 'ping'})
        self.assertEqual(json.loads(await recv(c)), {'type': 'pong'})

        self.proc.terminate()
        await asyncio.wait_for(asyncio.to_thread(self.proc.wait), 5)

    async def test_slow_peer_disconnect_keeps_sender_usable(self):
        a, b = await self.pair()
        b[0]._transport.pause_reading()
        msg = {'type': 'offer', 'sdp': {'kind': 'offer', 'sdp': 'x' * 240000}}
        for _ in range(90):
            await send(a, msg)
            await asyncio.sleep(0.02)
        self.assertEqual(json.loads(await recv(a))['type'], 'peer_left')
        await send(a, {'type': 'ping'})
        self.assertEqual(json.loads(await recv(a)), {'type': 'pong'})
if __name__ == '__main__':
    unittest.main()
