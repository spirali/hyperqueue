import time
from typing import List, Union

DEFAULT_TIMEOUT = 10


class TimeoutException(BaseException):
    pass


def wait_until(fn, sleep_s=0.2, timeout_s=DEFAULT_TIMEOUT):
    end = time.time() + timeout_s

    while time.time() < end:
        value = fn()
        if value is not None and value is not False:
            return value
        time.sleep(sleep_s)
    raise TimeoutException(f"Wait timeouted after {timeout_s} seconds")


def wait_for_state(
    env,
    ids: Union[int, List[int]],
    target_states: Union[str, List[str]],
    commands: List[str],
    state_index: int,
    **kwargs,
):
    if isinstance(ids, int):
        ids = {str(ids)}
    else:
        ids = set(str(id) for id in ids)

    if isinstance(target_states, str):
        target_states = {target_states.lower()}
    else:
        target_states = set(state.lower() for state in target_states)

    last_table = None

    def check():
        nonlocal last_table

        table = env.command(commands, as_table=True)
        last_table = table
        items = [row for row in table if row[0] in ids]
        return len(items) >= len(ids) and all(
            j[state_index].lower() in target_states for j in items
        )

    try:
        wait_until(check, **kwargs)
    except TimeoutException as e:
        if last_table is not None:
            raise Exception(f"{e}, most recent table:\n{last_table}")
        raise e


def wait_for_job_state(
    env, ids: Union[int, List[int]], target_states: Union[str, List[str]], **kwargs
):
    wait_for_state(env, ids, target_states, ["job", "list", "--all"], 2, **kwargs)


def wait_for_worker_state(
    env, ids: Union[int, List[int]], target_states: Union[str, List[str]], **kwargs
):
    wait_for_state(env, ids, target_states, ["worker", "list", "--all"], 1, **kwargs)
