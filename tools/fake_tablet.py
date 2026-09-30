"""Stand-in for the tablet app, to test the host without a tablet.

Run the host with `--no-adb` (and on Linux optionally `--test-source`), then this script.
It streams for ~6 s, stops acking for one second (the host must pause after 2 frames),
asks for a keyframe, and prints what arrived. With PyAV installed it also decodes the stream.
"""
import socket, struct, time, threading, io, statistics
W,H,FPS = 1280,800,60
s = socket.create_connection(("127.0.0.1", 27183))
s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
s.sendall(b"TDSP" + bytes([2]) + struct.pack(">III", W, H, FPS))
f = s.makefile("rb")
def rd(n):
    b = f.read(n)
    if len(b) < n: raise EOFError
    return b
lock = threading.Lock(); owed = [0]
def ack():
    s.sendall(bytes([2,0]))
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
        if kind != 2: continue
        au = body[8:]; now = time.time()-t0
        nals = [au[i+3] & 0x1f for i in range(len(au)-3) if au[i:i+3]==b"\0\0\1"]
        log.append((now, 5 in nals, len(au))); stream += au
        with lock:
            if 2.0 < now and owed[0] >= 0: owed[0] += 1; continue
        ack()
except (EOFError, OSError): pass
stall = [t for t,_,_ in log if 2.0 < t < 3.0]
keys = [round(t,2) for t,k,_ in log if k]
print(f"frames={len(log)} keyframes_at={keys} frames_during_1s_stall={len(stall)}")
gaps=[b[0]-a[0] for a,b in zip(log,log[1:]) if b[0] < 2.0]
print(f"mean gap before stall {statistics.mean(gaps)*1000:.1f} ms")
try:
    import av
    dec = sum(1 for _ in av.open(io.BytesIO(bytes(stream)), format="h264").decode(video=0))
    print("decoded", dec, "of", len(log))
except ImportError:
    print("install PyAV (pip install av) to also check that every frame decodes")
