import json
import time
from os.path import join

from .conftest import HqEnv
from .utils.check import check_error_log


def test_autoalloc_info(hq_env: HqEnv):
    hq_env.start_server(args=["--autoalloc-interval", "1m 5s"])
    table = hq_env.command(["auto-alloc", "info"], as_table=True)
    table.check_value_row("Refresh interval", "1m 5s")


def test_autoalloc_descriptor_info(hq_env: HqEnv):
    hq_env.start_server()
    hq_env.command(["auto-alloc", "add", "foo", "pbs", "queue", "5"])

    table = hq_env.command(["auto-alloc", "info"], as_table=True)[1:]
    table.check_value_columns(("Name", "Target worker count", "Max workers per allocation", "Queue", "Timelimit"), 0,
                              ("foo", "5", "1", "queue", "N/A"))

    hq_env.command(
        ["auto-alloc", "add", "bar", "pbs", "qexp", "1", "--max-workers-per-alloc", "2", "--timelimit", "1h"])
    table = hq_env.command(["auto-alloc", "info"], as_table=True)[1:]
    table.check_value_columns(("Name", "Target worker count", "Max workers per allocation", "Queue", "Timelimit"), 0,
                              ("bar", "1", "2", "qexp", "1h"))


def test_autoalloc_descriptor_name_collision(hq_env: HqEnv):
    hq_env.start_server()
    hq_env.command(["auto-alloc", "add", "foo", "pbs", "queue", "5"])
    hq_env.command(["auto-alloc", "add", "foo", "pbs", "queue", "5"], expect_fail="Descriptor foo already exists")


def test_add_pbs_descriptor(hq_env: HqEnv):
    hq_env.start_server(args=["--autoalloc-interval", "500ms"])
    output = hq_env.command(["auto-alloc", "add", "foo", "pbs", "queue", "5", "--max-workers-per-alloc", "2"])
    assert "Allocation queue foo was successfully created" in output

    info = hq_env.command(["auto-alloc", "info"], as_table=True)[1:]
    info.check_value_column("Name", 0, "foo")


def test_pbs_queue_qsub_fail(hq_env: HqEnv):
    qsub_code = "exit(1)"

    with hq_env.mock.mock_program("qsub", qsub_code):
        hq_env.start_server(args=["--autoalloc-interval", "100ms"])
        hq_env.command(["auto-alloc", "add", "foo", "pbs", "queue", "1"])
        time.sleep(0.2)
        table = hq_env.command(["auto-alloc", "events", "foo"], as_table=True)
        table.check_value_column("Event", 0, "Allocation submission failed")
        table.check_value_column("Message", 0, "qsub execution failed")


def test_pbs_queue_qsub_success(hq_env: HqEnv):
    qsub_code = """print("123.job")"""

    with hq_env.mock.mock_program("qsub", qsub_code):
        hq_env.start_server(args=["--autoalloc-interval", "100ms"])
        hq_env.command(["auto-alloc", "add", "foo", "pbs", "queue", "1"])
        time.sleep(0.2)
        table = hq_env.command(["auto-alloc", "events", "foo"], as_table=True)
        table.check_value_column("Event", 0, "Allocation queued")
        table.check_value_column("Message", 0, "123.job")


def test_pbs_queue_qsub_check_args(hq_env: HqEnv):
    output_log = join(hq_env.work_path, "output.log")
    qsub_code = f"""
import sys
import traceback

args = sys.argv[1:]    

def check_arg(key, val):
    for (index, arg) in enumerate(args):
        if arg == key:
            assert args[index + 1] == val
            return
    raise Exception(f"Key `{{key}}` not found")


def check():
    check_arg("-q", "queue")

    assert "-lselect=1" in args
    for arg in args:
        assert not arg.startswith("-lwalltime")

try:
    check()
except:
    with open("{output_log}", "w") as f:
        tb = traceback.format_exc()
        f.write(tb)
        f.write(" ".join(args))
"""

    with hq_env.mock.mock_program("qsub", qsub_code):
        with check_error_log(output_log):
            hq_env.start_server(args=["--autoalloc-interval", "100ms"])
            hq_env.command(["auto-alloc", "add", "foo", "pbs", "queue", "1"])
            time.sleep(0.2)


def test_pbs_qstat_status(hq_env: HqEnv):
    status_path = join(hq_env.work_path, "status")

    qsub_code = """print("jobid")"""

    qstat_code = f"""
import sys
import json

assert "jobid" in sys.argv

with open("{status_path}") as f:
    jobdata = json.loads(f.read())

data = {{
    "Jobs": {{
        "jobid": jobdata
    }}
}}
print(json.dumps(data))
"""

    def write_status(status: str, **args):
        data = {
            "job_state": status,
            **args
        }
        with open(status_path, "w") as f:
            f.write(json.dumps(data))

    with hq_env.mock.mock_program("qsub", qsub_code):
        with hq_env.mock.mock_program("qstat", qstat_code):
            write_status("Q", qtime="Thu Aug 19 13:05:38 2021")
            hq_env.start_server(args=["--autoalloc-interval", "100ms"])
            hq_env.command(["auto-alloc", "add", "foo", "pbs", "queue", "1"])

            # Queued
            time.sleep(0.5)
            table = hq_env.command(["auto-alloc", "events", "foo"], as_table=True)
            table.check_value_column("Event", -1, "Allocation queued")

            # Started
            write_status(status="R", stime="Thu Aug 19 13:05:39 2021")
            time.sleep(0.5)
            table = hq_env.command(["auto-alloc", "events", "foo"], as_table=True)
            table.check_value_column("Event", -1, "Allocation started")

            # Finished
            write_status(status="F", Exit_status=0)
            time.sleep(0.5)
            table = hq_env.command(["auto-alloc", "events", "foo"], as_table=True)
            assert "Allocation finished" in table.get_column_value("Event")


def test_pbs_qstat_allocation_failed(hq_env: HqEnv):
    qsub_code = """print("jobid")"""

    qstat_code = """
import sys
import json

assert "jobid" in sys.argv

data = {
    "Jobs": {
        "jobid": {
            "job_state": "F",
            "Exit_status": 1
        }
    }
}
print(json.dumps(data))
"""

    with hq_env.mock.mock_program("qsub", qsub_code):
        with hq_env.mock.mock_program("qstat", qstat_code):
            hq_env.start_server(args=["--autoalloc-interval", "100ms"])
            hq_env.command(["auto-alloc", "add", "foo", "pbs", "queue", "1"])

            time.sleep(0.5)
            table = hq_env.command(["auto-alloc", "events", "foo"], as_table=True)
            column = table.get_column_value("Event")
            assert "Allocation failed" in column
