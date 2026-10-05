#!/bin/sh
# Inside the container: PipeWire, headless GNOME mutter, a remote desktop + virtual-monitor
# screencast made the way GNOME's portal makes it, and the Linux file capturing it for a
# stand-in tablet. (Software rendering: what a real GPU does differently is not covered.)
export XDG_RUNTIME_DIR=/tmp/xdg; mkdir -p $XDG_RUNTIME_DIR; chmod 700 $XDG_RUNTIME_DIR
pipewire >/tmp/pw.log 2>&1 &
sleep 1; wireplumber >/tmp/wp.log 2>&1 &
sleep 1
${COMPOSITOR:-gnome-shell} --headless --wayland --no-x11 --virtual-monitor 1280x800 >/tmp/mutter.log 2>&1 &
sleep ${SETTLE:-12}
python3 /test/remote.py ${CURSOR:-1} ${STILL:-} >/tmp/cast.log 2>&1 &
for i in $(seq 1 20); do grep -q node /tmp/cast.log && break; sleep 0.5; done
node=$(awk '/node/{print $2}' /tmp/cast.log)
[ -n "$node" ] || { echo "no screencast"; tail -5 /tmp/mutter.log /tmp/cast.log; exit 1; }
# Something that changes on the virtual monitor (the pointer is there, so the window opens there).
if [ -n "${ANIMATE:-}" ]; then
  sleep 9
  WAYLAND_DISPLAY=wayland-0 gst-launch-1.0 -q videotestsrc pattern=ball is-live=true ! video/x-raw,width=640,height=400,framerate=30/1 ! waylandsink >/tmp/client.log 2>&1 &
fi
${BIN:-/src/dist/tabdisplay-linux-x86_64} --no-adb --pipewire-node "$node" --fps 90 >/tmp/host.log 2>&1 &
sleep 6
W=2800 H=1840 TILES=1 timeout 15 python3 /src/tools/fake_tablet.py >/tmp/fake.log 2>&1
kill -INT $! 2>/dev/null; sleep 1
grep -E "tiles=|frames=" /tmp/fake.log
grep -E "cannot be read|capture failed|no picture|CRITICAL" /tmp/host.log | head -5
grep "latency ms" /tmp/host.log | tail -1 | sed "s/.*total/total/"
grep -E "pointer" /tmp/cast.log
grep -q "keyframes_at=\[[0-9]" /tmp/fake.log && grep -q "pointer ok" /tmp/cast.log && ! grep -q "cannot be read" /tmp/host.log
