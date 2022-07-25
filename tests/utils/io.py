def check_file_contents(path: str, content):
    assert read_file(path) == str(content)


def read_file(path: str) -> str:
    with open(path) as f:
        return f.read()
