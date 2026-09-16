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
                System.out.println("Mausfer signaling stopped.");
            }, "mausfer-shutdown"));
            server.start();
            server.started.get(10, TimeUnit.SECONDS);
            System.out.println("Config: " + config);
            System.out.println("Mausfer signaling listening on 0.0.0.0:" + port + " (WebSocket). Ctrl+C to stop.");
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

    private static final class Server extends WebSocketServer {
        // Bound frames, clients, queued output and per-connection message rate.
        private static final int MAX_BYTES = 256 * 1024;
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

        @Override public synchronized void onMessage(WebSocket socket, String text) {
            Peer peer = peers.get(socket);
            if (peer == null || !socket.isOpen()) return;
            try {
                long now = System.nanoTime();
                if (now - peer.window >= TimeUnit.SECONDS.toNanos(1)) {
                    peer.window = now; peer.messages = 0;
                }
                if (++peer.messages > 128 || text.length() > MAX_BYTES) {
                    reject(socket, "message limit exceeded"); return;
                }
                JsonObject message = JsonParser.parseString(text).getAsJsonObject();
                String type = string(message, "type", 24);
                peer.activity = now;
                if (type.equals("ping")) {
                    JsonObject pong = new JsonObject(); pong.addProperty("type", "pong");
                    sendBounded(socket, pong);
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
                    for (WebSocket target : rooms.getOrDefault(peer.room, List.of())) {
                        if (target != socket) sendBounded(target, message);
                    }
                } else {
                    reject(socket, "unsupported message type");
                }
            } catch (RuntimeException e) {
                reject(socket, "invalid signaling message");
            }
        }

        private void join(WebSocket socket, Peer peer, JsonObject message) {
            if (peer.room != null) { reject(socket, "already joined a room"); return; }
            String room = string(message, "room", 64).replaceAll("[-\\s]", "").toUpperCase(Locale.ROOT);
            if (!room.matches("[A-HJ-NP-Z2-9]{16}")) { reject(socket, "invalid room code"); return; }
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
            if (!socket.isOpen()) return;
            if (socket instanceof WebSocketImpl connection
                    && connection.outQueue.stream().mapToLong(ByteBuffer::remaining).sum() > 1024 * 1024) {
                // Never let a slow peer accumulate unlimited handshake messages.
                socket.close(1008, "peer is too slow");
            } else {
                socket.send(message.toString());
            }
        }

        private static void log(WebSocket socket, String event) {
            System.out.println(java.time.Instant.now() + " [" + socket.getRemoteSocketAddress() + "] " + event);
        }

        private static void reject(WebSocket socket, String reason) {
            log(socket, "rejected: " + reason);
            if (!socket.isOpen()) return;
            JsonObject error = new JsonObject();
            error.addProperty("type", "error"); error.addProperty("message", reason);
            socket.send(error.toString());
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
                JsonObject left = new JsonObject(); left.addProperty("type", "peer_left");
                sendBounded(roomPeers.get(0), left);
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
