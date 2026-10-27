"""Powder: Steep items in Blender (a Blender add-on; Edit > Preferences > Add-ons > Install from Disk).

A "Powder" tab in the 3D view's sidebar:
- Import item: exports an item (archive + entry) with tools/forge/mesh.py to glTF and imports it: the armature
  (stick bones), every LOD (LOD1..3 hidden), weights, materials and the diffuse texture.
- Write mod: exports what you changed to glTF and writes an asset mod, mods/<name>/client/forge/<Base>/, for
  every datafile that holds the item's meshes (tools/forge/forge.py where), plus mod.toml. powder-wrap loads it
  when the mod is in POWDER_CLIENT_MODS.

The tools run in Blender's own Python (it has zstandard and numpy), so nothing else is needed; the Powder
repository and the Steep folder are set in the add-on's preferences. Archives are only read. The workflow and the
rules (names, materials, bones) are in tools/forge/BLENDER.md.
"""

import glob
import os
import re
import subprocess
import sys
import tempfile

import bpy
from bpy.props import BoolProperty, EnumProperty, PointerProperty, StringProperty

bl_info = {
    "name": "Powder: Steep models",
    "author": "Powder",
    "version": (0, 1, 0),
    "blender": (4, 2, 0),
    "location": "View3D > Sidebar > Powder",
    "description": "Import Steep items (armature, weights, textures) and write them back as Powder asset mods",
    "category": "Import-Export",
}

DEFAULT_GAME = "~/Games/ubisoft-connect/drive_c/Program Files (x86)/Ubisoft/Ubisoft Game Launcher/games/Steep"
LOG = "Powder log"


class _Defaults:  # the add-on run without being installed (e.g. from a script)
    repo = "~/Documents/GitHub/powder"
    game = DEFAULT_GAME


def prefs(context):
    a = context.preferences.addons.get(__name__)
    return a.preferences if a else _Defaults


def paths(context) -> tuple[str, str]:
    p = prefs(context)
    repo, game = os.path.expanduser(p.repo), os.path.expanduser(p.game)
    if not os.path.isfile(os.path.join(repo, "tools", "forge", "mesh.py")):
        raise RuntimeError(f"no tools/forge/mesh.py in {repo} (set the Powder repository in the add-on's preferences)")
    if not os.path.isdir(game):
        raise RuntimeError(f"no Steep folder at {game} (set it in the add-on's preferences)")
    return repo, game


def log(text: str):
    t = bpy.data.texts.get(LOG) or bpy.data.texts.new(LOG)
    t.write(text.rstrip() + "\n")


def run(repo: str, script: str, *args: str) -> str:
    """A tools/forge script in Blender's Python; its output goes to the "Powder log" text."""
    cmd = [sys.executable, "-I", os.path.join(repo, "tools", "forge", script), *args]
    log("$ " + " ".join(f"'{c}'" if " " in c else c for c in cmd[2:]))
    p = subprocess.run(cmd, capture_output=True, text=True)
    log(p.stdout + p.stderr)
    if p.returncode:
        raise RuntimeError((p.stderr or p.stdout).strip().splitlines()[-1] if (p.stderr or p.stdout).strip()
                           else f"{script} failed ({p.returncode})")
    return p.stdout


_archives: list = []  # EnumProperty items must stay referenced


def archive_items(self, context):
    _archives.clear()
    try:
        game = os.path.expanduser(prefs(context).game)
        names = sorted(os.path.basename(p) for p in glob.glob(os.path.join(game, "DataPC*.forge"))
                       if not re.search(r"_patch_\d+\.forge$", p))
    except Exception:
        names = []
    _archives.extend((n, n[:-6], f"{n} and its patches") for n in names)
    return _archives or [("", "(no archives: set the Steep folder)", "")]


class PowderPrefs(bpy.types.AddonPreferences):
    bl_idname = __name__
    repo: StringProperty(name="Powder repository", subtype="DIR_PATH", default="~/Documents/GitHub/powder")
    game: StringProperty(name="Steep folder", subtype="DIR_PATH", default=DEFAULT_GAME)

    def draw(self, context):
        self.layout.prop(self, "repo")
        self.layout.prop(self, "game")


