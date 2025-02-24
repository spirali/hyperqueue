from .conftest import HqEnv
from .utils import wait_for_job_state
from .utils.job import default_task_output
import os


def test_task_data_invalid_call(hq_env: HqEnv):
    hq_env.command(["task-data"])


def test_data_create_no_consumer(hq_env: HqEnv, tmp_path):
    tmp_path.joinpath("job.toml").write_text(
        """
[[task]]
id = 0
command = ["bash", "-c", "set -e; echo 'abc' > test.txt; $HQ data put 1 test.txt"]
keep_outputs = true
"""
    )
    hq_env.start_server()
    hq_env.start_worker()
    hq_env.command(["job", "submit-file", "job.toml"])
    wait_for_job_state(hq_env, 1, "FINISHED")


def test_data_transfer_invalid_input_id(hq_env: HqEnv, tmp_path):
    tmp_path.joinpath("job.toml").write_text("""
data_layer = true
        
[[task]]
id = 12
command = ["bash", "-c", "set -e; $HQ data get 3 test.txt"]
""")
    hq_env.start_server()
    hq_env.start_worker()
    hq_env.command(["job", "submit-file", "job.toml"])
    wait_for_job_state(hq_env, 1, "FAILED")
    with open(os.path.join(tmp_path, default_task_output(job_id=1, task_id=12, type="stderr"))) as f:
        assert "Input 3 not found" in f.read()


def test_data_transfer_invalid_upload(hq_env: HqEnv, tmp_path):
    tmp_path.joinpath("job.toml").write_text("""
data_layer = true
        
[[task]]
id = 12
command = ["bash", "-c", "set -e; touch test.txt; $HQ data put 3 test.txt; $HQ data put 3 test.txt"]
""")
    hq_env.start_server()
    hq_env.start_worker()
    hq_env.command(["job", "submit-file", "job.toml"])
    wait_for_job_state(hq_env, 1, "FAILED")
    with open(os.path.join(tmp_path, default_task_output(job_id=1, task_id=12, type="stderr"))) as f:
        assert "/3 already exists" in f.read()


def test_data_transfer_same_worker(hq_env: HqEnv, tmp_path):
    tmp_path.joinpath("job.toml").write_text(
        """
data_layer = true
        
[[task]]
id = 12
command = ["bash", "-c", "set -e; echo 'abc' > test.txt; sleep 1; $HQ data put 3 test.txt"]

[[task]]
id = 13
command = ["bash", "-c", "set -e; $HQ data get 0 out.txt; cat out.txt"]

[[task.data_deps]]
task_id = 12
data_id = 3 
"""
    )
    hq_env.start_server()
    hq_env.start_worker(cpus=4)
    hq_env.command(["job", "submit-file", "job.toml"])
    wait_for_job_state(hq_env, 1, "FINISHED")
    with open(os.path.join(tmp_path, default_task_output(job_id=1, task_id=13, type="stdout"))) as f:
        assert f.read() == "abc\n"


def test_data_transfer_different_worker(hq_env: HqEnv, tmp_path):
    tmp_path.joinpath("job.toml").write_text(
        """
data_layer = true
        
[[task]]
id = 1
command = ["bash", "-c", "set -e; echo 'abc' > test.txt; sleep 1; $HQ data put 22 test.txt"]
[[task.request]]
resources = { "a" = 1 }


[[task]]
id = 2
command = ["bash", "-c", "set -e; $HQ data get 0 out.txt; cat out.txt"]
[[task.request]]
resources = { "b" = 1 }
[[task.data_deps]]
task_id = 1
data_id = 3
"""
    )
    hq_env.start_server()
    hq_env.start_worker(args=["--resource", "a=sum(1)"])
    hq_env.start_worker(args=["--resource", "b=sum(1)"])
    hq_env.command(["job", "submit-file", "job.toml"])
    wait_for_job_state(hq_env, 1, "FINISHED")
    with open(os.path.join(tmp_path, default_task_output(job_id=1, task_id=13, type="stdout"))) as f:
        assert f.read() == "abc\n"


def test_data_transfer_big_data(hq_env: HqEnv, tmp_path):
    raise Exception("TODO")
