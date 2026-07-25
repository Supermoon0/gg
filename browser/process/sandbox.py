"""Renderer process sandbox: resource quotas + privilege reduction.

Applied inside the renderer child at startup, before any page bytes
are parsed, so a hostile or runaway document is bounded by the OS:

- POSIX: setrlimit — CPU seconds (SIGXCPU then SIGKILL ends a spinning
  renderer; the browser's crash recovery restarts it on the next
  commit), address-space bytes (a leak/allocation bomb gets
  MemoryError instead of taking the machine down), open files, no
  core dumps — plus umask 077 and PR_SET_NO_NEW_PRIVS (execve can
  never regain privileges).
- Windows: a Job object with a process-memory cap and an
  active-process limit of 1 (the renderer cannot spawn children),
  kill-on-job-close so no renderer outlives its browser.

Limits are env-tunable (GG_RENDERER_CPU_S, GG_RENDERER_MEMORY_MB,
GG_RENDERER_NOFILE) and GG_RENDERER_SANDBOX=0 disables the whole
sandbox for debugging. apply_renderer_sandbox() returns a report of
what was applied/skipped; the worker sends it back in hello_ack so
the browser can see the renderer's actual containment.
"""

import os


def _env_int(name, default):
    try:
        return int(os.environ.get(name, "").strip() or default)
    except ValueError:
        return default


def renderer_limits():
    """Effective limit configuration (env overrides the defaults)."""
    return {
        "cpu_seconds": _env_int("GG_RENDERER_CPU_S", 600),
        "memory_mb": _env_int("GG_RENDERER_MEMORY_MB", 4096),
        "open_files": _env_int("GG_RENDERER_NOFILE", 512),
    }


def _apply_posix(limits, report):
    import resource

    def set_limit(kind, name, soft, hard):
        cur_soft, cur_hard = resource.getrlimit(kind)

        def cap(value, ceiling):
            if value == resource.RLIM_INFINITY:
                return ceiling
            if ceiling == resource.RLIM_INFINITY:
                return value
            return min(value, ceiling)

        soft = cap(soft, cur_hard)
        hard = cap(hard, cur_hard)
        try:
            resource.setrlimit(kind, (soft, hard))
            report.append(f"{name}={soft}")
        except (ValueError, OSError) as exc:
            report.append(f"{name}:skipped({exc})")

    cpu = limits["cpu_seconds"]
    if cpu > 0:
        # soft limit raises SIGXCPU; the hard limit 30s later is the
        # SIGKILL backstop if the signal is somehow swallowed
        set_limit(resource.RLIMIT_CPU, "cpu_s", cpu, cpu + 30)
    mem = limits["memory_mb"]
    if mem > 0:
        set_limit(resource.RLIMIT_AS, "as_mb",
                  mem * 1024 * 1024, mem * 1024 * 1024)
    nofile = limits["open_files"]
    if nofile > 0:
        set_limit(resource.RLIMIT_NOFILE, "nofile", nofile, nofile)
    set_limit(resource.RLIMIT_CORE, "core", 0, 0)

    os.umask(0o077)
    report.append("umask=077")

    try:
        import ctypes
        libc = ctypes.CDLL(None, use_errno=True)
        PR_SET_NO_NEW_PRIVS = 38
        if libc.prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) == 0:
            report.append("no_new_privs")
        else:
            report.append("no_new_privs:skipped")
    except Exception:
        report.append("no_new_privs:unavailable")


def _apply_windows(limits, report):
    import ctypes
    from ctypes import wintypes

    class IO_COUNTERS(ctypes.Structure):
        _fields_ = [(name, ctypes.c_ulonglong) for name in (
            "ReadOperationCount", "WriteOperationCount",
            "OtherOperationCount", "ReadTransferCount",
            "WriteTransferCount", "OtherTransferCount")]

    class JOBOBJECT_BASIC_LIMIT_INFORMATION(ctypes.Structure):
        _fields_ = [
            ("PerProcessUserTimeLimit", ctypes.c_longlong),
            ("PerJobUserTimeLimit", ctypes.c_longlong),
            ("LimitFlags", wintypes.DWORD),
            ("MinimumWorkingSetSize", ctypes.c_size_t),
            ("MaximumWorkingSetSize", ctypes.c_size_t),
            ("ActiveProcessLimit", wintypes.DWORD),
            ("Affinity", ctypes.c_size_t),
            ("PriorityClass", wintypes.DWORD),
            ("SchedulingClass", wintypes.DWORD),
        ]

    class JOBOBJECT_EXTENDED_LIMIT_INFORMATION(ctypes.Structure):
        _fields_ = [
            ("BasicLimitInformation", JOBOBJECT_BASIC_LIMIT_INFORMATION),
            ("IoInfo", IO_COUNTERS),
            ("ProcessMemoryLimit", ctypes.c_size_t),
            ("JobMemoryLimit", ctypes.c_size_t),
            ("PeakProcessMemoryUsed", ctypes.c_size_t),
            ("PeakJobMemoryUsed", ctypes.c_size_t),
        ]

    JOB_OBJECT_LIMIT_ACTIVE_PROCESS = 0x00000008
    JOB_OBJECT_LIMIT_PROCESS_TIME = 0x00000002
    JOB_OBJECT_LIMIT_PROCESS_MEMORY = 0x00000100
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x00002000
    JobObjectExtendedLimitInformation = 9

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    job = kernel32.CreateJobObjectW(None, None)
    if not job:
        report.append("job_object:skipped(create)")
        return
    info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION()
    basic = info.BasicLimitInformation
    basic.LimitFlags = (JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
    basic.ActiveProcessLimit = 1
    if limits["cpu_seconds"] > 0:
        basic.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_TIME
        # 100ns units
        basic.PerProcessUserTimeLimit = \
            limits["cpu_seconds"] * 10_000_000
    if limits["memory_mb"] > 0:
        basic.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY
        info.ProcessMemoryLimit = limits["memory_mb"] * 1024 * 1024
    ok = kernel32.SetInformationJobObject(
        job, JobObjectExtendedLimitInformation,
        ctypes.byref(info), ctypes.sizeof(info))
    if not ok:
        report.append("job_object:skipped(set)")
        return
    if not kernel32.AssignProcessToJobObject(
            job, kernel32.GetCurrentProcess()):
        report.append("job_object:skipped(assign)")
        return
    applied = ["active_procs=1", "kill_on_close"]
    if limits["cpu_seconds"] > 0:
        applied.append(f"cpu_s={limits['cpu_seconds']}")
    if limits["memory_mb"] > 0:
        applied.append(f"mem_mb={limits['memory_mb']}")
    report.append("job_object:" + ",".join(applied))


def apply_renderer_sandbox():
    """Apply the platform sandbox to the *current* process. Returns a
    report list; never raises (a partially applied sandbox is better
    than no renderer)."""
    if os.environ.get("GG_RENDERER_SANDBOX", "1").strip() == "0":
        return ["disabled"]
    limits = renderer_limits()
    report = []
    try:
        if os.name == "posix":
            _apply_posix(limits, report)
        elif os.name == "nt":
            _apply_windows(limits, report)
        else:
            report.append(f"unsupported_platform:{os.name}")
    except Exception as exc:
        report.append(f"sandbox_error:{type(exc).__name__}:{exc}")
    return report
