from .conftest import HqEnv
from .utils import wait_for_job_state
from .utils.job import default_task_output



def test_task_data_invalid_call(hq_env: HqEnv):
    hq_env.command(["task-data"])


def test_data_create_no_consumer(hq_env: HqEnv, tmp_path):
    tmp_path.joinpath("job.toml").write_text(
        """
[[task]]
id = 0
command = ["bash", "-c", "echo $HQ_DATA_ACCESS"]
keep_outputs = true
"""
    )
    hq_env.start_server()
    hq_env.start_worker()
    hq_env.command(["job", "submit-file", "job.toml"])
    wait_for_job_state(hq_env, 1, "FINISHED")
    with open(default_task_output(1)) as f:
        key = f.read()
        path, token = key.rsplit(":", 1)
        assert len(path) > 8
        assert token.startswith("hq0-")
