package io.mausfer.signaling;

import com.google.gson.JsonObject;
import com.google.gson.JsonParser;
import java.net.InetSocketAddress;
import java.nio.ByteBuffer;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.atomic.AtomicLong;
import java.util.regex.Pattern;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.TimeUnit;
import org.java_websocket.WebSocket;
import org.java_websocket.WebSocketImpl;
import org.java_websocket.drafts.Draft_6455;
import org.java_websocket.handshake.ClientHandshake;
import org.java_websocket.server.WebSocketServer;

/** Foreground server: no installer, database or system service is required. */
public final class Main {
    public static void main(String[] args) {
        try {
            Path location = Path.of(Main.class.getProtectionDomain().getCodeSource().getLocation().toURI());
            Path directory = Files.isDirectory(location) ? location : location.getParent();
            Path config = directory.resolve("config.json");
            if (!Files.exists(config)) {
                Files.writeString(config, "{\n  \"port\": 38386\n}\n", StandardOpenOption.CREATE_NEW);
            }
            JsonObject settings = JsonParser.parseString(Files.readString(config)).getAsJsonObject();
            if (settings.size() != 1 || !settings.has("port")
                    || !settings.get("port").isJsonPrimitive()
                    || !settings.getAsJsonPrimitive("port").isNumber()) {
                throw new IllegalArgumentException("config.json must contain only a numeric port");
            }
            int port = settings.get("port").getAsBigDecimal().intValueExact();
            if (port < 1 || port > 65535) throw new IllegalArgumentException("port must be 1..65535");
            Server server = new Server(port);
            Runtime.getRuntime().addShutdownHook(new Thread(() -> {
                server.expiry.shutdownNow();
                try { server.stop(2000, "server shutting down"); }
                catch (InterruptedException e) { Thread.currentThread().interrupt(); }
                server.logs.offer(null, "Mausfer signaling stopped.");
                server.logs.close();
            }, "mausfer-shutdown"));
            server.start();
            server.started.get(10, TimeUnit.SECONDS);
            server.logs.offer(null, "Config: " + config);
            server.logs.offer(null, "Mausfer signaling listening on 0.0.0.0:" + port + " (WebSocket). Ctrl+C to stop.");
        } catch (Exception e) {
            System.err.println("Cannot start Mausfer signaling: " + e.getMessage());
            System.exit(1);
        }
    }

    private static final class Peer {
        final long opened = System.nanoTime();
        long activity = opened;
        String room;
        JsonObject device;
        long window = opened;
        int messages;
    }

    private static final class AsyncLog implements AutoCloseable {
        private record Line(java.time.Instant time, String address, String event) {}
        private final ArrayBlockingQueue<Line> queue = new ArrayBlockingQueue<>(1024);
        private final AtomicLong dropped = new AtomicLong();
        private volatile boolean running = true;
        private final Thread writer;

        AsyncLog() {
            writer = new Thread(this::write, "mausfer-log");
            writer.setDaemon(true);
            writer.start();
        }

        void offer(WebSocket socket, String event) {
            if (!queue.offer(new Line(java.time.Instant.now(),
                    socket == null ? "server" : String.valueOf(socket.getRemoteSocketAddress()), event))) dropped.incrementAndGet();
        }

        private void write() {
            while (running || !queue.isEmpty()) {
                try {
                    Line first = queue.poll(250, TimeUnit.MILLISECONDS);
                    if (first == null) continue;
                    StringBuilder batch = new StringBuilder();
                    Line line = first;
                    for (int i = 0; line != null; i++) {
                        batch.append(line.time()).append(" [").append(line.address())
                                .append("] ").append(line.event()).append('\n');
                        line = i < 31 ? queue.poll() : null;
                    }
                    long count = dropped.getAndSet(0);
                    if (count > 0) batch.append(java.time.Instant.now()).append(" [log] dropped ")
                            .append(count).append(" events: log output is too slow\n");
                    System.out.print(batch);
                } catch (InterruptedException ignored) {
                    // Shutdown wakes the poll; drain queued entries when output is writable.
                }
            }
        }

        @Override public void close() {
            running = false;
            writer.interrupt();
            try { writer.join(500); }
            catch (InterruptedException e) { Thread.currentThread().interrupt(); }
        }
    }

