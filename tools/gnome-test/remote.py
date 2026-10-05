# What xdg-desktop-portal-gnome does for a RemoteDesktop + ScreenCast portal session: a mutter
# RemoteDesktop session with a virtual-monitor screencast attached, started together. Prints the
# PipeWire node id, then moves the pointer through the stream like the portal does.
import sys, time, dbus, threading
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib
DBusGMainLoop(set_as_default=True)
bus = dbus.SessionBus()
M = "org.gnome.Mutter"
rd = dbus.Interface(bus.get_object(f"{M}.RemoteDesktop", "/org/gnome/Mutter/RemoteDesktop"), f"{M}.RemoteDesktop")
rd_path = rd.CreateSession()
rd_obj = bus.get_object(f"{M}.RemoteDesktop", rd_path)
rd_session = dbus.Interface(rd_obj, f"{M}.RemoteDesktop.Session")
session_id = rd_obj.Get(f"{M}.RemoteDesktop.Session", "SessionId", dbus_interface="org.freedesktop.DBus.Properties")
sc = dbus.Interface(bus.get_object(f"{M}.ScreenCast", "/org/gnome/Mutter/ScreenCast"), f"{M}.ScreenCast")
sc_path = sc.CreateSession(dbus.Dictionary({"remote-desktop-session-id": session_id}, signature="sv"))
sc_session = dbus.Interface(bus.get_object(f"{M}.ScreenCast", sc_path), f"{M}.ScreenCast.Session")
cursor = int(sys.argv[1]) if len(sys.argv) > 1 else 1
stream_path = sc_session.RecordVirtual(dbus.Dictionary({"cursor-mode": dbus.UInt32(cursor)}, signature="sv"))
def added(node):
    print("node", int(node), flush=True)
    def wiggle():
        time.sleep(8)
        for i in range(200):
            x, y = 100 + (i * 37) % 1100, 100 + (i * 23) % 600
            try:
                rd_session.NotifyPointerMotionAbsolute(stream_path, float(x), float(y))
            except dbus.DBusException as e:
                print("pointer failed:", e.get_dbus_message(), flush=True)
                return
            time.sleep(0.02)
        print("pointer ok", flush=True)
    if "--still" not in sys.argv:
        threading.Thread(target=wiggle, daemon=True).start()
bus.add_signal_receiver(added, signal_name="PipeWireStreamAdded", dbus_interface=f"{M}.ScreenCast.Stream", path=stream_path)
rd_session.Start()
GLib.MainLoop().run()
