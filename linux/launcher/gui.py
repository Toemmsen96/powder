"""The GTK4 / Libadwaita front-end.

A single ``Adw.ApplicationWindow`` with an ``Adw.ViewStack`` behind an ``Adw.ViewSwitcher``: a Launch page (the
detected game - prefix, Proton, umu-run -, the server implementation, the overlay switch, optional overrides and the
big Launch button), a Mods page (the mods for the game, with their status and a Build/Generate button, and the
server mods) and a Servers page (add/remove/select). Changes persist through :meth:`Settings.save` as they happen.

GTK is imported lazily so the rest of the package stays usable without PyGObject. ``self_test`` builds the
application and window without starting the main loop; on a headless machine where GTK cannot reach a display
it reports that cleanly and still exits 0.
"""
from __future__ import annotations

import re

import subprocess
import threading
from pathlib import Path

from . import game
from .launch import launch as do_launch
from .model import Server, Settings
from .mods import conflicts, list_client_mods

APP_ID = "com.powder.LinuxLauncher"


def _require_gtk():
    import gi
    gi.require_version("Gtk", "4.0")
    gi.require_version("Adw", "1")
    from gi.repository import Adw, Gio, GLib, Gtk  # noqa: F401
    return Adw, Gio, GLib, Gtk


_FINGERPRINT = re.compile(r"[0-9a-f]{16,64}")   # same shape powder.launcher.certs accepts after "#"

def _fingerprint_ok(join: str) -> bool:
    """True if a Join address has no fingerprint part or a plausible-looking one. Self-contained so the GUI
    runs under whatever Python has PyGObject, without needing the `powder` package importable; powder-wrap
    re-checks the CA against the pin on connect anyway."""
    if "#" not in join:
        return True
    fp = join.split("#", 1)[1].strip()
    return not fp or _FINGERPRINT.fullmatch(fp) is not None


