"""Stand-in for the tablet app, to test the host without a tablet.

Run the host with `--no-adb` (and on Linux optionally `--test-source`), then this script.
It streams for ~6 s, stops acking for one second (the host must pause after 2 frames),
asks for a keyframe, and prints what arrived. With PyAV installed it also decodes the stream.
"""
import socket, struct, time, threading, io, statistics
import os
W,H,FPS = int(os.environ.get("W",1280)),int(os.environ.get("H",800)),60
s = socket.create_connection(("127.0.0.1", 27183))
s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
TILES = os.environ.get("TILES") == "1"  # also take small changes as pixel tiles
s.sendall(b"TDSP" + bytes([5]) + struct.pack(">III", W, H, FPS) + bytes([1 if TILES else 0]))
_log = "fake tablet: hello from the log channel".encode()
s.sendall(bytes([8, 0]) + struct.pack(">H", len(_log)) + _log)
def lz4(src, size):
    """LZ4 block decoder (what the tablet's native code does), to check every tile unpacks."""
    out = bytearray(); i = 0
    while i < len(src):
        tok = src[i]; i += 1; n = tok >> 4
        if n == 15:
            while True:
                b = src[i]; i += 1; n += b
                if b != 255: break
        out += src[i:i+n]; i += n
        if i >= len(src): break
        off = src[i] | src[i+1] << 8; i += 2; m = (tok & 15) + 4
        if (tok & 15) == 15:
            while True:
                b = src[i]; i += 1; m += b
                if b != 255: break
        for _ in range(m): out.append(out[-off])
    if len(out) != size: raise ValueError(f"tile unpacked to {len(out)} bytes, expected {size}")
    return out
tiles = []
f = s.makefile("rb")
def rd(n):
    b = f.read(n)
    if len(b) < n: raise EOFError
    return b
lock = threading.Lock(); owed = [0]
def ack():
    # a pointer move and a scroll before every ack also exercise the variable-length framing
    s.sendall(bytes([4,0]) + struct.pack(">HH", 30000, 20000) + bytes([5,0]) + struct.pack(">hh", -3, 7) + bytes([2,0]))
stream = bytearray(); log=[]; t0=time.time()
def releaser():
    time.sleep(3.0)
    with lock:
        n = owed[0]; owed[0] = -1
    for _ in range(n): ack()
    time.sleep(1.2); s.sendall(bytes([3,0]))  # ask for a keyframe at ~4.2s
    time.sleep(1.5); s.shutdown(socket.SHUT_RDWR)
threading.Thread(target=releaser, daemon=True).start()
try:
    while True:
        kind = rd(1)[0]; ln = struct.unpack(">I", rd(4))[0]; body = rd(ln)
        if kind == 1: print("config", struct.unpack(">IIII", body[:16]), body[16]); continue
        if kind == 6:  # ping: answer at once (host time, our time) for the host's clock sync
            s.sendall(bytes([7,0]) + body[:8] + struct.pack(">Q", time.monotonic_ns() // 1000)); continue
        if kind == 7 and TILES:  # tile: pts, x, y, w, h, luma length, LZ4 Y, LZ4 CbCr
            pts, x, y, w, h, yl = struct.unpack(">QHHHHI", body[:20])
            lz4(body[20:20+yl], w*h); lz4(body[20+yl:], w*h//2)
            assert x + w <= W and y + h <= H, (x, y, w, h)
            tiles.append((time.time()-t0, w*h, ln))
            t = time.monotonic_ns() // 1000
            s.sendall(bytes([6,0]) + body[:8] + struct.pack(">QQQQQ", t, t, t, t, t))
            with lock:
                if 2.0 < time.time()-t0 and owed[0] >= 0: owed[0] += 1; continue
            ack(); continue
        if kind != 2: continue
        au = body[16:]; now = time.time()-t0  # after pts and the changed area
        t = time.monotonic_ns() // 1000  # "shown" right away: the host prints its side of the latency
        s.sendall(bytes([6,0]) + body[:8] + struct.pack(">QQQQQ", t, t, t, t, t))
        nals = [au[i+3] & 0x1f for i in range(len(au)-3) if au[i:i+3]==b"\0\0\1"]
        log.append((now, 5 in nals, len(au))); stream += au
        with lock:
            if 2.0 < now and owed[0] >= 0: owed[0] += 1; continue
        ack()
except (EOFError, OSError): pass
if TILES:
    print(f"tiles={len(tiles)} (all unpacked to the right size), mean area {statistics.mean([a for _,a,_ in tiles]) if tiles else 0:.0f} px, "
          f"mean {statistics.mean([b for *_,b in tiles])/1024 if tiles else 0:.1f} KB; during 1s stall: {len([1 for t,_,_ in tiles if 2.0 < t < 3.0])}")
stall = [t for t,_,_ in log if 2.0 < t < 3.0]
keys = [round(t,2) for t,k,_ in log if k]
print(f"frames={len(log)} keyframes_at={keys} frames_during_1s_stall={len(stall)}")
gaps=[b[0]-a[0] for a,b in zip(log,log[1:]) if b[0] < 2.0]
if gaps: print(f"mean gap before stall {statistics.mean(gaps)*1000:.1f} ms")
if os.environ.get("OUT"): open(os.environ["OUT"], "wb").write(stream)
try:
    import av
    dec = sum(1 for _ in av.open(io.BytesIO(bytes(stream)), format="h264").decode(video=0))
    print("decoded", dec, "of", len(log))
except ImportError:
    print("install PyAV (pip install av) to also check that every frame decodes")
