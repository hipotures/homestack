"""Reuse the live resize/migration/capacity fixtures under an active Rust controller.

No fixture implementation is duplicated here. Private credentials, server state,
and guest keys live in the disposable work directory, never in public artifacts.
"""
import importlib.util
import json
import os
import pathlib
import signal
import socket
import sys
import traceback

sys.dont_write_bytecode = True
assert os.getpgrp() == os.getpid(), "fixture requires its own controller-owned process group"
repo, report_path, output, result_path, kind, scenario = sys.argv[1:]
repo, output, result_path = map(pathlib.Path, (repo, output, result_path))
report = json.loads(pathlib.Path(report_path).read_text())
assert report["status"] == "running" and report["phase"] == "scenarios"
assert report["controller_pid"] != os.getpid()
os.kill(report["controller_pid"], 0)
work = pathlib.Path(report["temporary_directory"]).resolve(strict=True)
assert output.resolve().is_relative_to(work)
assert pathlib.Path(report["scenario_socket"]).parent.resolve() == work
with socket.socket(socket.AF_UNIX) as proof:
    proof.settimeout(5)
    proof.connect(report["scenario_socket"])
    assert proof.recv(4) == b"E2E\n", "active E2E controller did not confirm ownership"
ssh = pathlib.Path(report["ssh_config"]).read_text()
assert "HostName 127.0.0.1" in ssh and f"Port {int(report['ssh_port'])}" in ssh
assert len(report["nodes"]) in (1, 3)


def load(name):
    spec = importlib.util.spec_from_file_location("hs_e2e_" + name, repo / "scripts" / ("e2e-" + name + ".py"))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


resize = load("resize")
if kind == "resize":
    matrix = resize.Matrix(report, output)
elif kind == "migration":
    assert len(report["nodes"]) == 3
    matrix = load("migrate").Migration(report, output)
elif kind == "capacity":
    matrix = load("capacity").Capacity(report, output)
else:
    raise RuntimeError("unknown concrete fixture")


def interrupted(_signum, _frame):
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    raise TimeoutError("scenario deadline exceeded; fixture is unwinding")


signal.signal(signal.SIGTERM, interrupted)
result = {"status": "FAILED", "error": None, "error_category": None,
          "cleanup": {"status": "FAILED", "error": None}, "observations": [],
          "execution": "current compiled local homestack-server via public HTTP; independent real PVE/guest observations"}
try:
    matrix.prepare()
    if kind == "resize":
        _, _, role, layout = scenario.split(".")
        matrix.case(role, layout, resize.REFUSALS.get(layout))
    else:
        matrix.verify()
    result["status"] = "PASSED"
except Exception as error:
    result["error"] = str(error)
    result["error_category"] = type(error).__name__
    result["failed_step"] = traceback.extract_tb(error.__traceback__)[-1].name
finally:
    try:
        matrix.cleanup()
        result["cleanup"]["status"] = "PASSED"
    except Exception as error:
        result["cleanup"]["error"] = str(error)
    for name in ("baseline.json", "cloud-init.json", "results.json", "jobs.json", "cleanup.json", "migration-cleanup.json"):
        path = output / name
        if path.is_file():
            result["observations"].append({"source": name, "evidence": json.loads(path.read_text())})
    secrets = [matrix.api_token, resize.PASSWORD]
    token_file = getattr(matrix, "token_file", None)
    if token_file and token_file.is_file():
        token = token_file.read_text().strip()
        secrets.extend((token, token.split("=", 1)[-1]))

    def sanitize(value):
        if isinstance(value, dict):
            return {key: "[REDACTED]" if any(word in key.lower() for word in ("secret", "passphrase", "password", "private_key", "authorization")) else sanitize(item) for key, item in value.items()}
        if isinstance(value, list):
            return [sanitize(item) for item in value]
        if isinstance(value, str):
            for secret in secrets:
                if secret:
                    value = value.replace(secret, "[REDACTED]")
        return value

    temporary = result_path.with_suffix(".tmp")
    temporary.write_text(json.dumps(sanitize(result), indent=2) + "\n")
    temporary.replace(result_path)
# Terminate any child left behind by a failed fixture cleanup. The Rust
# controller allocated this worker's separate process group before spawning it.
signal.signal(signal.SIGTERM, signal.SIG_IGN)
os.killpg(os.getpgrp(), signal.SIGTERM)
sys.exit(0 if result["status"] == result["cleanup"]["status"] == "PASSED" else 1)