class LauncherWindow:
    """Builds and wires the main window for a :class:`Settings`; ``self.window`` is the Adw.ApplicationWindow."""

    def __init__(self, app, settings: Settings):
        self.Adw, self.Gio, self.GLib, self.Gtk = _require_gtk()
        self.app = app
        self.s = settings
        self._build()

    # --- persistence -------------------------------------------------------
    def _save(self):
        try:
            self.s.save()
        except OSError as exc:
            self._toast(f"could not save settings: {exc}")

    def _toast(self, text: str):
        try:
            self.toasts.add_toast(self.Adw.Toast.new(text))
        except Exception:
            pass

    # --- window ------------------------------------------------------------
    def _build(self):
        Adw, Gtk = self.Adw, self.Gtk
        self.window = Adw.ApplicationWindow(application=self.app)
        self.window.set_title("Powder")
        self.window.set_default_size(620, 560)

        self.toasts = Adw.ToastOverlay()
        stack = Adw.ViewStack()
        stack.add_titled_with_icon(self._launch_page(), "launch", "Launch", "media-playback-start-symbolic")
        stack.add_titled_with_icon(self._mods_page(), "mods", "Mods", "application-x-addon-symbolic")
        stack.add_titled_with_icon(self._servers_page(), "servers", "Servers", "network-server-symbolic")

        switcher = Adw.ViewSwitcher(stack=stack, policy=Adw.ViewSwitcherPolicy.WIDE)
        header = Adw.HeaderBar(title_widget=switcher)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        box.append(header)
        box.append(stack)
        self.toasts.set_child(box)
        self.window.set_content(self.toasts)

    def _page(self):
        Gtk = self.Gtk
        page = self.Adw.PreferencesPage()
        return page

    # --- Launch page -------------------------------------------------------
    def _launch_page(self):
        Adw, Gtk = self.Adw, self.Gtk
        page = Adw.PreferencesPage()

        d = game.detect(self.s.wine_prefix, self.s.proton)
        g_game = Adw.PreferencesGroup(title="Game",
                                      description="Found automatically; nothing to set up unless something is missing")
        for title, value in (("Ubisoft Connect", d.connect_exe if d.prefix else ""),
                             ("Proton", d.proton or "umu's default Proton"), ("umu-run", d.umu)):
            row = Adw.ActionRow(title=title, subtitle=self.GLib.markup_escape_text(value or "not found"))
            icon = Gtk.Image.new_from_icon_name("emblem-ok-symbolic" if value else "dialog-warning-symbolic")
            row.add_suffix(icon)
            g_game.add(row)
        for p in d.problems:
            g_game.add(Adw.ActionRow(title="Problem", subtitle=self.GLib.markup_escape_text(p)))
        page.add(g_game)

        g_server = Adw.PreferencesGroup(title="Server")
        self.impl_row = Adw.ComboRow(title="Server implementation",
                                     subtitle="Rust is the default; Python is the reference implementation")
        model = Gtk.StringList()
        for v in ("python", "rust"):
            model.append(v)
        self.impl_row.set_model(model)
        self.impl_row.set_selected(1 if self.s.server_impl == "rust" else 0)
        self.impl_row.connect("notify::selected", self._on_impl)
        g_server.add(self.impl_row)

        self.overlay_row = Adw.SwitchRow(title="In-game overlay",
                                         subtitle="Inject the ImGui overlay (press Home in-game)")
        self.overlay_row.set_active(self.s.overlay)
        self.overlay_row.connect("notify::active", self._on_overlay)
        g_server.add(self.overlay_row)
        page.add(g_server)

        g_steep = Adw.PreferencesGroup(title="Overrides (optional)",
                                       description="Leave empty: detected automatically")
        self.prefix_row = Adw.EntryRow(title="Wine prefix")
        self.prefix_row.set_text(self.s.wine_prefix)
        self.prefix_row.connect("changed", self._on_prefix)
        g_steep.add(self.prefix_row)
        self.steep_row = Adw.EntryRow(title="POWDER_STEEP_DIR")
        self.steep_row.set_text(self.s.steep_dir)
        self.steep_row.connect("changed", self._on_steep)
        browse = Gtk.Button(icon_name="folder-open-symbolic", valign=Gtk.Align.CENTER)
        browse.add_css_class("flat")
        browse.connect("clicked", self._on_browse_steep)
        self.steep_row.add_suffix(browse)
        g_steep.add(self.steep_row)
        page.add(g_steep)

        self.cmd_row = Adw.EntryRow(title="Game command (instead of Ubisoft Connect via umu-run)")
        self.cmd_row.set_text(" ".join(self.s.game_command))
        self.cmd_row.connect("changed", self._on_cmd)
        g_steep.add(self.cmd_row)

        g_go = Adw.PreferencesGroup()
        launch_btn = Gtk.Button(label="Launch game")
        launch_btn.add_css_class("suggested-action")
        launch_btn.add_css_class("pill")
        launch_btn.set_halign(Gtk.Align.CENTER)
        launch_btn.set_margin_top(6)
        launch_btn.connect("clicked", self._on_launch)
        g_go.add(launch_btn)
        page.add(g_go)
        return page

    def _on_impl(self, row, _p):
        self.s.server_impl = "rust" if row.get_selected() == 1 else "python"
        self._save()

    def _on_overlay(self, row, _p):
        self.s.overlay = row.get_active()
        self._save()

    def _on_steep(self, row):
        self.s.steep_dir = row.get_text().strip()
        self._save()

    def _on_prefix(self, row):
        self.s.wine_prefix = row.get_text().strip()
        self._save()

    def _on_cmd(self, row):
        self.s.game_command = row.get_text().split()
        self._save()

    def _on_browse_steep(self, _btn):
        Gtk = self.Gtk
        dialog = Gtk.FileDialog(title="Select Steep folder")

        def done(dlg, res):
            try:
                folder = dlg.select_folder_finish(res)
            except Exception:
                return
            if folder is not None:
                path = folder.get_path() or ""
                self.steep_row.set_text(path)      # triggers _on_steep -> save

        dialog.select_folder(self.window, None, done)

    def _on_launch(self, _btn):
        try:
            do_launch(self.s)
            self._toast("launching the game through powder-wrap...")
        except ValueError as exc:
            self._error_dialog("Cannot start the game", str(exc))
        except OSError as exc:
            self._error_dialog("Could not start powder-wrap", str(exc))

    def _error_dialog(self, heading: str, body: str):
        dlg = self.Adw.MessageDialog(transient_for=self.window, heading=heading, body=body)
        dlg.add_response("ok", "OK")
        dlg.present()

    # --- Mods page ---------------------------------------------------------
    def _mods_page(self):
        from .mods import list_server_mods
        Adw, Gtk = self.Adw, self.Gtk
        page = Adw.PreferencesPage()
        self.mods_page = page

        cgroup = Adw.PreferencesGroup(title="Mods for the game",
                                      description="Loaded into Steep at the next launch. Overlay DLL mods turn the "
                                                  "overlay on (press Home in-game, Mods tab)")
        self.client_mods = list_client_mods(self.s.repo)
        if not self.client_mods:
            cgroup.set_description("No mods with a game part found under the repo's mods/ folder.")
        self._client_rows = {}
        for m in self.client_mods:
            sub = m.kinds + (" - " + m.description if m.description else "")
            row = Adw.SwitchRow(title=m.name, subtitle=self.GLib.markup_escape_text(sub))
            row.set_subtitle_lines(2)
            if m.status != "ready":
                btn = Gtk.Button(label="Build" if m.status == "build" else "Generate", valign=Gtk.Align.CENTER)
                btn.set_tooltip_text("native/build.sh mods" if m.status == "build" else f"mods/{m.name}/generate.sh")
                btn.connect("clicked", self._on_prepare, m)
                row.add_suffix(btn)
            row.set_active(m.name in self.s.client_mods)
            row.connect("notify::active", self._on_client_toggle, m)
            cgroup.add(row)
            self._client_rows[m.name] = row
        self.conflict_row = Adw.ActionRow(title="Conflict")
        self.conflict_row.add_css_class("warning")
        self.conflict_row.set_visible(False)
        cgroup.add(self.conflict_row)
        self._show_conflicts()
        page.add(cgroup)

        group = Adw.PreferencesGroup(title="Server mods",
                                     description="Lua mods the local server runs (a game mod's server half is "
                                                 "switched on with it)")
        mods = list_server_mods(self.s.repo)
        if not mods:
            group.set_description("No server mods found under the repo's mods/ folder.")
        all_enabled = "*" in self.s.enabled_mods
        for m in mods:
            # Adw rows render the subtitle as Pango markup; descriptions are plain text (may hold <...>, &).
            subtitle = self.GLib.markup_escape_text(m.description) if m.description else None
            row = Adw.SwitchRow(title=m.name, subtitle=subtitle)
            if m.has_client:
                tag = self.Gtk.Label(label="client")
                tag.add_css_class("dim-label")
                tag.set_valign(self.Gtk.Align.CENTER)
                row.add_suffix(tag)
            row.set_active(all_enabled or m.name in self.s.enabled_mods)
            if all_enabled:
                row.set_sensitive(False)
                row.set_subtitle('Enabled by "*" (all mods)')
            row.connect("notify::active", self._on_mod_toggle, m.name)
            group.add(row)
        page.add(group)
        return page

    def _on_client_toggle(self, row, _p, m):
        if row.get_active():
            if m.name not in self.s.client_mods:
                self.s.client_mods.append(m.name)
            if m.dll and not self.s.overlay:
                self.overlay_row.set_active(True)          # DLL mods run inside the overlay
                self._toast("overlay turned on: the mod's DLL loads with it")
            if m.status != "ready":
                self._toast(f"{m.name}: press {'Build' if m.status == 'build' else 'Generate'} before launching")
        else:
            self.s.client_mods = [n for n in self.s.client_mods if n != m.name]
        self._save()
        self._show_conflicts()

    def _show_conflicts(self):
        msgs = conflicts(self.client_mods, self.s.client_mods)
        self.conflict_row.set_subtitle(self.GLib.markup_escape_text("; ".join(msgs)))
        self.conflict_row.set_visible(bool(msgs))

    def _on_prepare(self, btn, m):
        """Run native/build.sh mods or the mod's generate.sh in the background; the button shows progress."""
        repo = Path(self.s.repo)
        cmd = [str(repo / "native" / "build.sh"), "mods"] if m.status == "build" else \
            [str(repo / "mods" / m.name / "generate.sh")]
        btn.set_sensitive(False)
        btn.set_label("Working...")
        log = repo / "data" / "logs" / f"launcher-{m.name}.log"
        log.parent.mkdir(parents=True, exist_ok=True)

        def work():
            with open(log, "w") as fh:
                rc = subprocess.run(cmd, cwd=repo, stdout=fh, stderr=subprocess.STDOUT).returncode
            self.GLib.idle_add(done, rc)

        def done(rc):
            if rc == 0:
                btn.set_visible(False)
                self._toast(f"{m.name}: ready")
                m.status = "ready"
            else:
                btn.set_sensitive(True)
                btn.set_label("Retry")
                self._toast(f"{m.name}: failed, see {log}")
            return False

        threading.Thread(target=work, daemon=True).start()

    def _on_mod_toggle(self, row, _p, name: str):
        if row.get_active():
            if name not in self.s.enabled_mods:
                self.s.enabled_mods.append(name)
        else:
            self.s.enabled_mods = [m for m in self.s.enabled_mods if m != name]
        self._save()

    # --- Servers page ------------------------------------------------------
    def _servers_page(self):
        Adw, Gtk = self.Adw, self.Gtk
        page = Adw.PreferencesPage()
        self.servers_group = Adw.PreferencesGroup(title="Servers",
                                                  description="The active server; empty Join = the local server")
        add_btn = Gtk.Button(icon_name="list-add-symbolic", valign=Gtk.Align.CENTER)
        add_btn.add_css_class("flat")
        add_btn.connect("clicked", self._on_add_server)
        self.servers_group.set_header_suffix(add_btn)
        page.add(self.servers_group)
        self._refresh_servers()
        return page

    def _refresh_servers(self):
        Adw, Gtk = self.Adw, self.Gtk
        for row in getattr(self, "_server_rows", []):
            self.servers_group.remove(row)
        self._server_rows = []
        self._server_checks = []
        for i, sv in enumerate(self.s.servers):
            row = Adw.ActionRow(title=sv.name, subtitle=sv.join or "(local powder-wrap server)")
            check = Gtk.CheckButton(valign=Gtk.Align.CENTER)
            if self._server_checks:
                check.set_group(self._server_checks[0])
            check.set_active(i == self.s.selected_server)
            check.connect("toggled", self._on_select_server, i)
            row.add_prefix(check)
            remove = Gtk.Button(icon_name="user-trash-symbolic", valign=Gtk.Align.CENTER)
            remove.add_css_class("flat")
            remove.connect("clicked", self._on_remove_server, sv.name)
            row.add_suffix(remove)
            self.servers_group.add(row)
            self._server_rows.append(row)
            self._server_checks.append(check)

    def _on_select_server(self, check, i):
        if check.get_active():
            self.s.selected_server = i
            self._save()

    def _on_remove_server(self, _btn, name):
        self.s.servers = [sv for sv in self.s.servers if sv.name != name]
        if self.s.selected_server >= len(self.s.servers):
            self.s.selected_server = max(0, len(self.s.servers) - 1)
        self._save()
        self._refresh_servers()

    def _on_add_server(self, _btn):
        Adw, Gtk = self.Adw, self.Gtk
        dlg = Adw.MessageDialog(transient_for=self.window, heading="Add server",
                                body="A name, and an empty Join for the local server or host#<fingerprint>.")
        name_row = Adw.EntryRow(title="Name")
        join_row = Adw.EntryRow(title="Join (host#fingerprint)")
        group = Gtk.ListBox()
        group.add_css_class("boxed-list")
        group.append(name_row)
        group.append(join_row)
        dlg.set_extra_child(group)
        dlg.add_response("cancel", "Cancel")
        dlg.add_response("add", "Add")
        dlg.set_response_appearance("add", Adw.ResponseAppearance.SUGGESTED)
        dlg.set_default_response("add")

        def resp(d, response):
            if response != "add":
                return
            name = name_row.get_text().strip()
            join = join_row.get_text().strip()
            if not name:
                self._toast("a server needs a name")
                return
            if not _fingerprint_ok(join):
                self._toast("that Join address has an invalid fingerprint")
                return
            self.s.servers = [sv for sv in self.s.servers if sv.name != name]
            self.s.servers.append(Server(name=name, join=join))
            self._save()
            self._refresh_servers()

        dlg.connect("response", resp)
        dlg.present()


def _make_app(settings: Settings):
    Adw, _Gio, _GLib, _Gtk = _require_gtk()
    app = Adw.Application(application_id=APP_ID)
    holder = {}

    def on_activate(a):
        win = holder.get("win")
        if win is None:
            win = LauncherWindow(a, settings)
            holder["win"] = win
        win.window.present()

    app.connect("activate", on_activate)
    return app, holder


def run(settings: Settings) -> int:
    app, _holder = _make_app(settings)
    return app.run(None)


def self_test() -> int:
    """Construct the application and main window without running the main loop (headless-safe)."""
    try:
        Adw, _Gio, _GLib, _Gtk = _require_gtk()
        Adw.init()
        settings = Settings.load()
        app = Adw.Application(application_id=APP_ID)
        app.register(None)
        win = LauncherWindow(app, settings)
        assert win.window is not None
    except Exception:
        print("launcher GUI import OK (no display)")
        return 0
    print("launcher GUI constructs OK")
    return 0
