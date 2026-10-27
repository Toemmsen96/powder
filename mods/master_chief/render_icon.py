"""Blender script: the costume's menu picture, a front view of the model on a transparent background (like the
game's own costume icons, 256 x 256). Runs headless on the CPU (Cycles):

  blender --factory-startup -b --python render_icon.py -- <scene.gltf> <out.png> [size]
"""

import math
import sys

import bpy
from mathutils import Vector

argv = sys.argv[sys.argv.index("--") + 1:]
src, out = argv[0], argv[1]
size = int(argv[2]) if len(argv) > 2 else 256

bpy.ops.wm.read_factory_settings(use_empty=True)
bpy.ops.import_scene.gltf(filepath=src)
scene = bpy.context.scene
bpy.context.view_layer.update()

# the model's bounds (evaluated meshes, so armature poses count)
dg = bpy.context.evaluated_depsgraph_get()
lo, hi = Vector((1e9,) * 3), Vector((-1e9,) * 3)
for ob in scene.objects:
    if ob.type != "MESH":
        continue
    ev = ob.evaluated_get(dg)
    for v in ev.to_mesh().vertices:
        p = ev.matrix_world @ v.co
        lo = Vector(map(min, lo, p))
        hi = Vector(map(max, hi, p))
    ev.to_mesh_clear()
centre, ext = (lo + hi) / 2, hi - lo

# the model's base-colour textures carry alpha 0: on a transparent film the body would vanish
for mat in bpy.data.materials:
    for node in (mat.node_tree.nodes if mat.use_nodes else []):
        if node.type == "BSDF_PRINCIPLED":
            for link in list(node.inputs["Alpha"].links):
                mat.node_tree.links.remove(link)
            node.inputs["Alpha"].default_value = 1.0

# glTF's +Z (the model's front) is Blender's -Y: the camera looks at it from -Y, a little from the right and above
cam = bpy.data.objects.new("cam", bpy.data.cameras.new("cam"))
scene.collection.objects.link(cam)
cam.data.type = "ORTHO"
cam.data.ortho_scale = max(ext.z, ext.x) * 1.08
cam.data.clip_start, cam.data.clip_end = ext.length * 0.01, ext.length * 10
turn = math.radians(12)
cam.location = centre + Vector((math.sin(turn), -math.cos(turn), 0.08)) * ext.length * 3
cam.rotation_euler = (centre - cam.location).to_track_quat("-Z", "Y").to_euler()
scene.camera = cam

for name, rot, energy in (("key", (55, 0, -35), 4.0), ("fill", (70, 0, 140), 1.5), ("top", (10, 0, 0), 1.0)):
    sun = bpy.data.objects.new(name, bpy.data.lights.new(name, "SUN"))
    sun.data.energy = energy
    sun.rotation_euler = tuple(math.radians(a) for a in rot)
    scene.collection.objects.link(sun)
world = bpy.data.worlds.new("w")
world.use_nodes = True
world.node_tree.nodes["Background"].inputs[1].default_value = 0.6
scene.world = world

scene.render.engine = "CYCLES"
scene.cycles.device = "CPU"
scene.cycles.samples = 48
scene.render.film_transparent = True
scene.render.resolution_x = scene.render.resolution_y = size
scene.render.image_settings.file_format = "PNG"
scene.render.image_settings.color_mode = "RGBA"
scene.view_settings.view_transform = "Standard"
scene.render.filepath = out
bpy.ops.render.render(write_still=True)
