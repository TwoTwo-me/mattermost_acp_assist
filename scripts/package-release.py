#!/usr/bin/env python3
"""Package a prebuilt native binary with its runtime configuration samples."""

import argparse
import gzip
import hashlib
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
)
FILES = {
    "README.md": "README.md",
    "THIRD_PARTY_LICENSES.txt": "THIRD_PARTY_LICENSES.txt",
    ".env.example": ".env.example",
    "acp/agents.json": "acp/agents.json",
    "acp/registry.json": "acp/registry.example.json",
    "acp/prompt.example.txt": "acp/prompt.example.txt",
    "package.json": "package.json",
    "package-lock.json": "package-lock.json",
}


def runtime_notices():
    sysroot = Path(subprocess.check_output(["rustc", "--print", "sysroot"], text=True).strip())
    version = subprocess.check_output(["rustc", "--version"], text=True).strip()
    document_roots = [
        sysroot, sysroot / "share/doc/rust", sysroot / "share/doc/rustc",
        sysroot / "share/doc/rust/html", sysroot / "share/doc/rustc/html",
    ]
    required = {
        "LICENSE-MIT": ("LICENSE-MIT", "licenses/MIT.txt"),
        "LICENSE-APACHE": ("LICENSE-APACHE", "licenses/Apache-2.0.txt"),
        "COPYRIGHT": ("COPYRIGHT", "COPYRIGHT.txt", "COPYRIGHT.html"),
        "COPYRIGHT-library.html": ("COPYRIGHT-library.html",),
    }
    paths = []
    for label, names in required.items():
        path = next(
            (directory / name for directory in document_roots for name in names if (directory / name).is_file()),
            None,
        )
        if path is None:
            raise RuntimeError(
                f"matching Rust toolchain notice {label} is missing; install rust-docs for the toolchain that built the binary"
            )
        paths.append(path)
    library = sysroot / "lib/rustlib/src/rust/library"
    if not library.is_dir():
        raise RuntimeError("matching Rust standard library sources are missing; install rust-src for the build toolchain")
    notice_name = re.compile(r"^(?:licen[cs]e|unlicense|copying|copyright|notices?)(?:$|[._-])", re.IGNORECASE)
    source_notices = sorted(path for path in library.rglob("*") if path.is_file() and notice_name.match(path.name))
    if not source_notices:
        raise RuntimeError("matching Rust standard library source notices are missing")
    paths.extend(source_notices)
    sections = [
        f"\n{'=' * 78}\nRust toolchain runtime notices\nCompiler: {version}\n",
        "Collected from the installed build toolchain, including its library attribution\n"
        "document and available rust-src license/copyright/NOTICE files. Source paths are\n"
        "relative to that toolchain. HTML attribution documents are preserved verbatim.\n"
        "This collection is not a legal certification of every possible distribution.\n",
    ]
    for path in paths:
        text = path.read_text(encoding="utf-8")
        if not text.strip():
            raise RuntimeError(f"empty Rust runtime notice: {path.relative_to(sysroot)}")
        sections.append(f"\n--- {path.relative_to(sysroot).as_posix()} ---\n{text}")
        if not text.endswith("\n"):
            sections.append("\n")
    return "".join(sections)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--target", required=True, choices=TARGETS)
    parser.add_argument("--version", required=True, help="release tag, for example v0.1.4")
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    if not re.fullmatch(r"v[0-9]+\.[0-9]+\.[0-9]+(?:-[A-Za-z0-9.-]+)?", args.version):
        parser.error("--version must be a v-prefixed release version")
    if args.version != f"v{version}":
        parser.error(f"--version must match Cargo.toml: v{version}")
    sources = {"mattermost-acp-assist": args.binary, **{
        destination: ROOT / source for destination, source in FILES.items()
    }}
    for source in sources.values():
        if source.is_symlink() or not source.is_file():
            parser.error(f"package input must be a regular file, not a symlink: {source}")
    notices = (ROOT / "THIRD_PARTY_LICENSES.txt").read_text(encoding="utf-8")
    lock_hash = hashlib.sha256((ROOT / "Cargo.lock").read_bytes()).hexdigest()
    if f"Cargo.lock SHA256: {lock_hash}" not in notices.splitlines():
        parser.error("dependency notices are stale; run python3 scripts/generate-notices.py")
    notices += runtime_notices()
    name = f"mattermost-acp-assist-{args.version}-{args.target}"
    args.output.mkdir(parents=True, exist_ok=True)
    archive = args.output / f"{name}.tar.gz"
    with tempfile.TemporaryDirectory(prefix="acp-assist-package-") as temporary:
        stage = Path(temporary) / name
        (stage / "acp").mkdir(parents=True)
        for destination, source in sources.items():
            path = stage / destination
            if destination == "THIRD_PARTY_LICENSES.txt":
                path.write_text(notices, encoding="utf-8")
            else:
                shutil.copyfile(source, path)
            path.chmod(0o755 if destination == "mattermost-acp-assist" else 0o644)
        with archive.open("wb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as bundle:
                    for relative in ["", "acp", *sorted(sources)]:
                        path = stage / relative
                        info = bundle.gettarinfo(str(path), arcname=f"{name}/{relative}".rstrip("/"))
                        info.uid = info.gid = info.mtime = 0
                        info.uname = info.gname = ""
                        info.mode = 0o755 if info.isdir() or relative == "mattermost-acp-assist" else 0o644
                        if info.isfile():
                            with path.open("rb") as content:
                                bundle.addfile(info, content)
                        else:
                            bundle.addfile(info)
    with archive.open("rb") as content:
        digest = hashlib.file_digest(content, "sha256").hexdigest()
    checksum = archive.with_name(f"{archive.name}.sha256")
    checksum.write_text(f"{digest}  {archive.name}\n")
    print(archive)
    print(checksum)


if __name__ == "__main__":
    main()