    private static final class Server extends WebSocketServer {
        // Bound frames, clients, queued output and per-connection message rate.
        private static final int MAX_BYTES = 256 * 1024;
        private static final int MAX_QUEUED_BYTES = 256 * 1024;
        private static final int MAX_QUEUED_FRAMES = 64;
        private static final String PING = "{\"type\":\"ping\"}";
        private static final String PONG = "{\"type\":\"pong\"}";
        private static final String PEER_LEFT = "{\"type\":\"peer_left\"}";
        private static final Pattern ROOM_SEPARATORS = Pattern.compile("[-\\s]");
        private static final Pattern ROOM_CODE = Pattern.compile("[A-HJ-NP-Z2-9]{16}");
        private final AsyncLog logs = new AsyncLog();
        private final Map<WebSocket, Peer> peers = new HashMap<>();
        private final Map<String, List<WebSocket>> rooms = new HashMap<>();
        final CompletableFuture<Void> started = new CompletableFuture<>();
        final ScheduledExecutorService expiry = Executors.newSingleThreadScheduledExecutor(r -> {
            Thread thread = new Thread(r, "mausfer-expiry");
            thread.setDaemon(true);
            return thread;
        });

        Server(int port) {
            super(new InetSocketAddress("0.0.0.0", port), 2,
                    List.of(new Draft_6455(List.of(), MAX_BYTES)));
            setReuseAddr(true);
            setTcpNoDelay(true);
            setConnectionLostTimeout(30);
            setMaxPendingConnections(128);
        }

        @Override public void onStart() {
            expiry.scheduleAtFixedRate(this::expire, 10, 10, TimeUnit.SECONDS);
            started.complete(null);
        }

        @Override public synchronized void onOpen(WebSocket socket, ClientHandshake handshake) {
            if (peers.size() >= 1024) { reject(socket, "server is full"); return; }
            peers.put(socket, new Peer());
            log(socket, "connected");
        }

        private synchronized void expire() {
            long now = System.nanoTime();
            for (var entry : new ArrayList<>(peers.entrySet())) {
                Peer peer = entry.getValue();
                long limit = TimeUnit.SECONDS.toNanos(peer.room == null ? 15 : 20 * 60);
                if (now - (peer.room == null ? peer.activity : peer.opened) > limit) reject(entry.getKey(), "session expired");
            }
        }

        @Override public void onMessage(WebSocket socket, String text) {
            Peer peer;
            synchronized (this) {
                peer = peers.get(socket);
                if (peer == null || !socket.isOpen()) return;
                long now = System.nanoTime();
                if (now - peer.window >= TimeUnit.SECONDS.toNanos(1)) {
                    peer.window = now; peer.messages = 0;
                }
                if (++peer.messages > 128 || text.length() > MAX_BYTES) {
                    reject(socket, "message limit exceeded"); return;
                }
                if (text.equals(PING)) {
                    peer.activity = now;
                    sendBounded(socket, PONG);
                    return;
                }
            }
            try {
                // Parse outside the room lock so independent decoder threads can work concurrently.
                JsonObject message = JsonParser.parseString(text).getAsJsonObject();
                String type = string(message, "type", 24);
                synchronized (this) {
                    if (peers.get(socket) != peer || !socket.isOpen()) return;
                    peer.activity = System.nanoTime();
                    if (type.equals("ping")) {
                        sendBounded(socket, PONG);
                    } else if (type.equals("join")) {
                        join(socket, peer, message);
                    } else if (type.equals("offer") || type.equals("answer") || type.equals("ice")) {
                        if (peer.room == null) { reject(socket, "join a room first"); return; }
                        if (type.equals("ice")) {
                            string(message, "candidate", 16384);
                        } else {
                            JsonObject sdp = message.getAsJsonObject("sdp");
                            if (!type.equals(string(sdp, "kind", 8))) throw new IllegalArgumentException();
                            string(sdp, "sdp", MAX_BYTES);
                        }
                        if (!type.equals("ice")) log(socket, "relaying " + type);
                        List<WebSocket> members = rooms.get(peer.room);
                        if (members != null && members.size() == 2) {
                            WebSocket target = members.get(members.get(0) == socket ? 1 : 0);
                            sendBounded(target, text);
                        }
                    } else {
                        reject(socket, "unsupported message type");
                    }
                }
            } catch (RuntimeException e) {
                reject(socket, "invalid signaling message");
            }
        }

