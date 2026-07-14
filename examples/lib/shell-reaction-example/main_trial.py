import sys
from datetime import datetime
import psutil

PROCESS_NAME = "shell-reaction-example"


def find_pid_by_name(name: str) -> int | None:
    """Iterates through running processes to find a matching binary name."""
    for proc in psutil.process_iter(["pid", "name", "exe"]):
        try:
            if proc.info["name"] == name or (
                proc.info["exe"] and name in proc.info["exe"]
            ):
                return proc.info["pid"]
        except (psutil.NoSuchProcess, psutil.AccessDenied, psutil.ZombieProcess):
            continue
    return None


def get_process_stats(pid: int) -> tuple[float, int]:
    """Returns a tuple of (Memory in MB, Thread Count) for the given PID."""
    try:
        process = psutil.Process(pid)
        memory_bytes = process.memory_info().rss
        memory_mb = memory_bytes / (1024 * 1024)

        # Get the number of threads currently running in this process
        thread_count = process.num_threads()

        return memory_mb, thread_count
    except (psutil.NoSuchProcess, psutil.AccessDenied):
        return 0.0, 0


if __name__ == "__main__":
    timestamp = datetime.now().isoformat()
    pid = find_pid_by_name(PROCESS_NAME)

    with open("output.txt", "a") as f:
        if pid:
            memory_usage_mb, thread_count = get_process_stats(pid)
            log_line = f"Timestamp: {timestamp} | Process: {PROCESS_NAME} | PID: {pid} | Memory: {memory_usage_mb:.2f} MB | Threads: {thread_count}\n"
        else:
            log_line = f"Timestamp: {timestamp} | Process: {PROCESS_NAME} | Status: NOT RUNNING\n"

        f.write(log_line)