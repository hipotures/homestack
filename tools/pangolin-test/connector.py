"""Linux process ownership for the experiment's detached Pangolin CLI Site."""
import fcntl
import os
from pathlib import Path
import resource
import select
import signal
import stat
import subprocess
import sys
import time

if __name__ == '__main__':
    # Isolated Python omits the script directory; import only our known sibling.
    sys.path.insert(0, str(Path(__file__).resolve().parent))
from ovh_dns import DnsFailure


def site_config(credentials):
    return {**credentials, "native": False, "nativeMain": False, "disableClients": True,
            "disableSsh": True, "logLevel": "ERROR", "metrics": False, "otlp": False,
            "pprof": False, "enforceHcCert": True}


def environment():
    result = {key: os.environ[key] for key in ("PATH", "LANG", "HOME") if key in os.environ}
    result.update({"PANGOLIN_CLI_LOG_FILE": os.devnull,
                   "PANGOLIN_CLI_DISABLE_COMPANION_MODE": "true",
                   "PANGOLIN_CLI_DISABLE_UPDATE_CHECK": "true"})
    return result


def process_info(pid):
    try:
        proc = Path('/proc') / str(pid)
        if proc.stat().st_uid != os.getuid():
            raise DnsFailure("Recorded connector belongs to another OS account; refusing access.")
        fields = (proc / 'stat').read_text().rsplit(')', 1)[1].split()
        if fields[0] in ('Z', 'X'):
            return None
        return {"start_ticks": fields[19], "session": int(fields[3]),
                "boot_id": Path('/proc/sys/kernel/random/boot_id').read_text().strip()}
    except (FileNotFoundError, ProcessLookupError):
        return None


class PersistentConnector:
    def __init__(self, workspace, run):
        self.work, self.run = workspace, run
        self.child = None
        self.config = workspace.path / 'connector-site.json'

    def lease(self):
        fd = os.open(self.work.path / 'connector.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600:
            os.close(fd)
            raise DnsFailure("Connector lock must be a private regular file owned by this user (0600).")
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            return fd
        except BlockingIOError:
            os.close(fd)
            return None

    def current(self):
        record = self.work.load('connector.json')
        if record is None:
            return None
        if (record.get('run') != self.run or record.get('config') != str(self.config)
                or type(record.get('pid')) is not int or record['pid'] <= 1
                or not isinstance(record.get('binary'), str)):
            raise DnsFailure("Invalid connector ownership record; refusing adoption or termination.")
        info = process_info(record['pid'])
        if info is None:
            return None
        if any(info[key] != record.get(key) for key in ('start_ticks', 'boot_id')):
            raise DnsFailure("Recorded PID was reused; refusing adoption or termination.")
        if info['session'] != record['pid']:
            raise DnsFailure("Connector process session changed; refusing termination.")
        try:
            proc = Path('/proc') / str(record['pid'])
            argv = (proc / 'cmdline').read_bytes().rstrip(b'\0').split(b'\0')
            expected = [record['binary'], 'up', 'site', '--config-file', str(self.config)]
            exe = os.readlink(proc / 'exe')
            if argv != [os.fsencode(x) for x in expected] or exe != record['binary']:
                raise DnsFailure("Connector executable/arguments changed; refusing adoption or termination.")
        except (FileNotFoundError, ProcessLookupError):
            return None
        return record

    def running(self):
        record = self.current()
        lease = self.lease()
        if lease is not None:
            os.close(lease)
            if record is not None:
                raise DnsFailure("Connector lost its ownership lock; refusing duplicate startup.")
            return None
        if record is None:
            raise DnsFailure("Connector lock is held without verifiable ownership; refusing duplicate startup.")
        return record

    def start(self, binary, credentials):
        record = self.running()
        if record is not None:
            return {"local_connector": "running", "pid": record['pid'], "reused": True}
        lease = self.lease()
        if lease is None:
            raise DnsFailure("Another connector startup owns this experiment; refusing duplicate startup.")
        try:
            binary = str(Path(binary).resolve(strict=True))
            self.work.save('connector-site.json', site_config(credentials))
            # The launcher sets limits before exec; no credentials enter argv or env.
            self.child = subprocess.Popen([sys.executable, '-I', str(Path(__file__).resolve()),
                binary, str(self.config), str(lease)], stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=environment(),
                start_new_session=True, pass_fds=(lease,), cwd=self.work.path)
            info = process_info(self.child.pid)
            if info is None:
                raise DnsFailure("Pangolin CLI launcher exited; sensitive output discarded.")
            record = {**info, "run": self.run, "pid": self.child.pid,
                      "binary": binary, "config": str(self.config),
                      "starter_pid": os.getpid(), "starter_info": process_info(os.getpid())}
            # Save immediately: interrupted startup keeps a non-secret recovery record.
            self.work.save('connector.json', record)
            end = time.monotonic() + 5
            while time.monotonic() < end:
                if self.child.poll() is not None:
                    raise DnsFailure("Pangolin CLI exited during startup; sensitive output discarded.")
                try:
                    if self.current() is not None:
                        return {"local_connector": "running", "pid": record['pid'], "reused": False}
                except DnsFailure:
                    # exec changes the launcher's argv once; never accept an unknown process.
                    pass
                time.sleep(0.02)
            raise DnsFailure("CLI startup ownership could not be verified; preserve state, do not launch another connector.")
        finally:
            # The CLI inherits this same locked file description for its lifetime.
            os.close(lease)

    def stop(self):
        record = self.running()
        if record is None:
            return {"local_connector": "stopped", "already_stopped": True}
        try:
            pidfd = os.pidfd_open(record['pid'])
        except ProcessLookupError:
            return {"local_connector": "stopped", "already_stopped": True}
        try:
            if self.current() != record:
                raise DnsFailure("Connector identity changed before stop; no signal sent.")
            signal.pidfd_send_signal(pidfd, signal.SIGTERM)
            if not select.select([pidfd], [], [], 5)[0]:
                signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                if not select.select([pidfd], [], [], 5)[0]:
                    raise DnsFailure("Owned connector did not stop; preserve ownership state.")
        except ProcessLookupError:
            pass
        finally:
            os.close(pidfd)
        if self.child is not None:
            self.child.wait(timeout=1)
        self.work.save('connector.json', {**record, "stopped": True})
        return {"local_connector": "stopped", "pid": record['pid']}


def launch():
    """Private exec entry point, not a credential-taking CLI."""
    if len(sys.argv) != 4:
        raise SystemExit(2)
    binary, config, lease = sys.argv[1:]
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    # Site mode has a separate self-updater: prohibit regular-file writes so it
    # cannot replace the pinned, verified binary. Socket/device writes still work.
    resource.setrlimit(resource.RLIMIT_FSIZE, (0, 0))
    os.set_inheritable(int(lease), True)
    os.execve(binary, [binary, 'up', 'site', '--config-file', config], environment())


if __name__ == '__main__':
    try:
        launch()
    except BaseException:
        raise SystemExit(1) from None
