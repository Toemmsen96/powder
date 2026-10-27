# mesh_edit: the first mesh mod

This mod tests whether an edited mesh works in the game. It changes the default male rider's jacket mesh,
`STP_M_Jacket_001`, which the menu rider (Freerider M, item `JACKET_M_N_001`) wears. Every vertex of all 4 LODs
moves **5 cm outward along its normal**, so the jacket looks like a thick puffer jacket. The vertex and triangle
counts, UVs, normals, tangents, skin weights, bones, materials and ids stay as they are. Only positions and bounds
change. `mods/model_swap/README.md` shows how the menu rider's jacket chain was found.

| replaced entry (keeps its id) | archive (base) | holds | what changes |
|---|---|---|---|
| `0x8b66b1fe21` `STP_M_Jacket_001` (entity) | `DataPC_extra_chr_onboarding` | `_LOD0` `0x7a5374f895`, `_LOD1` `0x8b66b1fe20`, `_LOD2` `0x8b66b1fe22`, `_LOD3` `0x8b66b1fe23`, material `_ALL`, texture set | all 4 LOD meshes inflated |
| `0x7a5374f895` `STP_M_Jacket_001_LOD0` | `DataPC_STP_Alpes` | the LOD0 mesh, material, texture set | LOD0 inflated |

These are all the datafiles that embed one of the four LOD meshes. A scan of the header blocks of all 164,970
datafiles in the 27 archives found LOD0 in exactly these two, and LOD1..3 only in the entity. Neither id has a copy
in a `_patch_01`/`_02`, so `_patch_03` is the newest copy. The two LOD0 copies were already different in one header
byte (+0x0D: 1 in the entity, 2 in the Alps), and each keeps its own byte. The edited LOD0 payloads differ in that
byte only.

Each entry is an unpacked datafile folder under `client/forge/<base>/`. The session tooling builds one archive per
base from them, `<base>_patch_03.forge` (see `mods/model_swap`), and the game loads the newest copy of an id.

## What you should see

In the main menu, Freerider M's jacket is visibly puffier: about 10 cm wider across the chest and the sleeves. The
colours and the texture stay, because the UVs are unchanged. The zipper pulls are a separate entity
(`STP_M_Jacket_001_Zippers01`), so they keep their place and may sink into the thicker jacket. The inside of the hood
and of the cuffs moves inward, along their own normals, so the hood opening is a little narrower. The jacket still
moves with the rider, because skinning is untouched. Every LOD changes the same way, so the jacket stays puffy at a
distance.

If the jacket does not change: check that the session put `DataPC_extra_chr_onboarding_patch_03.forge` beside its
base (powder-wrap's log says "asset mods: ..."). If the game crashes at the menu, the mesh is the first suspect,
since the archive path is proven by `model_swap`. Turn the mod off and report it.

## How it was made

`tools/forge/mesh.py edit <archive> <entry> --inflate 0.05 -o client/forge/<base>` does this:
1. It decodes each mesh (format 2: i16 positions times S/32767 with S = 2 m, u8 normals).
2. It averages the normal over all vertices at the same position, so UV seams move together and do not open.
3. It moves each vertex 5 cm along that normal and encodes it again.
4. It writes the same rows into the mesh's second, reordered vertex copy.
5. It recomputes the bounds (header box, section-1 centre/half size and per-submesh box).

`tools/forge/MESHES.md` describes the format and how it was established.

## Verified vs assumed

Verified with tools, without running the game:
- The decoded jacket is a hooded jacket in A-pose on a ~1.8 m rider: bounds x -0.55..0.55, y -0.16..0.20,
  z 0.86..1.66 m. Its UV islands line up with the diffuse texture, and its normals agree with the face winding.
  The inflated LOD0 has bounds -0.60..0.60, -0.21..0.25, 0.82..1.70.
- An identity edit returns all four payloads byte for byte. Importing the inflated OBJs into the original gives
  exactly the edited payloads.
- `mesh.py check` passes on the edited meshes: they parse, re-encode, section 2 is a reordering of section 1, and
  positions match the bounds.
- `forge.py build ... --like <base>` of both folders gives archives that pass `forge.py verify --decompress`.
  `forge.py stack` with them next to the shipped archives shows `<- ..._patch_03 > <base>` for both ids. The
  prefetch blobs are the shipped ones, since the dependencies are unchanged.

Assumed (needs the live test):
- The engine draws the vertex data it finds, with no checksum or cached copy of the mesh elsewhere.
- Bounds a few cm larger cause no trouble, since culling uses the new ones.
- The 8-bit per-vertex value in the position's w (G: cloth/wind) does not depend on the position.
- The menu rider uses LOD0 or LOD1 at menu distance. All 4 are edited anyway.

## Regenerating

The entries are Steep's own data, so they are not in git (`/mods/*/client/forge/` is ignored). Run
`mods/mesh_edit/generate.sh [<Steep folder>]` once before enabling the mod. `MESH_EDIT_INFLATE=0.08` changes the
amount, in metres. A negative value shrinks the jacket and may push it into the body. The script does this, with
`G` as the game folder:

```
M=tools/forge/mesh.py; D=mods/mesh_edit/client/forge
python3 $M edit "$G/DataPC_extra_chr_onboarding.forge" 0x8b66b1fe21 --inflate 0.05 -o $D/DataPC_extra_chr_onboarding
python3 $M edit "$G/DataPC_STP_Alpes.forge" 0x7a5374f895 --inflate 0.05 -o $D/DataPC_STP_Alpes
```

`mesh.py` takes the entry from the newest *shipped* archive of the stack. It leaves out the `_patch_03` files that
a running or killed powder-wrap session put beside the game (`.powder-assets`), so running the script during a
session does not inflate the jacket twice.

## Undo

Turn the mod off: leave it out of `POWDER_CLIENT_MODS`, or decline it in the launcher. The edit exists only in
the generated `_patch_03` archives, which the session adds and removes. The installed game files are never
changed, and the save is untouched.
