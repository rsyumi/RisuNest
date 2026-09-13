"""Build isolated, synthetic iphoneos artifacts. Does not contact Firebase."""
import hashlib
import json
import os
import pathlib
import plistlib
import shutil
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
ARTIFACTS = ROOT / "artifacts"
BENCH_ID = "io.github.rsyumi.risunest.ios.bench"


def run(args, cwd=ROOT, env=None):
    print("+", " ".join(map(str, args)), flush=True)
    subprocess.run(list(map(str, args)), cwd=cwd, env=env, check=True)


def find_app(directory, identifier):
    matches = []
    for path in directory.rglob("*.app"):
        info_path = path / "Info.plist"
        if not info_path.is_file():
            continue
        info = plistlib.loads(info_path.read_bytes())
        if info.get("CFBundleIdentifier") == identifier and info.get("CFBundleSupportedPlatforms") == ["iPhoneOS"]:
            assert info.get("MinimumOSVersion") == "16.4", info.get("MinimumOSVersion")
            matches.append(path)
    assert matches, "Missing iphoneos app: " + identifier
    # The archive and build output may both contain the same target.
    return sorted(matches, key=lambda path: (".xcarchive" not in str(path), str(path)))[0]


def sign_and_verify(app):
    run(["codesign", "--force", "--deep", "--sign", "-", app])
    run(["codesign", "--verify", "--deep", "--strict", "--verbose=2", app])
    info = plistlib.loads((app / "Info.plist").read_bytes())
    binary = app / info["CFBundleExecutable"]
    run(["lipo", binary, "-verify_arch", "arm64"])
    platform = subprocess.check_output(["xcrun", "vtool", "-show-build", str(binary)], text=True)
    assert "platform IOS\n" in platform or "platform      IOS\n" in platform or any(
        line.split() == ["platform", "IOS"] for line in platform.splitlines()
    ), platform


def patch_targets(plist, app_path):
    targets = [target for config in plist.get("TestConfigurations", []) for target in config.get("TestTargets", [])]
    if not targets:
        targets = [value for key, value in plist.items() if key != "__xctestrun_metadata__" and isinstance(value, dict)]
    assert len(targets) == 1 and targets[0].get("IsUITestBundle"), "Expected one UI test target"
    target = targets[0]
    target["UITargetAppPath"] = app_path
    target["UITargetAppBundleIdentifier"] = BENCH_ID
    dependencies = target.setdefault("DependentProductPaths", [])
    if app_path not in dependencies:
        dependencies.append(app_path)


