"""Black-box checks against a packaged Linework.exe on a never-switched Windows desktop."""

import argparse
from concurrent.futures import ThreadPoolExecutor
import ctypes
from ctypes import wintypes as wt
import json
import os
import re
from pathlib import Path
import subprocess
import tempfile
import time
import uuid

import win32api
import win32con as c
import win32event
import win32gui as g
import win32process
import win32ui
from PIL import Image

U = ctypes.WinDLL("user32", use_last_error=True)
U.CreateDesktopW.argtypes = [
    wt.LPCWSTR,
    wt.LPCWSTR,
    ctypes.c_void_p,
    wt.DWORD,
    wt.DWORD,
    ctypes.c_void_p,
]
U.CreateDesktopW.restype = wt.HANDLE
U.CloseDesktop.argtypes = [wt.HANDLE]
U.CloseDesktop.restype = wt.BOOL
U.SetThreadDesktop.argtypes = [wt.HANDLE]
U.SetThreadDesktop.restype = wt.BOOL
U.GetThreadDesktop.argtypes = [wt.DWORD]
U.GetThreadDesktop.restype = wt.HANDLE
U.EnumDesktopWindows.argtypes = [wt.HANDLE, ctypes.c_void_p, wt.LPARAM]
U.EnumDesktopWindows.restype = wt.BOOL
CALLBACK = ctypes.WINFUNCTYPE(wt.BOOL, wt.HWND, wt.LPARAM)
msg = g.SendMessage
post = g.PostMessage


def wait(what, fn, timeout=15):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        try:
            value = fn()
        except g.error as exc:
            if exc.winerror not in (1400, 1421):
                raise
            value = None
        except PermissionError:
            value = None
        if value:
            return value
        time.sleep(0.025)
    raise AssertionError("Timed out waiting for " + what)


def verify(name, actual, expected):
    if actual != expected:
        raise AssertionError(f"{name}: expected {expected!r}, got {actual!r}")
    print(f"PASS {name}: {actual!r}", flush=True)


class App:
    def __init__(self, exe, fixture, profile, arguments):
        self.name = "LineworkPrivate" + uuid.uuid4().hex
        self.desktop = U.CreateDesktopW(self.name, None, None, 0, 0x01FF, None)
        if not self.desktop:
            raise ctypes.WinError(ctypes.get_last_error())
        self.old_desktop = U.GetThreadDesktop(win32api.GetCurrentThreadId())
        if not U.SetThreadDesktop(self.desktop):
            U.CloseDesktop(self.desktop)
            raise ctypes.WinError(ctypes.get_last_error())
        self.proc = None
        try:
            startup = win32process.STARTUPINFO()
            startup.lpDesktop = self.name
            env = {**os.environ, "LINEWORK_PROFILE": str(profile)}
            args = subprocess.list2cmdline([str(exe), *map(str, arguments)])
            self.proc, thread, self.pid, _ = win32process.CreateProcess(
                str(exe),
                args,
                None,
                None,
                False,
                c.CREATE_UNICODE_ENVIRONMENT,
                env,
                str(fixture),
                startup,
            )
            thread.Close()
            self.h = wait("packaged main window", lambda: self.find("LineworkMain"))
            self.ed = wait("native editing control", lambda: g.GetDlgItem(self.h, 700))
            self.side = wait("workspace sidebar", lambda: g.GetDlgItem(self.h, 601))
        except Exception:
            self.close()
            raise

    def find(self, cls, title=None):
        results = []

        @CALLBACK
        def record(h, unused):
            if (
                win32process.GetWindowThreadProcessId(h)[1] == self.pid
                and g.GetClassName(h) == cls
            ):
                if title is None or title == g.GetWindowText(h):
                    results.append(h)
            return True

        U.EnumDesktopWindows(self.desktop, record, 0)
        return next(iter(results), 0)

    def close(self):
        try:
            if self.proc:
                if win32event.WaitForSingleObject(self.proc, 0) == c.WAIT_TIMEOUT:
                    try:
                        win32api.TerminateProcess(self.proc, 0)
                    except g.error as e:
                        if (
                            e.winerror != 5
                            or win32event.WaitForSingleObject(self.proc, 5000)
                            != c.WAIT_OBJECT_0
                        ):
                            raise
                win32event.WaitForSingleObject(self.proc, 5000)
                self.proc.Close()
        finally:
            U.SetThreadDesktop(self.old_desktop)
            U.CloseDesktop(self.desktop)

    def key(self, f, hwnd=None):
        hwnd = hwnd or self.ed
        post(hwnd, c.WM_KEYDOWN, 0x6F + f, 1)
        post(hwnd, c.WM_KEYUP, 0x6F + f, 1)

    def text(self, max_bytes=600):
        n = min(msg(self.ed, 2006, 0, 0), max_bytes)
        return bytes([msg(self.ed, 2007, i, 0) & 255 for i in range(n)]).decode(
            "utf8", "replace"
        )

    def field(self, dialog, control, value):
        field = wait(f"dialog field {control}", lambda: g.GetDlgItem(dialog, control))
        assert msg(field, c.WM_SETTEXT, 0, value)

    def accept(self, dialog):
        post(dialog, c.WM_COMMAND, 1, 0)
        wait("dialog close", lambda: not g.IsWindow(dialog))

    def ready(self, name):
        wait("open tab " + name, lambda: name in g.GetWindowText(self.h))

    def rows(self):
        return msg(self.side, c.LB_GETCOUNT, 0, 0)