        private void join(WebSocket socket, Peer peer, JsonObject message) {
            if (peer.room != null) { reject(socket, "already joined a room"); return; }
            String room = ROOM_SEPARATORS.matcher(string(message, "room", 64)).replaceAll("").toUpperCase(Locale.ROOT);
            if (!ROOM_CODE.matcher(room).matches()) { reject(socket, "invalid room code"); return; }
            JsonObject supplied = message.getAsJsonObject("device");
            JsonObject device = new JsonObject();
            device.addProperty("id", string(supplied, "id", 128));
            device.addProperty("name", string(supplied, "name", 256));
            device.addProperty("version", string(supplied, "version", 32));
            int port = supplied.get("port").getAsBigDecimal().intValueExact();
            if (port < 0 || port > 65535) throw new IllegalArgumentException();
            device.addProperty("port", port);
            List<WebSocket> roomPeers = rooms.computeIfAbsent(room, ignored -> new ArrayList<>(2));
            if (roomPeers.size() >= 2 || roomPeers.stream().anyMatch(other ->
                    peers.get(other).device.get("id").equals(device.get("id")))) {
                reject(socket, "room is full"); return;
            }
            peer.room = room;
            peer.device = device;
            roomPeers.add(socket);
            log(socket, "joined room; peers=" + roomPeers.size());
            if (roomPeers.size() == 2) {
                log(socket, "paired; starting connection negotiation");
                WebSocket first = roomPeers.get(0);
                JsonObject toFirst = new JsonObject();
                toFirst.addProperty("type", "peer_joined"); toFirst.add("peer", device);
                JsonObject toSecond = new JsonObject();
                toSecond.addProperty("type", "peer_joined"); toSecond.add("peer", peers.get(first).device);
                sendBounded(first, toFirst);
                sendBounded(socket, toSecond);
            }
        }

        private static String string(JsonObject object, String key, int limit) {
            var value = object.get(key);
            if (value == null || !value.isJsonPrimitive() || !value.getAsJsonPrimitive().isString())
                throw new IllegalArgumentException();
            String text = value.getAsString();
            if (text.isBlank() || text.length() > limit) throw new IllegalArgumentException();
            return text;
        }

        private static void sendBounded(WebSocket socket, JsonObject message) {
            sendBounded(socket, message.toString());
        }

        private static void sendBounded(WebSocket socket, String message) {
            if (!socket.isOpen()) return;
            if (socket instanceof WebSocketImpl connection) {
                // The frame cap makes this scan bounded even for tiny queued messages.
                if (connection.outQueue.size() >= MAX_QUEUED_FRAMES) {
                    socket.closeConnection(1008, "peer is too slow");
                    return;
                }
                long queued = 0;
                for (ByteBuffer buffer : connection.outQueue) {
                    queued += buffer.remaining();
                    if (queued > MAX_QUEUED_BYTES) {
                        socket.closeConnection(1008, "peer is too slow");
                        return;
                    }
                }
            }
            socket.send(message);
        }

        private void log(WebSocket socket, String event) {
            logs.offer(socket, event);
        }

        private void reject(WebSocket socket, String reason) {
            log(socket, "rejected: " + reason);
            if (!socket.isOpen()) return;
            JsonObject error = new JsonObject();
            error.addProperty("type", "error"); error.addProperty("message", reason);
            sendBounded(socket, error);
            socket.close(1008, reason);
        }

        @Override public synchronized void onClose(WebSocket socket, int code, String reason, boolean remote) {
            log(socket, "disconnected code=" + code + " remote=" + remote);
            Peer peer = peers.remove(socket);
            if (peer == null || peer.room == null) return;
            List<WebSocket> roomPeers = rooms.get(peer.room);
            if (roomPeers == null) return;
            roomPeers.remove(socket);
            if (roomPeers.isEmpty()) rooms.remove(peer.room);
            else {
                sendBounded(roomPeers.get(0), PEER_LEFT);
            }
        }

        @Override public void onMessage(WebSocket socket, ByteBuffer bytes) {
            reject(socket, "binary messages are not supported");
        }

        @Override public void onError(WebSocket socket, Exception error) {
            if (socket == null) {
                started.completeExceptionally(error);
                System.err.println("Signaling listener error: " + error.getMessage());
            } else {
                log(socket, "connection error: " + error.getClass().getSimpleName());
                socket.close(1011, "connection error");
            }
        }
    }
}
