"""Package an already-tested standalone daemon, without building the app."""
import argparse
import hashlib
import json
from pathlib import Path
import tarfile
import zipfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--target", required=True, choices=[
        "x86_64-pc-windows-msvc", "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu", "x86_64-apple-darwin", "aarch64-apple-darwin",
    ])
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    source = Path(__file__).resolve().parents[3]
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    files = [(binary, binary.name), (source / "LICENSE", "LICENSE"),
             (source / "server/sync/README.md", "README.md")]
    stem = "risunest-sync-server-" + args.target
    if "windows" in args.target:
        archive = output / (stem + ".zip")
        with zipfile.ZipFile(archive, "x", compression=zipfile.ZIP_DEFLATED) as bundle:
            for path, name in files:
                bundle.write(path, stem + "/" + name)
    else:
        archive = output / (stem + ".tar.gz")
        with tarfile.open(archive, "x:gz") as bundle:
            for path, name in files:
                def normalize_mode(info):
                    # Cross-host packaging must not lose the executable bit.
                    info.mode = 0o755 if path == binary else 0o644
                    return info
                bundle.add(path, arcname=stem + "/" + name,
                           recursive=False, filter=normalize_mode)
    with archive.open("rb") as stream:
        checksum = hashlib.file_digest(stream, "sha256").hexdigest()
    (output / (archive.name + ".sha256")).write_text(checksum + "  " + archive.name + "\n", encoding="utf-8")
    print(json.dumps({"target": args.target, "binaryBytes": binary.stat().st_size,
                      "archiveBytes": archive.stat().st_size, "sha256": checksum}))


if __name__ == "__main__":
    main()
