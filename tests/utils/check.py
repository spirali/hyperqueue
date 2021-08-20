import contextlib
import os


@contextlib.contextmanager
def check_error_log(path: str):
    """Check that the file at the given path is either missing or empty.
    If not, raise an exception with its contents"""
    yield
    if not os.path.isfile(path):
        return

    with open(path) as f:
        data = f.read().strip()
        if data:
            raise Exception(f"Error log at {path}\n{data}")
