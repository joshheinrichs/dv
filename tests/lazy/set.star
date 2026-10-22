# A package set as lazy_attrs: packages see the set through `pkgs(args)`, which rebuilds it
# (equal calls are one set).

NAMES = ["a", "b", "c", "cyc1", "cyc2", "broken", "uses_broken", "unsupported", "picky"] + ["w%d" % i for i in range(16)]

def pkgs(args):
    return lazy_attrs(call, args, names = NAMES)

def mk(name, args, deps = [], extra = {}):
    return derivation(
        name = name,
        system = args["system"],
        builder = "/bin/sh",
        deps = deps,
        **extra
    )

def busy(n):
    x = 0
    for i in range(n):
        x = (x * 31 + i) % 1000003
    return x

def call(name, args):
    p = pkgs(args)
    if name == "a":
        return mk("a", args)
    if name == "b":
        return mk("b", args, [p.a])
    if name == "c":
        return mk("c", args, [p.a, p.b], {"aname": p.a.name})
    if name == "cyc1":
        return mk("cyc1", args, [p.cyc2])
    if name == "cyc2":
        return mk("cyc2", args, [p.cyc1])
    if name == "broken":
        fail("broken on purpose")
    if name == "uses_broken":
        # getattr's default must not hide that `broken` failed
        return mk("uses_broken", args, extra = {"x": getattr(p.broken, "name", "fallback")})
    if name == "unsupported":
        return struct(supported = False, drv = poison("unsupported: needs a quantum computer"))
    if name == "picky":
        u = p.unsupported
        if u.supported:
            fail("unsupported says it is supported")
        return mk("picky", args, [u.drv])
    return mk(name, args, extra = {"work": busy(args["work"])})

set = pkgs({"system": "x86_64-linux", "work": 300000})
same = pkgs({"system": "x86_64-linux", "work": 300000})
other = pkgs({"system": "aarch64-linux", "work": 300000})

def wide(s):
    return mk("wide", {"system": "x86_64-linux"}, [getattr(s, "w%d" % i) for i in range(16)])
