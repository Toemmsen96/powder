# x64 stack unwinding over a dumped image (dumpexe.py): the exception directory's RUNTIME_FUNCTIONs and their
# UNWIND_INFO, as Windows' RtlVirtualUnwind does it. Used by stackwatch.py to turn a stack snapshot taken outside
# the game into a call stack. Frames inside VMProtect-virtualized code or our own hooks have no unwind info: the
# walk stops there (or, for a leaf, takes [rsp] as the return address).
import bisect, struct

UWOP_PUSH_NONVOL, UWOP_ALLOC_LARGE, UWOP_ALLOC_SMALL, UWOP_SET_FPREG = 0, 1, 2, 3
UWOP_SAVE_NONVOL, UWOP_SAVE_NONVOL_FAR, UWOP_EPILOG, UWOP_SPARE = 4, 5, 6, 7
UWOP_SAVE_XMM128, UWOP_SAVE_XMM128_FAR, UWOP_PUSH_MACHFRAME = 8, 9, 10
REGS = ["rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15"]


class Image:
    def __init__(self, path: str, base: int):
        self.d = open(path, "rb").read()
        self.base = base
        pe = struct.unpack_from("<I", self.d, 0x3C)[0]
        va, sz = struct.unpack_from("<II", self.d, pe + 24 + 112 + 8 * 3)        # exception directory
        ents = [struct.unpack_from("<III", self.d, va + 12 * i) for i in range(sz // 12)]
        self.funcs = sorted(e for e in ents if e[0] < e[1])
        self.starts = [f[0] for f in self.funcs]
        self.size = len(self.d)

    def func(self, rva: int):
        i = bisect.bisect_right(self.starts, rva) - 1
        if i >= 0 and self.funcs[i][0] <= rva < self.funcs[i][1]:
            return self.funcs[i]
        return None

    def u8(self, rva): return self.d[rva]
    def u16(self, rva): return struct.unpack_from("<H", self.d, rva)[0]
    def u32(self, rva): return struct.unpack_from("<I", self.d, rva)[0]


def unwind_frame(img: Image, regs: dict, read64) -> bool:
    """Unwind one frame in place: regs (rip, rsp + the nonvolatiles) -> the caller's. read64(addr) -> int|None."""
    rip = regs["rip"]
    rva = rip - img.base
    f = img.func(rva) if 0 <= rva < img.size else None
    if f is None:                                    # leaf or no unwind info: return address on top of the stack
        ret = read64(regs["rsp"])
        if ret is None:
            return False
        regs["rip"], regs["rsp"] = ret, regs["rsp"] + 8
        return True
    begin, _end, info = f
    off_in_func = rva - begin
    machframe = False
    while True:
        flags_ver = img.u8(info)
        flags = flags_ver >> 3
        prolog = img.u8(info + 1)
        count = img.u8(info + 2)
        fr = img.u8(info + 3)
        frame_reg, frame_off = fr & 0xF, fr >> 4
        codes = info + 4
        i = 0
        while i < count:
            code_off = img.u8(codes + 2 * i)
            op = img.u8(codes + 2 * i + 1)
            uop, oinfo = op & 0xF, op >> 4
            slots = {UWOP_ALLOC_LARGE: 2 if oinfo == 0 else 3, UWOP_SAVE_NONVOL: 2, UWOP_SAVE_NONVOL_FAR: 3,
                     UWOP_SAVE_XMM128: 2, UWOP_SAVE_XMM128_FAR: 3, UWOP_EPILOG: 2, UWOP_SPARE: 3}.get(uop, 1)
            if off_in_func < code_off and begin == f[0]:   # in the prolog: this step has not happened yet
                i += slots
                continue
            if uop == UWOP_PUSH_NONVOL:
                v = read64(regs["rsp"])
                if v is None:
                    return False
                regs[REGS[oinfo]] = v
                regs["rsp"] += 8
            elif uop == UWOP_ALLOC_LARGE:
                regs["rsp"] += img.u16(codes + 2 * (i + 1)) * 8 if oinfo == 0 else img.u32(codes + 2 * (i + 1))
            elif uop == UWOP_ALLOC_SMALL:
                regs["rsp"] += oinfo * 8 + 8
            elif uop == UWOP_SET_FPREG:
                regs["rsp"] = regs[REGS[frame_reg]] - frame_off * 16
            elif uop == UWOP_SAVE_NONVOL:
                v = read64(regs["rsp"] + img.u16(codes + 2 * (i + 1)) * 8)
                if v is None:
                    return False
                regs[REGS[oinfo]] = v
            elif uop == UWOP_SAVE_NONVOL_FAR:
                v = read64(regs["rsp"] + img.u32(codes + 2 * (i + 1)))
                if v is None:
                    return False
                regs[REGS[oinfo]] = v
            elif uop == UWOP_PUSH_MACHFRAME:
                sp = regs["rsp"] + (8 if oinfo else 0)
                rip2, rsp2 = read64(sp), read64(sp + 24)
                if rip2 is None or rsp2 is None:
                    return False
                regs["rip"], regs["rsp"] = rip2, rsp2
                machframe = True
            i += slots
        if flags & 4:                                # UNW_FLAG_CHAININFO: continue with the parent's codes
            chained = codes + 2 * ((count + 1) & ~1)
            begin, _e, info = struct.unpack_from("<III", img.d, chained)
            off_in_func = 1 << 30                    # the parent's prolog has fully run
            continue
        break
    if machframe:
        return True
    ret = read64(regs["rsp"])
    if ret is None:
        return False
    regs["rip"], regs["rsp"] = ret, regs["rsp"] + 8
    return True


def walk(img: Image, regs: dict, read64, depth: int = 24) -> list[int]:
    """Return addresses from regs (rip first), unwinding up to depth frames."""
    regs = dict(regs)
    out = [regs["rip"]]
    for _ in range(depth):
        if not unwind_frame(img, regs, read64) or regs["rip"] < 0x10000:
            break
        out.append(regs["rip"])
    return out