def main():
    assert sys.platform == "darwin", "Xcode on macOS is required"
    assert os.environ.get("TAURI_ENV_PLATFORM") == "ios", "Explicit iOS build environment required"
    ARTIFACTS.mkdir(exist_ok=True)
    cli = ROOT / "node_modules/@tauri-apps/cli/tauri.js"
    run(["node", cli, "ios", "build", "--ci", "--target", "aarch64", "--no-sign", "--archive-only",
         "--config", "src-tauri/tauri.agent.conf.json"])
    product = find_app(ROOT / "src-tauri/gen/apple", "io.github.rsyumi.risunest")
    product_info = plistlib.loads((product / "Info.plist").read_bytes())
    binary = product / product_info["CFBundleExecutable"]
    body = binary.read_bytes()
    assert all(marker not in body for marker in [b"ios_bench_phase", b"ios_bench_report", b"RISUNEST_IOS_PHASE"]), "Test code in product"
    del body
    sign_and_verify(product)

    run(["pnpm", "exec", "vite", "build", "--mode", "agent", "--config", "benchmarks/ios/vite.config.ts"])
    bench = ROOT / "benchmarks/ios/native"
    run(["node", cli, "ios", "init", "--ci", "--skip-targets-install"], cwd=bench)
    apple = bench / "gen/apple"
    project = apple / "project.yml"
    text = project.read_text()
    assert "node tauri ios xcode-script" in text
    project.write_text(text.replace("node tauri ios xcode-script", "node " + str(cli) + " ios xcode-script"))
    run(["xcodegen", "generate", "--spec", "project.yml"], cwd=apple)
    for path in apple.glob("*_iOS/Info.plist"):
        info = plistlib.loads(path.read_bytes())
        for key in ["NSCameraUsageDescription", "NSLocalNetworkUsageDescription", "UIBackgroundModes"]:
            if key in product_info:
                info[key] = product_info[key]
        info["BGTaskSchedulerPermittedIdentifiers"] = [BENCH_ID + ".generation"]
        info["UIFileSharingEnabled"] = True
        info["LSSupportsOpeningDocumentsInPlace"] = True
        path.write_bytes(plistlib.dumps(info))
    for path in apple.glob("Sources/**/*"):
        if path.is_file() and path.suffix in [".mm", ".h"]:
            path.write_text(path.read_text().replace("start_app", "ios_bench_start"))
    run(["node", cli, "ios", "build", "--ci", "--target", "aarch64", "--no-sign", "--archive-only"], cwd=bench)
    bench_app = find_app(apple, BENCH_ID)

    workspace = ARTIFACTS / "testlab-build"
    workspace.mkdir()
    spec = {"name": "RisuNestUITests", "targets": {"RisuNestUITests": {
        "type": "bundle.ui-testing", "platform": "iOS", "deploymentTarget": "16.4",
        "sources": [str(ROOT / "benchmarks/ios/NativeUITests.swift")],
        "settings": {"base": {"GENERATE_INFOPLIST_FILE": "YES", "PRODUCT_BUNDLE_IDENTIFIER": BENCH_ID + ".uitests"}},
    }}, "schemes": {"RisuNestUITests": {"build": {"targets": {"RisuNestUITests": ["test"]}},
                                      "test": {"targets": ["RisuNestUITests"]}}}}
    (workspace / "project.json").write_text(json.dumps(spec))
    run(["xcodegen", "generate", "--spec", "project.json"], cwd=workspace)
    run(["xcodebuild", "build-for-testing", "-project", "RisuNestUITests.xcodeproj", "-scheme", "RisuNestUITests",
         "-sdk", "iphoneos", "-destination", "generic/platform=iOS", "-derivedDataPath", "DerivedData",
         "CODE_SIGNING_ALLOWED=NO", "ARCHS=arm64"], cwd=workspace)
    products = workspace / "DerivedData/Build/Products"
    debug = products / "Debug-iphoneos"
    shutil.copytree(bench_app, debug / bench_app.name)
    for app in debug.glob("*.app"):
        sign_and_verify(app)
    configs = list(products.glob("*.xctestrun"))
    assert len(configs) == 1, configs
    config = plistlib.loads(configs[0].read_bytes())
    patch_targets(config, "__TESTROOT__/Debug-iphoneos/" + bench_app.name)
    configs[0].write_bytes(plistlib.dumps(config))
    run(["plutil", "-lint", configs[0]])
    run(["zip", "-r", "-y", ARTIFACTS / "RisuNest-ios-testlab.zip", "Debug-iphoneos", configs[0].name], cwd=products)
    payload = workspace / "Payload"
    payload.mkdir()
    shutil.copytree(product, payload / product.name)
    run(["zip", "-r", "-y", ARTIFACTS / "RisuNest-ios-device-agent.ipa", "Payload"], cwd=workspace)
    metadata = {
        "source": json.loads((ROOT / ".build-lab/source.json").read_text()),
        "xcode": subprocess.check_output(["xcodebuild", "-version"], text=True).strip(),
        "architecture": "arm64", "platform": "iphoneos", "minimumIOS": "16.4",
        "signing": "ad-hoc; locally verified; Test Lab re-signing acceptance unverified",
        "firebaseRun": False,
        "files": {path.name: {"bytes": path.stat().st_size, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                  for path in [ARTIFACTS / "RisuNest-ios-testlab.zip", ARTIFACTS / "RisuNest-ios-device-agent.ipa"]},
    }
    (ARTIFACTS / "testlab-package.json").write_text(json.dumps(metadata, indent=2))


if __name__ == "__main__":
    main()
