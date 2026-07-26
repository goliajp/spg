#!/usr/bin/env python3
"""v7.39 (round 504) — dump a MySQL server's raw wire packets for one query.

A real mariadb:11 client cannot read a result set from SPG's MySQL wire: any
row-returning SELECT, including `SELECT 1`, fails client-side with
`ERROR 2000 (HY000) Unknown or undefined error code`, while DDL and DML work
and return proper MySQL error codes. The server logs nothing, and SPG's own
mysqlwire e2e tests pass — so the divergence is between SPG's encoder and a
real client's reader, and only the bytes can say where.

This speaks the protocol by hand so the packets are visible, and takes the
same capability flags a real client sends, so the server makes the same
choices it makes for one.

  mysqlwire-packets.py <host> <port> <user> <pass> <db> "<sql>"
"""

import binascii
import hashlib
import os
import socket
import sys

CLIENT_LONG_PASSWORD = 0x00000001
CLIENT_FOUND_ROWS = 0x00000002
CLIENT_LONG_FLAG = 0x00000004
CLIENT_CONNECT_WITH_DB = 0x00000008
CLIENT_LOCAL_FILES = 0x00000080
CLIENT_PROTOCOL_41 = 0x00000200
CLIENT_TRANSACTIONS = 0x00002000
CLIENT_SECURE_CONNECTION = 0x00008000
CLIENT_MULTI_STATEMENTS = 0x00010000
CLIENT_MULTI_RESULTS = 0x00020000
CLIENT_PLUGIN_AUTH = 0x00080000
CLIENT_CONNECT_ATTRS = 0x00100000
CLIENT_PLUGIN_AUTH_LENENC_CLIENT_DATA = 0x00200000
CLIENT_DEPRECATE_EOF = 0x01000000


def recv_packet(sock):
    head = b""
    while len(head) < 4:
        chunk = sock.recv(4 - len(head))
        if not chunk:
            return None, None
        head += chunk
    length = head[0] | (head[1] << 8) | (head[2] << 16)
    seq = head[3]
    body = b""
    while len(body) < length:
        chunk = sock.recv(length - len(body))
        if not chunk:
            break
        body += chunk
    return seq, body


def send_packet(sock, seq, body):
    n = len(body)
    sock.sendall(bytes([n & 0xFF, (n >> 8) & 0xFF, (n >> 16) & 0xFF, seq]) + body)


def native_password(password, salt):
    if not password:
        return b""
    p = password.encode()
    stage1 = hashlib.sha1(p).digest()
    stage2 = hashlib.sha1(stage1).digest()
    token = hashlib.sha1(salt + stage2).digest()
    return bytes(a ^ b for a, b in zip(stage1, token))


def parse_handshake(body):
    """Return (salt, plugin) from an initial handshake packet."""
    i = 1  # protocol version
    end = body.index(b"\0", i)
    i = end + 1  # server version
    i += 4  # connection id
    salt = body[i : i + 8]
    i += 8 + 1  # + filler
    i += 2  # capability low
    if len(body) > i:
        i += 1  # charset
        i += 2  # status
        i += 2  # capability high
        salt_len = body[i]
        i += 1
        i += 10  # reserved
        more = max(13, salt_len - 8)
        salt += body[i : i + more - 1]
        i += more
        plugin = body[i:].split(b"\0")[0] if i < len(body) else b""
    else:
        plugin = b""
    return salt, plugin


def show(tag, seq, body):
    print(f"  [{tag}] seq={seq} len={len(body)}")
    print(f"        {binascii.hexlify(body).decode()}")
    printable = "".join(chr(c) if 32 <= c < 127 else "." for c in body)
    print(f"        {printable}")


def main():
    host, port, user, password, db, sql = sys.argv[1:7]
    sock = socket.create_connection((host, int(port)), timeout=10)

    seq, body = recv_packet(sock)
    print(f"== handshake from {host}:{port}")
    show("server-greeting", seq, body)
    salt, plugin = parse_handshake(body)
    print(f"        salt={binascii.hexlify(salt).decode()} plugin={plugin!r}")

    caps = (
        CLIENT_LONG_PASSWORD
        | CLIENT_FOUND_ROWS
        | CLIENT_LONG_FLAG
        | CLIENT_LOCAL_FILES
        | CLIENT_PROTOCOL_41
        | CLIENT_TRANSACTIONS
        | CLIENT_SECURE_CONNECTION
        | CLIENT_MULTI_STATEMENTS
        | CLIENT_MULTI_RESULTS
        | CLIENT_PLUGIN_AUTH
    )
    if db:
        caps |= CLIENT_CONNECT_WITH_DB
    # DEPRECATE_EOF=1 asks the server for the modern framing, so the two
    # branches can be measured against the same server.
    if os.environ.get("DEPRECATE_EOF") == "1":
        caps |= CLIENT_DEPRECATE_EOF
    print(f"        client caps = 0x{caps:08x} (deprecate_eof={bool(caps & CLIENT_DEPRECATE_EOF)})")

    auth = native_password(password, salt)
    resp = b""
    resp += caps.to_bytes(4, "little")
    resp += (16 * 1024 * 1024).to_bytes(4, "little")
    resp += bytes([45])  # utf8mb4_general_ci
    resp += b"\0" * 23
    resp += user.encode() + b"\0"
    resp += bytes([len(auth)]) + auth
    if db:
        resp += db.encode() + b"\0"
    resp += b"mysql_native_password\0"
    send_packet(sock, seq + 1, resp)

    seq, body = recv_packet(sock)
    show("auth-result", seq, body)
    if not body or body[0] != 0x00:
        print("  auth did not end in OK; stopping")
        return

    print(f"== COM_QUERY {sql!r}")
    send_packet(sock, 0, bytes([0x03]) + sql.encode())
    sock.settimeout(3)
    n = 0
    eofs = 0
    while n < 40:
        try:
            seq, body = recv_packet(sock)
        except socket.timeout:
            print("  (no more packets — server went quiet)")
            break
        if seq is None:
            print("  (connection closed by server)")
            break
        show(f"reply{n}", seq, body)
        n += 1
        # Without DEPRECATE_EOF a result set ends at its SECOND EOF: one
        # closes the column definitions, one closes the rows.
        want = 1 if os.environ.get("DEPRECATE_EOF") == "1" else 2
        if body and body[0] == 0xFE and len(body) < 60:
            eofs += 1
            if eofs == want:
                break
        if body and body[0] == 0xFF:
            break


if __name__ == "__main__":
    main()
