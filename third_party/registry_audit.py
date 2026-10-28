"""Run cargo-audit against explicit crates.io identities, without dependencies."""

from pathlib import Path
import subprocess


REGISTRY_SOURCE = "registry+https://github.com/rust-lang/crates.io-index"


def audit(packages, directory):
    """Audit (name, version, checksum) identities with an empty ignore list."""
    lock = Path(directory) / "Cargo.lock"
    contents = "version = 4\n"
    for name, version, checksum in packages:
        contents += (f'\n[[package]]\nname = "{name}"\nversion = "{version}"\n'
                     f'source = "{REGISTRY_SOURCE}"\nchecksum = "{checksum}"\n')
    lock.write_text(contents, encoding="utf-8")
    config = Path(directory) / ".cargo/audit.toml"
    config.parent.mkdir()
    config.write_text("[advisories]\nignore = []\n", encoding="utf-8")
    subprocess.run(["cargo", "audit", "--deny", "warnings", "--file", str(lock)], cwd=directory, check=True)
