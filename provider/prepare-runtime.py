#!/usr/bin/env python3
"""Prepare a verified Kathamo provider runtime for Cargo."""
import base64
import gzip
import hashlib
import mmap
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
from urllib.request import urlopen

# 18.18.3 is the last upstream release with Antigravity enabled.
VERSION = "18.18.3"
PROVIDER = Path(__file__).resolve().parent
CALLBACK_PAGE = PROVIDER / "oauth-callback.html"
ICON = PROVIDER.parent / "assets" / "logo" / "kathamo.ico"
ASSETS = {
    ("linux", "x86_64"): ("linux-x64", "8c9a178fed74ec1da21aafb7d28cd9839b3b0c9e7d607c86cc3b6177355729f4"),
    ("linux", "aarch64"): ("linux-arm64", "c49a84875d915c7d0de8bb69fe180b3bf53c16600bc4d5a61ea22ac2b3ac45c9"),
    ("macos", "aarch64"): ("darwin-arm64", "87e8e593046dfb1047e359ad845648ffc8bda91aa59a9785690d5107f562c32d"),
    ("macos", "x86_64"): ("darwin-x64", "8c09dd1afafca855e817f5eb4f865a16fd31bb47a4bd205363cdf472ededb311"),
    ("windows", "x86_64"): ("windows-x64.exe", "07d50f434f7051dc79b9c80b468e78b6a0fa904ca1bd599a52d8734d0cd2bfd8"),
    ("windows", "aarch64"): ("windows-arm64.exe", "1f0081a4cd9f4e4ed253c657a9fe768d9375774ec949e7ce064c119acaf8af12"),
}


def digest(path):
    checksum = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            checksum.update(chunk)
    return checksum.hexdigest()


def callback_page():
    # Reuse the 32px PNG embedded in the existing application icon.
    icon = ICON.read_bytes()
    count = struct.unpack_from("<H", icon, 4)[0]
    for index in range(count):
        width, height, _, _, _, _, length, offset = struct.unpack_from(
            "<BBBBHHII", icon, 6 + index * 16
        )
        if (width, height) == (32, 32):
            png = icon[offset:offset + length]
            if png.startswith(b"\x89PNG\r\n\x1a\n"):
                return CALLBACK_PAGE.read_text(encoding="utf-8").replace(
                    "__KATHAMO_LOGO__", base64.b64encode(png).decode("ascii")
                )
    sys.exit("Kathamo application icon is missing its 32px PNG")


def replace_callback_page(binary, html):
    # Bun embeds the callback page as text. Keep its original byte length so
    # module offsets and all OAuth executable code stay intact.
    with binary.open("r+b") as file, mmap.mmap(file.fileno(), 0) as content:
        matches = []
        for encoding in ("utf-8", "utf-16le"):
            title = "<title>omp · authentication</title>".encode(encoding)
            position = content.find(title)
            if position < 0:
                continue
            if content.find(title, position + len(title)) >= 0:
                sys.exit("Multiple OAuth callback templates in the provider runtime")
            start = content.rfind("<!doctype html>".encode(encoding), max(0, position - 2048), position)
            closing = "</html>".encode(encoding)
            end = content.find(closing, position, position + 32768)
            if start >= 0 and end >= 0:
                end += len(closing)
                if "__OAUTH_STATE__".encode(encoding) in content[start:end]:
                    matches.append((encoding, start, end))
        if len(matches) != 1:
            sys.exit("Could not identify the pinned provider's OAuth callback template")
        encoding, start, end = matches[0]
        page = html.encode(encoding)
        padding = " ".encode(encoding)
        if len(page) > end - start or (end - start - len(page)) % len(padding):
            sys.exit("Kathamo OAuth callback page does not fit the provider template")
        content[start:end] = page + padding * ((end - start - len(page)) // len(padding))


def main():
    os_name, arch, cache = sys.argv[1:]
    asset = ASSETS.get((os_name, arch))
    if asset is None:
        sys.exit(f"No Kathamo provider runtime for {os_name} {arch}")
    name, checksum = asset
    bundle = Path(cache) / f"{VERSION}-{name}"
    bundle.parent.mkdir(parents=True, exist_ok=True)
    if not bundle.is_file() or digest(bundle) != checksum:
        url = f"https://github.com/can1357/oh-my-pi/releases/download/v{VERSION}/omp-{name}"
        print(f"Fetching Kathamo provider snapshot {VERSION} ({name})", file=sys.stderr, flush=True)
        with tempfile.TemporaryDirectory(dir=bundle.parent) as tmp:
            download = Path(tmp) / "download"
            with urlopen(url, timeout=120) as response, download.open("wb") as target:
                for chunk in iter(lambda: response.read(1024 * 1024), b""):
                    target.write(chunk)
            if digest(download) != checksum:
                sys.exit("Kathamo provider snapshot checksum mismatch")
            download.replace(bundle)
    html = callback_page()
    branding = hashlib.sha256(html.encode() + Path(__file__).read_bytes()).hexdigest()[:12]
    revision = f"{VERSION}-kathamo-{branding}"
    compressed = bundle.with_name(f"{revision}-{name}.{checksum[:12]}.gz")
    if not compressed.is_file():
        with tempfile.TemporaryDirectory(dir=bundle.parent) as tmp:
            branded = Path(tmp) / bundle.name
            shutil.copyfile(bundle, branded)
            replace_callback_page(branded, html)
            if os_name == "macos":
                subprocess.run(["codesign", "--force", "--sign", "-", str(branded)], check=True)
            staging = Path(tmp) / "bundle.gz"
            with branded.open("rb") as source, gzip.open(staging, "wb", compresslevel=1) as target:
                shutil.copyfileobj(source, target)
            staging.replace(compressed)

    # gzip's final four bytes store the original size modulo 2^32. The
    # runtime is smaller than 4 GiB, and macOS re-signing may change its size.
    with compressed.open("rb") as bundle_file:
        bundle_file.seek(-4, 2)
        size = struct.unpack("<I", bundle_file.read(4))[0]
    print(compressed)
    print(size)
    print(revision)


if __name__ == "__main__":
    sys.exit(main())
