#!/usr/bin/env python3
"""Validate current P/H macOS package and GUI evidence; write one new receipt.

This reads saved inputs and current Git identities only. It never builds,
launches, signs, or runs a GUI. The output path is mandatory and must not exist.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
import tomllib
from datetime import datetime, timezone
from pathlib import Path
from typing import Any


if sys.version_info < (3, 11):
    raise SystemExit("make-mac-aggregate-r376.py requires Python 3.11 or newer")

HERE = Path(__file__).resolve().parent
BASE = HERE.parents[1]
PRODUCT = BASE / "lingxi-app"
HARNESS = BASE / "harness-runtime"
SDK = BASE / "llm-client"
PRODUCT_PACKAGE_PARENT = Path(
    "/Users/luolingfeng/lingxi/.buildcache/claude-alignment-integration-r51/packages"
)

NATIVE_VERSION = "2.1.295"
SDK_SHA = "a363c5b3a237d53eea12c3abfb3b98648ef15d3e"
H_SHA = "44f2eef5baf8ae16254934dbca451cbfd528d955"
P_SHA = "17593eaa66dc762d5c9c581ba348398c914bdbcd"
MOBILE_SHA = "6b033d41a2b6b736fda1e8f996f55a4b20bd885d"
PINNED_BUN = {
    "version": "1.4.3",
    "revision": "c6da4a4d3010e5553438c60f6bd76d981976867c",
    "source_sha256": "28c7dd0a06916aba284d570ef44a2d70e9c538eaf2ba58dfe0910525563b3f79",
}
SCHEMA = "mac-final-r376-v1"
CUA_SCHEMA = "mac-cua-evidence-r376-v1"
EXPECTED_PINS = {
    "llm-client": SDK_SHA,
    "harness-runtime": H_SHA,
    "lingxi-app": P_SHA,
}


class EvidenceError(RuntimeError):
    pass


def require(condition: bool, message: str) -> None:
    if not condition:
        raise EvidenceError(message)


def regular_file(raw_path: Path, label: str) -> Path:
    path = raw_path.expanduser().absolute()
    require(not path.is_symlink(), f"{label} must not be a symlink: {path}")
    require(path.is_file(), f"{label} is missing or not a regular file: {path}")
    require(path.stat().st_size > 0, f"{label} is empty: {path}")
    return path.resolve(strict=True)


def regular_directory(raw_path: Path, label: str) -> Path:
    path = raw_path.expanduser().absolute()
    require(not path.is_symlink(), f"{label} must not be a symlink: {path}")
    require(path.is_dir(), f"{label} is missing or not a directory: {path}")
    return path.resolve(strict=True)


def sha_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def sha_file(path: Path) -> str:
    return sha_bytes(path.read_bytes())


def file_ref(path: Path, role: str) -> dict[str, Any]:
    return {
        "role": role,
        "path": str(path.resolve(strict=True)),
        "bytes": path.stat().st_size,
        "sha256": sha_file(path),
    }


def read_text(path: Path, label: str) -> str:
    try:
        return path.read_text(encoding="utf-8", errors="strict")
    except (OSError, UnicodeError) as error:
        raise EvidenceError(f"{label} is not readable UTF-8: {path}") from error


def read_json(path: Path, label: str) -> dict[str, Any]:
    try:
        value = json.loads(path.read_text(encoding="utf-8", errors="strict"))
    except (OSError, UnicodeError, json.JSONDecodeError) as error:
        raise EvidenceError(f"{label} is not valid UTF-8 JSON: {path}") from error
    require(isinstance(value, dict), f"{label} must be a JSON object")
    return value


def command_output(*args: str, cwd: Path) -> str:
    result = subprocess.run(
        args,
        cwd=cwd,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    require(result.returncode == 0, f"read-only identity command failed: {args}")
    return result.stdout.strip()


def check_repository(path: Path, expected: str, label: str) -> None:
    require(path.is_dir() and not path.is_symlink(), f"{label} checkout is missing or symlinked")
    actual = command_output("git", "rev-parse", "HEAD", cwd=path)
    require(actual == expected, f"{label} HEAD is {actual}, expected {expected}")
    status = command_output(
        "git", "status", "--porcelain=v1", "--untracked-files=no", cwd=path
    )
    require(not status, f"{label} has tracked working-tree changes")


def check_product_sdk_pin() -> None:
    manifest_path = PRODUCT / "Cargo.toml"
    lock_path = PRODUCT / "Cargo.lock"
    try:
        manifest = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
        lock = tomllib.loads(lock_path.read_text(encoding="utf-8"))
    except (OSError, tomllib.TOMLDecodeError) as error:
        raise EvidenceError("Product manifest or lockfile is unreadable") from error

    url = "https://github.com/lingxi-coder/llm-client.git"
    direct = [
        dep
        for dep in manifest.get("workspace", {}).get("dependencies", {}).values()
        if isinstance(dep, dict) and dep.get("git") == url
    ]
    require(len(direct) == 1 and direct[0].get("rev") == SDK_SHA,
            "Product direct SDK pin differs from frozen SDK SHA")
    expected = f"git+{url}?rev={SDK_SHA}#{SDK_SHA}"
    rows = [
        package.get("source", "")
        for package in lock.get("package", [])
        if package.get("source", "").startswith(f"git+{url}?")
    ]
    require(len(rows) == 5 and all(row == expected for row in rows),
            "Product Cargo.lock SDK sources do not all match frozen SDK SHA")


def one_match(pattern: str, text: str, label: str) -> re.Match[str]:
    matches = list(re.finditer(pattern, text, re.MULTILINE))
    require(len(matches) == 1, f"{label} must appear exactly once; found {len(matches)}")
    return matches[0]


def check_exit_file(path: Path, label: str) -> dict[str, Any]:
    text = read_text(path, label)
    require(text.strip() == "exit_code=0", f"{label} must contain exactly exit_code=0")
    return file_ref(path, label)


def check_wrapper_stdout(path: Path) -> tuple[str, dict[str, Any]]:
    text = read_text(path, "package wrapper stdout")
    guard_objects: list[dict[str, Any]] = []
    for line in text.splitlines():
        candidate = line.strip()
        if not candidate.startswith("{"):
            continue
        try:
            value = json.loads(candidate)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and value.get("status") == "guarded":
            guard_objects.append(value)
    require(len(guard_objects) == 3,
            f"expected the wrapper's three successful source-guard records, found {len(guard_objects)}")
    expected_counts = {
        "harness_manifest": 39,
        "harness_lock": 78,
        "sdk_manifest": 1,
        "sdk_lock": 5,
        "mobile_manifest": 3,
        "mobile_lock": 5,
    }
    for index, guard in enumerate(guard_objects, start=1):
        require(guard.get("pins") == expected_counts,
                f"source guard {index} did not verify expected Product dependency rows")
        tools = guard.get("tools", {})
        require(tools.get("bun") == PINNED_BUN["version"],
                f"source guard {index} did not verify Bun 1.4.3")
        require(all("1.94.0" in tools.get(key, "") for key in ("cargo", "rustc", "rustdoc")),
                f"source guard {index} did not verify Rust 1.94.0")
        require(isinstance(guard.get("free_bytes"), int) and guard["free_bytes"] >= 3 * 1024**3,
                f"source guard {index} did not pass the 3 GiB disk floor")
    wrapper_root = one_match(
        r"^Flare package verified by normal wrapper: (.+)$", text,
        "normal package wrapper success sentinel",
    ).group(1)
    return wrapper_root, file_ref(path, "package wrapper stdout")


def check_package_log(path: Path, wrapper_root: str, app_arg: Path, zip_arg: Path) -> dict[str, Any]:
    text = read_text(path, "Flare package log")
    expected_header = f"Flare package: Product {P_SHA} / Harness {H_SHA}"
    require(text.splitlines()[0] == expected_header,
            "Flare package log is not bound to current P/H pins")
    logged_root = one_match(
        r"^Flare package verified by normal wrapper: (.+)$", text,
        "normal package wrapper log sentinel",
    ).group(1)
    require(Path(logged_root).expanduser().absolute() == Path(wrapper_root).expanduser().absolute(),
            "wrapper stdout and package log identify different output roots")

    output_root = regular_directory(Path(wrapper_root), "fresh Flare output root")
    package_parent = regular_directory(PRODUCT_PACKAGE_PARENT, "package output parent")
    require(output_root.parent == package_parent,
            "Flare output root is outside the dedicated package parent")
    require(output_root.name.startswith(f"p-{P_SHA[:10]}-r342-"),
            "package root does not identify the current P candidate and r342 wrapper")

    app_logged = one_match(r"^\[package:mac\] app: (.+)$", text, "Flare app path").group(1)
    zip_logged = one_match(r"^\[package:mac\] zip: (.+)$", text, "Flare ZIP path").group(1)
    require(Path(app_logged).expanduser().absolute() == app_arg,
            "selected app does not match the normal Flare package log")
    require(Path(zip_logged).expanduser().absolute() == zip_arg,
            "selected ZIP does not match the normal Flare package log")
    require(app_arg.is_relative_to(output_root) and zip_arg.is_relative_to(output_root),
            "app or ZIP is outside the fresh package output root")
    require(app_arg.name == "LingXi Code.app", "app artifact is not LingXi Code.app")

    expected_zip_sha = one_match(
        r"^\[package:mac\] sha256: ([0-9a-f]{64})$", text, "Flare ZIP SHA-256"
    ).group(1)
    actual_zip_sha = sha_file(zip_arg)
    require(actual_zip_sha == expected_zip_sha, "Flare ZIP bytes differ from logged SHA-256")
    verify_zip = one_match(
        r"^\[verify:package\] ZIP ([0-9]+) bytes, SHA-256 ([0-9a-f]{64})$",
        text,
        "normal wrapper ZIP verification",
    )
    require(int(verify_zip.group(1)) == zip_arg.stat().st_size
            and verify_zip.group(2) == actual_zip_sha,
            "normal wrapper ZIP size/hash verification does not match artifact")
    require(f"[package:mac:flare] verified app: {app_arg}" in text,
            "normal Flare wrapper did not verify the selected app")
    require(any(line.startswith("[verify:package] OK ") for line in text.splitlines()),
            "normal Flare wrapper static verifier did not report OK")
    require(any(line.startswith("[verify:package:smoke] OK ") for line in text.splitlines()),
            "normal Flare wrapper packaged smoke did not report OK")
    return {
        "output_root": str(output_root),
        "zip_sha256": actual_zip_sha,
        "zip_bytes": zip_arg.stat().st_size,
        "wrapper_log": file_ref(path, "Flare package log"),
    }


def check_static_audit(path: Path, app: Path) -> dict[str, Any]:
    text = read_text(path, "independent static audit log")
    require(f"[mac-package-audit] Product {P_SHA}" in text,
            "independent static audit is not bound to final Product SHA")
    require(f"[mac-package-audit] Harness Cargo.lock {H_SHA}" in text,
            "independent static audit is not bound to final Harness SHA")
    require(f"[mac-package-audit] static package verification passed: {app}" in text,
            "independent static audit did not pass the selected app")
    asar = regular_file(app / "Contents/Resources/app.asar", "packaged app.asar")
    asar_sha = sha_file(asar)
    audit_asar_sha = one_match(
        r"^\[mac-package-audit\] app\.asar SHA-256 ([0-9a-f]{64})$",
        text,
        "independent app.asar digest",
    ).group(1)
    require(asar_sha == audit_asar_sha, "app.asar bytes differ from independent audit")
    bun_match = one_match(
        r"^\[mac-package-audit\] Bun ([^ ]+) revision ([0-9a-f]{40}); source SHA-256 ([0-9a-f]{64}); sealed SHA-256 ([0-9a-f]{64})$",
        text,
        "packaged Bun identity",
    )
    require(
        (bun_match.group(1), bun_match.group(2), bun_match.group(3))
        == (PINNED_BUN["version"], PINNED_BUN["revision"], PINNED_BUN["source_sha256"]),
        "independent audit reports a different Bun source pin",
    )
    return {
        "status": "passed",
        "app_asar": file_ref(asar, "packaged app.asar"),
        "bun": {
            "version": bun_match.group(1),
            "revision": bun_match.group(2),
            "source_sha256": bun_match.group(3),
            "sealed_sha256": bun_match.group(4),
        },
        "log": file_ref(path, "independent static audit log"),
    }


def check_driver(path: Path, log_path: Path, exit_ref: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any]]:
    result = read_json(path, "packaged Host driver result")
    require(result.get("outcome") == "passed" and result.get("stage") == "complete",
            "packaged Host driver did not terminate passed/complete")
    require(result.get("providerCredentialCleanup") == "cleared",
            "packaged Host driver did not clear the synthetic provider credential")
    app_info = result.get("app", {})
    require(app_info.get("bundleName") == "LingXi Code.app"
            and app_info.get("bundleId") == "com.lingxi.code.development"
            and isinstance(app_info.get("version"), str) and app_info["version"],
            "packaged Host driver app identity is incomplete")
    bun = result.get("packagedBun", {})
    require((bun.get("version"), bun.get("revision"))
            == (PINNED_BUN["version"], PINNED_BUN["revision"])
            and isinstance(bun.get("sha256"), str)
            and re.fullmatch(r"[0-9a-f]{64}", bun["sha256"]) is not None,
            "packaged Host driver Bun identity is incomplete or wrong")
    handoff = result.get("handoff", {})
    app_pid = handoff.get("appPid")
    require(isinstance(app_pid, int) and app_pid > 0,
            "packaged Host driver has no owned GUI app PID")
    require(handoff.get("stopReason") in {"control-file", "timeout"},
            "packaged Host driver did not finish its bounded GUI handoff")

    log_text = read_text(log_path, "packaged Host driver log")
    handoff_lines = [line[len("GUI_HANDOFF "):] for line in log_text.splitlines()
                     if line.startswith("GUI_HANDOFF ")]
    require(len(handoff_lines) == 1, "Host driver log must contain exactly one GUI_HANDOFF record")
    try:
        handoff_log = json.loads(handoff_lines[0])
    except json.JSONDecodeError as error:
        raise EvidenceError("Host driver GUI_HANDOFF record is invalid JSON") from error
    require(handoff_log.get("appPid") == app_pid,
            "Host driver result PID differs from the live handoff record")

    final_lines: list[dict[str, Any]] = []
    for line in log_text.splitlines():
        if not line.startswith("{"):
            continue
        try:
            entry = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(entry, dict) and "resultPath" in entry:
            final_lines.append(entry)
    require(len(final_lines) == 1, "Host driver log must contain one terminal result record")
    final_line = final_lines[0]
    require(Path(final_line.get("resultPath", "")).expanduser().absolute() == path
            and final_line.get("outcome") == "passed"
            and final_line.get("stage") == "complete",
            "Host driver log terminal record differs from result JSON")
    return result, {
        "result": file_ref(path, "packaged Host driver result"),
        "log": file_ref(log_path, "packaged Host driver log"),
        "exit": exit_ref,
        "app_pid": app_pid,
        "stop_reason": handoff["stopReason"],
    }


def check_cua(
    path: Path,
    screenshot_path: Path,
    app: Path,
    driver_result_path: Path,
    driver_result: dict[str, Any],
) -> tuple[dict[str, Any], dict[str, Any]]:
    cua = read_json(path, "visible CUA observation")
    require(cua.get("schema") == CUA_SCHEMA and cua.get("status") == "passed",
            "visible CUA observation schema/status is not passed")
    require(cua.get("observation_method") == "actual-visible-CUA",
            "CUA record must identify an actual visible UI interaction")
    require(cua.get("native_version") == NATIVE_VERSION,
            "CUA record must retain the frozen Native version")
    recorded_at = cua.get("recorded_at")
    require(isinstance(recorded_at, str), "CUA record must include its observation timestamp")
    try:
        parsed_time = datetime.fromisoformat(recorded_at.replace("Z", "+00:00"))
    except ValueError as error:
        raise EvidenceError("CUA observation timestamp is not ISO-8601") from error
    require(parsed_time.tzinfo is not None, "CUA observation timestamp must include a timezone")
    require(cua.get("product_sha") == P_SHA and cua.get("harness_sha") == H_SHA
            and cua.get("sdk_sha") == SDK_SHA,
            "CUA record is not bound to final P/H/SDK pins")
    require(Path(cua.get("app_path", "")).expanduser().absolute() == app,
            "CUA observed app path differs from the verified package")
    handoff_pid = driver_result.get("handoff", {}).get("appPid")
    require(cua.get("app_pid") == handoff_pid,
            "CUA app PID does not match the Host driver's GUI process")
    require(cua.get("driver_result_sha256") == sha_file(driver_result_path),
            "CUA record does not bind the exact Host driver result")
    screenshot = regular_file(screenshot_path, "actual CUA screenshot")
    require(Path(cua.get("screenshot_path", "")).expanduser().absolute() == screenshot,
            "CUA record screenshot path differs from selected image")
    screenshot_sha = sha_file(screenshot)
    require(cua.get("screenshot_sha256") == screenshot_sha,
            "CUA screenshot digest does not match its recorded hash")
    image_prefix = screenshot.read_bytes()[:8]
    require(image_prefix.startswith(b"\x89PNG\r\n\x1a\n") or image_prefix[:3] == b"\xff\xd8\xff",
            "CUA screenshot must be PNG or JPEG image bytes")

    visible = cua.get("visible_ui", {})
    require(visible == {
        "title": "Packaged Host Client UI",
        "input_label": "Message",
        "input_placeholder": "Type a value",
        "button_label": "Submit",
        "initial_result": "Waiting for a value.",
    }, "CUA visible UI observation does not match the required packaged form")
    expected_actions = [
        {
            "typeText": "packaged-host-cua-value",
            "visible_input": "packaged-host-cua-value",
            "visible_result": "Input callback received: packaged-host-cua-value",
            "focus": "Message",
        },
        {
            "typeText": "xy",
            "visible_input": "packaged-host-cua-valuexy",
            "visible_result": "Input callback received: packaged-host-cua-valuexy",
            "focus": "Message",
        },
        {
            "click": "Submit",
            "visible_result": "Submit callback accepted: packaged-host-cua-valuexy",
            "focus": "Submit",
        },
    ]
    require(cua.get("native_cua_actions") == expected_actions,
            "CUA actions do not record the required visible input and Submit results")
    return cua, {
        "record": file_ref(path, "actual visible CUA observation"),
        "screenshot": file_ref(screenshot, "actual CUA screenshot"),
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--wrapper-log", required=True, type=Path)
    parser.add_argument("--wrapper-stdout", required=True, type=Path)
    parser.add_argument("--wrapper-exit", required=True, type=Path)
    parser.add_argument("--static-audit-log", required=True, type=Path)
    parser.add_argument("--static-exit", required=True, type=Path)
    parser.add_argument("--app", required=True, type=Path)
    parser.add_argument("--zip", required=True, type=Path)
    parser.add_argument("--driver-log", required=True, type=Path)
    parser.add_argument("--driver-result", required=True, type=Path)
    parser.add_argument("--driver-exit", required=True, type=Path)
    parser.add_argument("--cua-observations", required=True, type=Path)
    parser.add_argument("--cua-screenshot", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    wrapper_log = regular_file(args.wrapper_log, "Flare package log")
    wrapper_stdout = regular_file(args.wrapper_stdout, "package wrapper stdout")
    wrapper_exit = check_exit_file(regular_file(args.wrapper_exit, "package wrapper exit"), "package wrapper exit")
    static_log = regular_file(args.static_audit_log, "independent static audit log")
    static_exit = check_exit_file(regular_file(args.static_exit, "independent static audit exit"), "static audit exit")
    driver_log = regular_file(args.driver_log, "packaged Host driver log")
    driver_result_path = regular_file(args.driver_result, "packaged Host driver result")
    driver_exit = check_exit_file(regular_file(args.driver_exit, "Host driver exit"), "Host driver exit")
    cua_path = regular_file(args.cua_observations, "actual visible CUA observation")
    app = regular_directory(args.app, "verified Flare app")
    zip_path = regular_file(args.zip, "verified Flare ZIP")

    check_repository(PRODUCT, P_SHA, "Product")
    check_repository(HARNESS, H_SHA, "Harness")
    check_repository(SDK, SDK_SHA, "SDK")
    check_product_sdk_pin()

    wrapper_root, wrapper_stdout_ref = check_wrapper_stdout(wrapper_stdout)
    package_data = check_package_log(wrapper_log, wrapper_root, app, zip_path)
    static_data = check_static_audit(static_log, app)
    driver_result, driver_data = check_driver(driver_result_path, driver_log, driver_exit)
    require(driver_result["packagedBun"]["sha256"] == static_data["bun"]["sealed_sha256"],
            "Host driver Bun digest differs from the package's independent static audit")
    version = driver_result["app"]["version"]
    require(app.parent.name == f"LingXi-Code-{version}-mac-arm64"
            and zip_path.name == f"LingXi-Code-{version}-mac-arm64.zip",
            "package paths do not match the Host driver's versioned app identity")
    cua, cua_data = check_cua(
        cua_path,
        regular_file(args.cua_screenshot, "actual CUA screenshot"),
        app,
        driver_result_path,
        driver_result,
    )
    require(static_exit["bytes"] > 0 and wrapper_exit["bytes"] > 0,
            "exit-code inputs must be retained as non-empty files")

    wrapper_script = regular_file(HERE / "run-repaired-h-package.sh", "r376 package wrapper")
    package_runner = regular_file(BASE / "acceptance/product-final-r342-candidate/package-flare.sh",
                                  "guarded Flare package runner")
    source_guard = regular_file(BASE / "acceptance/product-final-r342-candidate/source_guard.py",
                                "package source guard")
    static_verifier = regular_file(PRODUCT / "acceptance/mac-package-audit-r354-prep/verify-final-macos-package.mjs",
                                   "independent package verifier")
    host_driver = regular_file(BASE / "acceptance/product-js-gates-fbe2a847-20261014/packaged-host-driver-r375/run-packaged-host-mod-smoke.mjs",
                               "packaged Host driver source")
    wrapper_script_text = read_text(wrapper_script, "r376 package wrapper")
    runner_text = read_text(package_runner, "guarded Flare package runner")
    require(f"readonly sdk_sha={SDK_SHA}" in wrapper_script_text
            and f"readonly sdk_sha={SDK_SHA}" in runner_text,
            "the package wrapper no longer pins the frozen SDK SHA")
    require(f"readonly mobile_sha={MOBILE_SHA}" in wrapper_script_text
            and f"readonly mobile_sha={MOBILE_SHA}" in runner_text,
            "the package wrapper no longer pins the accepted Mobile SHA")

    output = args.output.expanduser().absolute()
    receipt_dir = regular_directory(HERE / "receipts", "receipt output directory")
    require(output.parent.resolve(strict=True) == receipt_dir,
            "output must be a direct child of final-ledger-r376/receipts")
    require(not output.exists() and not output.is_symlink(),
            f"refusing to overwrite an existing receipt: {output}")

    cleanup = {"providerCredentialCleanup": "cleared"}
    cleanup[f"app_pid_{driver_data['app_pid']}"] = "exited"
    receipt = {
        "schema": SCHEMA,
        "status": "passed",
        "recorded_at": datetime.now(timezone.utc).isoformat(),
        "native_version": NATIVE_VERSION,
        "bun": PINNED_BUN["version"],
        "pins": EXPECTED_PINS,
        "signed_package": {
            "path": str(app),
            "zip": str(zip_path),
            "zip_sha256": package_data["zip_sha256"],
            "app_asar_sha256": static_data["app_asar"]["sha256"],
            "static": "passed",
            "startup_smoke": "passed",
            "independent_audit": "passed",
            "actual_cua_gui": "passed",
            "typed": "packaged-host-cua-valuexy",
            "submit_result": "Submit callback accepted: packaged-host-cua-valuexy",
            "bundle_id": driver_result["app"]["bundleId"],
            "version": driver_result["app"]["version"],
        },
        "driver": {
            "outcome": driver_result["outcome"],
            "stage": driver_result["stage"],
            "app_pid": driver_data["app_pid"],
            "handoff_stop_reason": driver_data["stop_reason"],
            "provider_credential_cleanup": driver_result["providerCredentialCleanup"],
        },
        "cua": {
            "schema": cua["schema"],
            "observation_method": cua["observation_method"],
            "app_pid": cua["app_pid"],
            "screenshot_sha256": cua["screenshot_sha256"],
            "visible_ui": cua["visible_ui"],
            "native_cua_actions": cua["native_cua_actions"],
        },
        "evidence_files": [
            wrapper_exit,
            wrapper_stdout_ref,
            package_data["wrapper_log"],
            static_exit,
            static_data["log"],
            static_data["app_asar"],
            driver_data["exit"],
            driver_data["log"],
            driver_data["result"],
            *cua_data.values(),
            file_ref(zip_path, "signed package ZIP"),
            file_ref(wrapper_script, "r376 package wrapper source"),
            file_ref(package_runner, "guarded Flare package runner source"),
            file_ref(source_guard, "package source guard source"),
            file_ref(static_verifier, "independent package verifier source"),
            file_ref(host_driver, "packaged Host driver source"),
        ],
        "cleanup": cleanup,
        "limits": [
            "Development signing only; no notarization or stapling claim.",
            "Actual visible CUA is scoped to the recorded packaged form and Submit flow.",
            "No physical-device GUI acceptance is implied.",
            "No optional sidebar drag is claimed by this receipt.",
            "This macOS package receipt does not certify all 42 Native parity contracts.",
        ],
    }
    payload = (json.dumps(receipt, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    try:
        descriptor = os.open(output, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except OSError as error:
        raise EvidenceError(f"cannot create explicit receipt output {output}: {error}") from error
    with os.fdopen(descriptor, "wb") as stream:
        stream.write(payload)
        stream.flush()
        os.fsync(stream.fileno())
    print(f"created {output} sha256={sha_bytes(payload)}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except EvidenceError as error:
        print(f"mac aggregate receipt rejected: {error}", file=sys.stderr)
        raise SystemExit(2)
