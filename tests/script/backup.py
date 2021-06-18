from os.path import join, isfile, isdir

from ..conftest import HqEnv, HQ_BINARY
from ..utils import wait_for_job_state, read_file


def test_backup_success_delete_output(hq_env: HqEnv):
    hq_env.start_server()
    hq_env.start_worker()

    stdout = join(hq_env.work_path, "stdout")
    stderr = join(hq_env.work_path, "stderr")
    backup = join(hq_env.work_path, "backup")

    hq_env.command([
        "submit", "--stdout", stdout, "--stderr", stderr,
        "--",
        HQ_BINARY, "script", "backup-output", backup,
        "--",
        "bash", "-c", "echo stdout; echo stderr >&2"
    ])
    wait_for_job_state(hq_env, 1, "FINISHED")

    assert not isfile(stdout)
    assert not isfile(stderr)
    assert not isdir(backup)


def test_backup_failure_copy_output(hq_env: HqEnv):
    hq_env.start_server()
    hq_env.start_worker()

    stdout = join(hq_env.work_path, "stdout")
    stderr = join(hq_env.work_path, "stderr")
    backup = join(hq_env.work_path, "backup")

    hq_env.command([
        "submit", "--stdout", stdout, "--stderr", stderr,
        "--",
        HQ_BINARY, "script", "backup-output", backup,
        "--",
        "bash", "-c", "echo stdout; echo stderr >&2; exit 1"
    ])
    wait_for_job_state(hq_env, 1, "FAILED")

    assert read_file(join(backup, "stdout")) == "stdout\n"
    assert read_file(join(backup, "stderr")) == "stderr\n"
