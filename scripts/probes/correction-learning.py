#!/usr/bin/env python3
"""Disposable GTK field for the ignored AT-SPI correction-learning probe.

The Rust test controls only this window through marker files, never the user's
keyboard or clipboard. Close it automatically when the test exits or times out.
"""
import sys
import json
import os
import subprocess
import time
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import GLib, Gtk

root = Path(sys.argv[1])
app = Gtk.Application(application_id="io.hex.CorrectionProbe")
started = time.monotonic()


def activate(application):
    window = Gtk.ApplicationWindow(application=application, title="Hex correction probe")
    window.set_default_size(560, 120)
    entry = Gtk.Entry()
    entry.set_text("")
    entry.set_position(0)
    window.set_child(entry)
    window.present()
    entry.grab_focus()
    seen = set()

    def step():
        if time.monotonic() - started > 25 or (root / "close").exists():
            application.quit()
            return False
        if time.monotonic() - started > 1 and "ready" not in seen:
            # Focus only this disposable window when testing under niri.
            if os.environ.get("NIRI_SOCKET"):
                result = subprocess.run(["niri", "msg", "-j", "windows"], capture_output=True, text=True, check=True)
                windows = json.loads(result.stdout)
                own = [w for w in windows if w.get("app_id") == "io.hex.CorrectionProbe"]
                if len(own) == 1:
                    subprocess.run(["niri", "msg", "action", "focus-window", "--id", str(own[0]["id"])], check=True)
            entry.select_region(0, 0)
            entry.set_position(0)
            seen.add("ready")
            (root / "ready").touch()
        for stage, text in [
            ("insert", "Ask cloud to review this diff."),
            ("correct", "Ask Claude to review this diff."),
        ]:
            if (root / stage).exists() and stage not in seen:
                entry.set_text(text)
                entry.set_position(len(text))
                seen.add(stage)
                (root / (stage + "-done")).touch()
        return True

    GLib.timeout_add(50, step)


app.connect("activate", activate)
app.run([])
