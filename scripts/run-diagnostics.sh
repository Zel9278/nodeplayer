#!/bin/sh
# POSIX watchdog: kills only the dedicated session/process group it starts.
# Requires python3 (stdlib only). Children deliberately escaping their process group
# are outside this M0 runner's containment; this is not a security sandbox.
set -eu
if [ "$#" -lt 2 ]; then
  echo 'usage: run-diagnostics.sh <nodeplayer> <scenario.json> [new-run-directory] [duration-seconds] [extra-nodeplayer-args...]' >&2
  exit 64
fi
exec python3 - "$@" <<'PY'
import datetime, json, os, pathlib, signal, subprocess, sys, time
exe, scenario = sys.argv[1:3]
root = pathlib.Path(sys.argv[3] if len(sys.argv) > 3 else 'diagnostics-runs/run-' + datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ') + '-' + str(os.getpid())).absolute()
seconds = float(sys.argv[4]) if len(sys.argv) > 4 else 30.0
if not 0 < seconds <= 86400:
    raise SystemExit('duration must be > 0 and <= 86400 seconds')
root.parent.mkdir(parents=True, exist_ok=True)
root.mkdir(mode=0o700) # exclusive: never mix an existing run or a normal profile
artifacts = root / 'artifacts'
argv = [str(pathlib.Path(exe).absolute()), '--diagnostic-scenario', str(pathlib.Path(scenario).absolute()), '--diagnostic-output', str(artifacts), '--diagnostic-duration', str(seconds)] + sys.argv[5:]
# The application has seconds; the independent watchdog adds bounded startup/shutdown grace.
deadline = time.monotonic() + seconds + 10.0
child = None
outcome, code, detail = 'aborted', 125, 'launcher did not start'
interrupted = False
for sig in (signal.SIGTERM, signal.SIGINT):
    def interrupt(signum, frame):
        global interrupted
        interrupted = True
    signal.signal(sig, interrupt)
try:
    with (root / 'stdout.log').open('xb') as out, (root / 'stderr.log').open('xb') as err:
        child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=out, stderr=err, start_new_session=True)
        (root / 'process.json').write_text(json.dumps({'pid': child.pid, 'process_group': child.pid, 'watchdog_seconds': seconds + 10, 'tree_scope': 'owned POSIX session/process group; escaping descendants unsupported'}) + '\n')
        while child.poll() is None:
            if interrupted:
                outcome, code, detail = 'aborted', 125, 'runner interrupted'
                break
            if time.monotonic() >= deadline:
                outcome, code, detail = 'timeout', 124, 'external watchdog expired'
                break
            if any((root / name).stat().st_size > 32 * 1024 * 1024 for name in ('stdout.log', 'stderr.log')):
                outcome, code, detail = 'failed', 2, 'stdout/stderr exceeded 32 MiB watchdog threshold'
                break
            time.sleep(0.1)
        else:
            native = child.returncode
            summary = artifacts / 'summary.json'
            if summary.is_file():
                try:
                    result = json.loads(summary.read_text())
                    outcome, code = result['outcome'], int(result['exit_code'])
                    if {'passed': 0, 'failed': 2, 'timeout': 124, 'aborted': 125}.get(outcome) != code:
                        raise ValueError('inconsistent outcome/exit code')
                    detail = 'application summary'
                    if native != 0 and code == 0:
                        outcome, code, detail = 'crash', 3, 'nonzero process exit despite passing summary'
                except (ValueError, KeyError, OSError):
                    outcome, code, detail = 'failed', 2, 'invalid application summary'
            else:
                outcome, code, detail = ('crash', 3, 'process exited without summary') if native != 0 else ('failed', 2, 'missing application summary')
except (OSError, ValueError) as error:
    outcome, code, detail = 'aborted', 125, 'launcher or artifact error: ' + type(error).__name__
finally:
    if child is not None:
        # Always clean the owned group, including children left after the main process exits.
        try: os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError: pass
        try: child.wait(timeout=2)
        except subprocess.TimeoutExpired: pass
        try: os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError: pass
        try: child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            outcome, code, detail = 'failed', 2, 'owned parent could not be reaped'
    result = {'outcome': outcome, 'exit_code': code, 'detail': detail, 'artifacts': str(artifacts), 'pid': child.pid if child else None, 'no_upload': True}
    (root / 'runner-summary.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))
raise SystemExit(code)
PY
