#!/usr/bin/env python3
"""Mock openab-pty runtime for reverse-attach smoke tests. stdlib only.

Endpoints (per openab-pty CLIENT-CONTRACT §9, as replicated by the Swift client):
  POST /admin/sessions/<session>/tools-attach   Authorization: Bearer <ADMIN>
       body {"ttl_secs": N}  -> 200 {"secret": ..., "expires_in": N}
  GET  /tools/attach/<session>  (WebSocket)     Authorization: Bearer <secret>
       -> 101, then the runtime (this side) speaks MCP JSON-RPC to the hands
          node, logs every reply, and closes with a scripted close code.

Env:
  PORT        listen port (default 18090)
  ADMIN       admin credential expected on mint
  SECRETS     comma-separated "session=secret" pre-minted pairs (for the
              secret-not-admin path)
  CLOSES      comma-separated close codes to use on successive attaches
              (default "1000,4010"); last one repeats
  LOG         JSON-lines file for observed traffic (default /tmp/mock-runtime.jsonl)
"""
import base64, hashlib, json, os, socket, struct, sys, threading, time, urllib.parse

PORT = int(os.environ.get("PORT", "18090"))
ADMIN = os.environ.get("ADMIN", "admin-secret")
CLOSES = [int(c) for c in os.environ.get("CLOSES", "1000,4010").split(",")]
LOG = os.environ.get("LOG", "/tmp/mock-runtime.jsonl")
GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

secrets = {}
for pair in filter(None, os.environ.get("SECRETS", "").split(",")):
    s, v = pair.split("=", 1)
    secrets[s] = v
attach_count = 0
lock = threading.Lock()


def log(ev):
    ev["t"] = round(time.time(), 3)
    with lock:
        with open(LOG, "a") as f:
            f.write(json.dumps(ev) + "\n")
    print(json.dumps(ev), flush=True)


def read_http(conn):
    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = conn.recv(4096)
        if not chunk:
            return None
        buf += chunk
    head, rest = buf.split(b"\r\n\r\n", 1)
    lines = head.decode("latin1").split("\r\n")
    method, path, _ = lines[0].split(" ", 2)
    headers = {}
    for l in lines[1:]:
        k, _, v = l.partition(":")
        headers[k.strip().lower()] = v.strip()
    n = int(headers.get("content-length", "0"))
    while len(rest) < n:
        chunk = conn.recv(4096)
        if not chunk:
            break
        rest += chunk
    return method, path, headers, rest[:n]


def respond(conn, status, reason, body):
    b = json.dumps(body).encode()
    conn.sendall(
        f"HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n"
        f"Content-Length: {len(b)}\r\nConnection: close\r\n\r\n".encode() + b
    )


# --- minimal WS framing (server side: send unmasked, receive masked) ---------
def ws_send(conn, opcode, payload):
    hdr = bytes([0x80 | opcode])
    n = len(payload)
    if n < 126:
        hdr += bytes([n])
    elif n < 65536:
        hdr += bytes([126]) + struct.pack(">H", n)
    else:
        hdr += bytes([127]) + struct.pack(">Q", n)
    conn.sendall(hdr + payload)


def recv_exact(conn, n):
    b = b""
    while len(b) < n:
        c = conn.recv(n - len(b))
        if not c:
            raise ConnectionError("eof")
        b += c
    return b


def ws_recv(conn):
    b1, b2 = recv_exact(conn, 2)
    opcode = b1 & 0x0F
    masked = b2 & 0x80
    n = b2 & 0x7F
    if n == 126:
        n = struct.unpack(">H", recv_exact(conn, 2))[0]
    elif n == 127:
        n = struct.unpack(">Q", recv_exact(conn, 8))[0]
    mask = recv_exact(conn, 4) if masked else None
    data = recv_exact(conn, n)
    if mask:
        data = bytes(d ^ mask[i % 4] for i, d in enumerate(data))
    return opcode, data


def ws_close(conn, code):
    ws_send(conn, 0x8, struct.pack(">H", code))
    try:
        conn.settimeout(3)
        op, _ = ws_recv(conn)  # peer's close echo
        log({"ev": "close_echo", "opcode": op})
    except Exception as e:
        log({"ev": "close_echo_missing", "err": str(e)})


