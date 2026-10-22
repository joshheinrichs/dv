# The seed and zlib's tarball as repkgs' nix/sources.nix and <nix/fetchurl.nix> make them.

def fetchurl(url, name, hash, unpack = False):
    return derivation(
        builder = "builtin:fetchurl",
        name = name,
        url = url,
        urls = [url],
        unpack = unpack,
        executable = False,
        system = "builtin",
        outputHash = hash,
        outputHashAlgo = "",
        outputHashMode = "recursive" if unpack else "flat",
        preferLocalBuild = True,
        impureEnvVars = ["http_proxy", "https_proxy", "ftp_proxy", "all_proxy", "no_proxy"],
    )

seed = fetchurl(
    url = "https://github.com/Mic92/repkgs/releases/download/seed-4/seed-4-x86_64-linux.nar.xz",
    name = "seed",
    hash = "sha256-SeXFfroI8UhfUiw2fpx/0UL/ZbuxRwYpK0aMSZ8e7NA=",
    unpack = True,
)

zlib_src = derivation(
    name = "zlib-1.3.2",
    system = "x86_64-linux",
    unpacker = seed,
    builder = str(seed) + "/bin/nu",
    args = ["--no-config-file", path("//pkgs/up/uptrack/src/unpack.nu"), "--fetch"],
    outputHashMode = "recursive",
    preferLocalBuild = True,
    impureEnvVars = ["http_proxy", "https_proxy", "ftp_proxy", "all_proxy", "no_proxy"],
    urls = ["https://github.com/madler/zlib/releases/download/v1.3.2/zlib-1.3.2.tar.gz"],
    outputHash = "sha256-tKLefmGqB2HVUMILJ7Gqa0v9GH7fqv64d5S5kK1tcYU=",
)
