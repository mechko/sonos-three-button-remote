#!/usr/bin/env python3
"""Audit Git-tracked source for private paths and local Wi-Fi values (Python 3.11+)."""

import pathlib
import subprocess
import sys
import tomllib


def main():
    try:
        root = pathlib.Path(subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"], text=True).strip())
        paths = subprocess.check_output(
            ["git", "ls-files", "-z"], cwd=root).decode().split("\0")
    except subprocess.CalledProcessError:
        print("Initialize Git and stage the public source files before auditing.", file=sys.stderr)
        return 1

    local = root / "wifi.local.toml"
    secrets = []
    if local.exists():
        try:
            config = tomllib.loads(local.read_text())
            secrets = [v.encode() for k, v in config.items()
                       if k in ("ssid", "password") and isinstance(v, str) and v
                       and v not in ("your-network", "your-password")]
        except (OSError, ValueError):
            print("Cannot parse local Wi-Fi configuration (values omitted).", file=sys.stderr)
            return 1
    failures = []
    for name in filter(None, paths):
        path = pathlib.PurePosixPath(name)
        private = (any(p in ("target", ".cargo-cache", ".aws", ".codex", ".agents", "__pycache__")
                       for p in path.parts)
                   or ".local." in path.name or path.name.startswith(".env")
                   or path.suffix in (".pem", ".key", ".bin", ".elf", ".uf2", ".pyc"))
        if private:
            failures.append(name + ": private/generated path")
            continue
        # Scan the index: this is what a commit would publish, not unstaged edits.
        data = subprocess.check_output(["git", "show", ":" + name], cwd=root)
        if any(secret in data for secret in secrets):
            failures.append(name + ": contains a local Wi-Fi value (value omitted)")
    if failures:
        print("Public source audit failed:\n" + "\n".join(failures), file=sys.stderr)
        return 1
    print("Public source audit passed: no private paths or known local Wi-Fi values in the index.")
    if not secrets:
        print("No private Wi-Fi values available for comparison; review files manually.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