class PowderSettings(bpy.types.PropertyGroup):
    archive: EnumProperty(name="Archive", items=archive_items)
    entry: StringProperty(name="Item", default="STP_M_Jacket_001",
                          description="Entry name or id (0x...), e.g. STP_M_Jacket_001")
    lod0_only: BoolProperty(name="LOD0 only", default=False)
    textures: BoolProperty(name="Textures", default=True, description="Embed each material's diffuse texture")
    rig: StringProperty(name="Extra bones from", default="",
                        description="Items whose bones join the armature (comma-separated), e.g. STP_M_Pants_001 "
                                    "for the legs")
    mod_name: StringProperty(name="Mod", default="my_mod", description="mods/<name> in the Powder repository")
    mode: EnumProperty(name="Mode", items=[
        ("REPLACE", "Replace the item", "The item's visible LOD objects replace their meshes (hidden ones stay)"),
        ("ADD", "Add selected pieces", "The selected objects are added to the item's meshes, which keep theirs")])
    into: EnumProperty(name="Into", items=[
        ("NAME", "Each LOD by name", "An object goes into the LOD it is named after"),
        ("ALL", "Every LOD", "Every exported object goes into all LODs"),
        ("LOD0", "LOD0 only", "Every exported object goes into LOD0")])


def tidy(objs):
    for o in objs:
        if o.type == "ARMATURE":
            for pb in o.pose.bones:
                pb.custom_shape = None
            o.data.display_type = "STICK"
        elif o.type == "MESH" and any(c.name.startswith("glTF_not_exported") for c in o.users_collection):
            o.hide_set(True)  # the importer's bone shape
        elif o.type == "MESH" and re.search(r"_LOD[1-9]$", o.name):
            o.hide_set(True)


def object_mode(context):
    if context.mode != "OBJECT" and context.view_layer.objects.active:
        bpy.ops.object.mode_set(mode="OBJECT")


class POWDER_OT_import_item(bpy.types.Operator):
    bl_idname = "powder.import_item"
    bl_label = "Import item"
    bl_description = "Export the item from the game's archives to glTF and import it here"
    bl_options = {"REGISTER", "UNDO"}

    def execute(self, context):
        st = context.scene.powder
        try:
            repo, game = paths(context)
            out = os.path.join(tempfile.gettempdir(), "powder_" + re.sub(r"\W", "_", st.entry) + ".glb")
            args = ["export", os.path.join(game, st.archive), st.entry, "-o", out]
            if st.lod0_only:
                args += ["--mesh", "LOD0"]
            if st.textures:
                args += ["--textures"]
            for r in filter(None, (x.strip() for x in st.rig.split(","))):
                args += ["--rig", r]
            run(repo, "mesh.py", *args)
        except RuntimeError as ex:
            self.report({"ERROR"}, str(ex))
            return {"CANCELLED"}
        object_mode(context)
        before = set(bpy.data.objects)
        bpy.ops.import_scene.gltf(filepath=out)
        new = set(bpy.data.objects) - before
        tidy(new)
        for o in new:
            o["powder_archive"], o["powder_item"] = st.archive, st.entry
        self.report({"INFO"}, f"{st.entry}: {sum(o.type == 'MESH' for o in new)} objects imported")
        return {"FINISHED"}


def item_objects(context, item: str) -> list:
    return [o for o in context.scene.objects if o.get("powder_item") == item]


