#!/usr/bin/env python3
"""Package a locally built Hulder executable without downloading or publishing."""
import argparse
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tomllib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--binary", type=Path, required=True)
parser.add_argument("--output", type=Path, required=True, help="New .app path; existing paths are refused")
parser.add_argument("--channel", choices=("dev", "nightly", "preview", "stable"), default="dev")
args = parser.parse_args()
root = Path(__file__).resolve().parents[1]
binary = args.binary.resolve(strict=True)
app = args.output.absolute()
if app.suffix != ".app" or app.exists():
    parser.error("output must be a new .app path")
if not binary.is_file() or not os.access(binary, os.X_OK):
    parser.error("binary must be an executable file")
metadata = tomllib.loads((root / "crates/zed/Cargo.toml").read_text())["package"]
bundle = metadata["metadata"]["bundle-" + args.channel]
contents = app / "Contents"
resources = contents / "Resources"
resources.mkdir(parents=True)
(contents / "MacOS").mkdir()
shutil.copy2(binary, contents / "MacOS/Hulder")
info = dict(CFBundleName=bundle["name"], CFBundleDisplayName=bundle["name"],
    CFBundleExecutable="Hulder", CFBundleIdentifier=bundle["identifier"],
    CFBundlePackageType="APPL", CFBundleShortVersionString=metadata["version"],
    CFBundleVersion=metadata["version"], NSHighResolutionCapable=True,
    LSMinimumSystemVersion="11.0", CFBundleIconFile="Hulder.icns")
for fragment in (root / "crates/zed/resources/info").glob("*.plist"):
    info.update(plistlib.loads(("<plist version=\"1.0\"><dict>" + fragment.read_text() + "</dict></plist>").encode()))
(contents / "Info.plist").write_bytes(plistlib.dumps(info))
# App and document icons are separate assets.
shutil.copy2(root / "crates/zed/resources/Hulder.icns", resources / "Hulder.icns")
shutil.copy2(root / "crates/zed/resources/Document.icns", resources / "Document.icns")
for name in ("LICENSE-GPL", "LICENSE-APACHE", "HULDER_GUIDE.md", "RELEASE.md"):
    shutil.copy2(root / name, resources / name)
shutil.copy2(root / "assets/licenses.md", resources / "THIRD-PARTY-LICENSES.md")
subprocess.run(["codesign", "--force", "--sign", "-", str(app)], check=True)
subprocess.run(["codesign", "--verify", "--deep", "--strict", str(app)], check=True)
print(f"Packaged Hulder ({args.channel}): {app}. Locally signed; Developer ID signing and notarization are required before public binary distribution.")