def rpc(conn, id_, method, params=None):
    msg = {"jsonrpc": "2.0", "id": id_, "method": method}
    if params is not None:
        msg["params"] = params
    ws_send(conn, 0x1, json.dumps(msg).encode())
    while True:
        op, data = ws_recv(conn)
        if op == 0x1:
            reply = json.loads(data)
            log({"ev": "rpc", "method": method, "reply": reply})
            return reply
        if op == 0x9:
            ws_send(conn, 0xA, data)


def drive_mcp(conn, session, nth):
    """Scripted turn: initialize, tools/list, sys_info, exec, notification."""
    rpc(conn, 1, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                "clientInfo": {"name": "mock-runtime", "version": "0"}})
    # notification: must produce no reply; verified by the next rpc's id
    ws_send(conn, 0x1, json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}).encode())
    r = rpc(conn, 2, "tools/list")
    assert r["id"] == 2, "notification produced a reply"
    rpc(conn, 3, "tools/call", {"name": "sys_info", "arguments": {}})
    rpc(conn, 4, "tools/call", {"name": "bash", "arguments": {"command": "echo hands-node-$(hostname); pwd", "cwd": "~"}})
    rpc(conn, 7, "tools/call", {"name": "bash", "arguments": {"command": "sleep 30 & sleep 30", "timeout_secs": 1}})
    rpc(conn, 5, "tools/call", {"name": "nope", "arguments": {}})
    rpc(conn, 6, "bogus/method")
    ws_send(conn, 0x9, b"ping")
    op, data = ws_recv(conn)
    log({"ev": "pong", "ok": op == 0xA and data == b"ping"})


def handle(conn, addr):
    global attach_count
    try:
        conn.settimeout(20)
        req = read_http(conn)
        if not req:
            return
        method, path, headers, body = req
        auth = headers.get("authorization", "")
        bearer = auth[7:] if auth.startswith("Bearer ") else None
        p = urllib.parse.urlparse(path).path.strip("/").split("/")

        if method == "POST" and p[:2] == ["admin", "sessions"] and p[3:] == ["tools-attach"]:
            session = p[2]
            if bearer != ADMIN:
                log({"ev": "mint", "session": session, "status": 401})
                return respond(conn, 401, "Unauthorized", {"error": "bad admin credential"})
            ttl = json.loads(body or "{}").get("ttl_secs")
            secret = "minted-" + base64.urlsafe_b64encode(os.urandom(9)).decode()
            with lock:
                secrets[session] = secret
            log({"ev": "mint", "session": session, "status": 200, "ttl_secs": ttl})
            return respond(conn, 200, "OK", {"secret": secret, "expires_in": ttl})

        if method == "GET" and p[:2] == ["tools", "attach"] and len(p) == 3:
            session = p[2]
            if headers.get("upgrade", "").lower() != "websocket":
                return respond(conn, 400, "Bad Request", {"error": "expected websocket"})
            if bearer != secrets.get(session):
                log({"ev": "attach", "session": session, "status": 401})
                return respond(conn, 401, "Unauthorized", {"error": "bad attach secret"})
            key = headers["sec-websocket-key"]
            accept = base64.b64encode(hashlib.sha1((key + GUID).encode()).digest()).decode()
            conn.sendall(
                ("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
                 f"Connection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").encode())
            with lock:
                attach_count += 1
                nth = attach_count
            close_code = CLOSES[min(nth - 1, len(CLOSES) - 1)]
            log({"ev": "attach", "session": session, "status": 101, "nth": nth, "will_close": close_code})
            drive_mcp(conn, session, nth)
            ws_close(conn, close_code)
            log({"ev": "closed", "session": session, "code": close_code, "nth": nth})
            return

        respond(conn, 404, "Not Found", {"error": "not found"})
    except Exception as e:
        log({"ev": "error", "err": repr(e)})
    finally:
        try:
            conn.close()
        except Exception:
            pass


def main():
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", PORT))
    srv.listen(8)
    log({"ev": "listening", "port": PORT, "closes": CLOSES, "preminted": sorted(secrets)})
    while True:
        c, a = srv.accept()
        threading.Thread(target=handle, args=(c, a), daemon=True).start()


if __name__ == "__main__":
    main()