class POWDER_OT_write_mod(bpy.types.Operator):
    bl_idname = "powder.write_mod"
    bl_label = "Write mod"
    bl_description = "Export to glTF and write the asset mod (mods/<name>/client/forge) for every datafile that " \
                     "holds the item's meshes"

    def execute(self, context):
        st = context.scene.powder
        try:
            return self.write(context, st)
        except RuntimeError as ex:
            self.report({"ERROR"}, str(ex))
            return {"CANCELLED"}

    def write(self, context, st):
        repo, game = paths(context)
        if not re.fullmatch(r"[A-Za-z0-9_-]+", st.mod_name) or st.mod_name.startswith(("_", "-")):
            raise RuntimeError("mod name: letters, digits, _ and - only")
        item = next((o["powder_item"] for o in [context.active_object] if o and o.get("powder_item")), st.entry)
        source = item_objects(context, item)
        if not source:
            raise RuntimeError(f"no objects of {item} in the scene (Import item first)")
        archive = source[0]["powder_archive"]
        ids = sorted({o["steep_mesh_id"] for o in source if "steep_mesh_id" in o})
        entry = next((o["steep_entry"] for o in source if "steep_entry" in o), item)
        if st.mode == "REPLACE":
            meshes = [o for o in source if o.type == "MESH" and o.visible_get() and "steep_mesh_id" in o]
            if st.into == "NAME" and any("@" in o.name for o in meshes):
                raise RuntimeError("an item object has an @ in its name")
        else:
            meshes = [o for o in context.selected_objects if o.type == "MESH" and o not in source]
            if st.into == "NAME":
                raise RuntimeError("added pieces need Into: Every LOD or LOD0 only")
        if not meshes:
            raise RuntimeError("nothing to export: " + ("no visible LOD objects" if st.mode == "REPLACE" else
                                                         "select the pieces to add"))
        rigs = {o.find_armature() for o in meshes} | {o.parent for o in meshes if o.parent}
        export = set(meshes) | {r for r in rigs if r}
        object_mode(context)
        keep_sel, keep_active = list(context.selected_objects), context.view_layer.objects.active
        hidden = [o for o in export if o.hide_get()]
        try:
            for o in hidden:
                o.hide_set(False)
            bpy.ops.object.select_all(action="DESELECT")
            for o in export:
                o.select_set(True)
            glb = os.path.join(tempfile.gettempdir(), f"powder_{st.mod_name}.glb")
            bpy.ops.export_scene.gltf(filepath=glb, export_format="GLB", use_selection=True, export_animations=False)
        finally:
            for o in hidden:
                o.hide_set(True)
            bpy.ops.object.select_all(action="DESELECT")
            for o in keep_sel:
                o.select_set(True)
            context.view_layer.objects.active = keep_active
        # every datafile that holds one of the item's meshes gets the same import
        targets = {(archive[:-6], entry)}
        if ids:
            out = run(repo, "forge.py", "where", game, *[x for i in ids for x in ("--id", i)])
            targets |= {(m[2], m[1]) for m in re.finditer(r"entry (0x[0-9a-f]+) .*\(base (\S+)\)", out)}
        flags = ["--to", "LOD", "--ignore-names"] if st.into == "ALL" else \
            ["--to", "LOD0", "--ignore-names"] if st.into == "LOD0" else []
        if st.mode == "ADD":
            flags.append("--add")
        mod = os.path.join(repo, "mods", st.mod_name)
        written = []
        for base, entry in sorted(targets):
            res = run(repo, "mesh.py", "import", os.path.join(game, base + ".forge"), entry, "--glb", glb, *flags,
                      "-o", os.path.join(mod, "client", "forge", base))
            written += [line for line in res.splitlines() if line.endswith("imported mesh(es)")]
        toml = os.path.join(mod, "mod.toml")
        if not os.path.exists(toml):
            with open(toml, "w") as f:
                f.write(f'version = "0.1"\ndescription = "Made in Blender: {item} ({st.mode.lower()})"\n')
        self.report({"INFO"}, f"mods/{st.mod_name}: {len(written)} datafile(s) written; enable it with "
                              f"POWDER_CLIENT_MODS={st.mod_name}")
        return {"FINISHED"}


class POWDER_PT_panel(bpy.types.Panel):
    bl_label = "Powder (Steep)"
    bl_space_type = "VIEW_3D"
    bl_region_type = "UI"
    bl_category = "Powder"

    def draw(self, context):
        st = context.scene.powder
        col = self.layout.column()
        col.label(text="Import from the game", icon="IMPORT")
        col.prop(st, "archive")
        col.prop(st, "entry")
        row = col.row()
        row.prop(st, "lod0_only")
        row.prop(st, "textures")
        col.prop(st, "rig")
        col.operator("powder.import_item", icon="ARMATURE_DATA")
        col.separator()
        col.label(text="Write an asset mod", icon="EXPORT")
        col.prop(st, "mod_name")
        col.prop(st, "mode")
        col.prop(st, "into")
        col.operator("powder.write_mod", icon="FILE_TICK")
        col.label(text=f'Output: the "{LOG}" text', icon="TEXT")


CLASSES = (PowderPrefs, PowderSettings, POWDER_OT_import_item, POWDER_OT_write_mod, POWDER_PT_panel)


def register():
    for c in CLASSES:
        bpy.utils.register_class(c)
    bpy.types.Scene.powder = PointerProperty(type=PowderSettings)


def unregister():
    del bpy.types.Scene.powder
    for c in reversed(CLASSES):
        bpy.utils.unregister_class(c)
