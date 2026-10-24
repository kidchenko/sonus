# A stand-in MPRIS player on the session bus, as org.mpris.MediaPlayer2.spotify,
# for test-live.sh: a track with a cover, and a log of the calls it gets.
import struct, zlib
from gi.repository import Gio, GLib

def png(w, h, rgb):
    raw = b"".join(b"\0" + bytes(rgb) * w for _ in range(h))
    def chunk(t, d):
        return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw)) + chunk(b"IEND", b""))
open("/tmp/cover.png", "wb").write(png(300, 200, (200, 40, 90)))

XML = """<node>
 <interface name="org.mpris.MediaPlayer2.Player">
  <method name="PlayPause"/><method name="Next"/><method name="Previous"/>
  <method name="SetPosition"><arg type="o" direction="in"/><arg type="x" direction="in"/></method>
  <method name="Seek"><arg type="x" direction="in"/></method>
 </interface>
 <interface name="org.freedesktop.DBus.Properties">
  <method name="GetAll"><arg type="s" direction="in"/><arg type="a{sv}" direction="out"/></method>
 </interface>
</node>"""
state = {"status": "Playing", "pos": 83_000_000}

def props():
    meta = {
        "mpris:trackid": GLib.Variant("o", "/com/spotify/track/abc"),
        "xesam:title": GLib.Variant("s", "Relax, Take It Easy"),
        "xesam:artist": GLib.Variant("as", ["Mika"]),
        "mpris:length": GLib.Variant("t", 240_000_000),
        "mpris:artUrl": GLib.Variant("s", "file:///tmp/cover.png"),
    }
    b = lambda v: GLib.Variant("b", v)
    return {
        "PlaybackStatus": GLib.Variant("s", state["status"]),
        "Metadata": GLib.Variant("a{sv}", meta),
        "Position": GLib.Variant("x", state["pos"]),
        "CanControl": b(True), "CanPlay": b(True), "CanPause": b(True),
        "CanGoNext": b(True), "CanGoPrevious": b(False), "CanSeek": b(True),
    }

def call(conn, sender, path, iface, method, params, inv):
    if method == "GetAll":
        inv.return_value(GLib.Variant("(a{sv})", (props(),)))
        return
    if method == "PlayPause":
        state["status"] = "Paused" if state["status"] == "Playing" else "Playing"
    if method == "SetPosition":
        state["pos"] = params.unpack()[1]
    print("CALLED", method, params.unpack() if params else "", flush=True)
    inv.return_value(None)

info = Gio.DBusNodeInfo.new_for_xml(XML)
def on_bus(conn, name):
    for i in info.interfaces:
        conn.register_object("/org/mpris/MediaPlayer2", i, call, None, None)
Gio.bus_own_name(Gio.BusType.SESSION, "org.mpris.MediaPlayer2.spotify", 0, on_bus, None, None)
GLib.MainLoop().run()
