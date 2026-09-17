#!/usr/bin/env python3
"""Empirical probe of org.freedesktop.portal.InputCapture on this session.

Answers, on the actual running compositor:
  1. Does CreateSession trigger a user consent dialog?
  2. What zones (screen geometry) does the compositor report?
  3. Does a second run re-prompt (persistence behavior)?

Read-only with respect to input: no barriers are set, nothing is captured.
"""

import sys
import time

import gi
gi.require_version("Gio", "2.0")
from gi.repository import Gio, GLib

PORTAL_BUS = "org.freedesktop.portal.Desktop"
PORTAL_PATH = "/org/freedesktop/portal/desktop"
IFACE = "org.freedesktop.portal.InputCapture"
KEYBOARD, POINTER = 1, 2


class Probe:
    def __init__(self):
        self.bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        self.loop = GLib.MainLoop()
        self.sender = self.bus.get_unique_name()[1:].replace(".", "_")
        self.counter = 0
        self.response = None

    def request(self, method, args_builder):
        """Portal request pattern: call, then wait for the Response signal."""
        self.counter += 1
        token = f"bwprobe{self.counter}"
        req_path = f"/org/freedesktop/portal/desktop/request/{self.sender}/{token}"
        self.response = None

        def on_response(bus, sender, path, iface, signal, params):
            self.response = params.unpack()
            self.loop.quit()

        sub = self.bus.signal_subscribe(
            None, "org.freedesktop.portal.Request", "Response",
            req_path, None, Gio.DBusSignalFlags.NONE, on_response)

        params = args_builder(token)
        t0 = time.monotonic()
        ret = self.bus.call_sync(PORTAL_BUS, PORTAL_PATH, IFACE, method, params,
                                 None, Gio.DBusCallFlags.NONE, 30000, None)
        actual_path = ret.unpack()[0]
        if actual_path != req_path:
            print(f"  (request path {actual_path} != expected {req_path})")
        # 60s budget: a consent dialog waits for a human.
        GLib.timeout_add_seconds(60, self.loop.quit)
        self.loop.run()
        elapsed = time.monotonic() - t0
        self.bus.signal_unsubscribe(sub)
        if self.response is None:
            print(f"  {method}: NO RESPONSE within 60s")
            sys.exit(1)
        code, results = self.response
        print(f"  {method}: response={code} in {elapsed:.2f}s -> {results}")
        if code != 0:
            print("  (1 = user cancelled, 2 = other error)")
            sys.exit(1)
        return results, elapsed


def main():
    p = Probe()
    print(f"probe connected to session bus as :{p.sender.replace('_', '.')}")

    def create_args(token):
        return GLib.Variant("(sa{sv})", ("", {
            "handle_token": GLib.Variant("s", token),
            "session_handle_token": GLib.Variant("s", f"bwsession{p.counter}"),
            "capabilities": GLib.Variant("u", KEYBOARD | POINTER),
        }))

    print("CreateSession (watch the screen for a consent dialog)...")
    results, elapsed = p.request("CreateSession", create_args)
    session = results["session_handle"]
    caps = results.get("capabilities")
    print(f"  session={session} granted_capabilities={caps}")
    dialog_likely = elapsed > 1.5
    print(f"  human-time elapsed: {elapsed:.2f}s -> dialog "
          f"{'LIKELY SHOWN' if dialog_likely else 'probably NOT shown'}")

    def zones_args(token):
        return GLib.Variant("(oa{sv})", (session, {
            "handle_token": GLib.Variant("s", token),
        }))

    print("GetZones...")
    results, _ = p.request("GetZones", zones_args)
    for z in results.get("zones", []):
        print(f"  zone: {z}")

    print("probe complete (no capture attempted)")


if __name__ == "__main__":
    main()
