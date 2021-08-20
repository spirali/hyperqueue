from typing import Optional, List

JOB_TABLE_ROWS = 15


# TODO: create a pandas dataframe instead?
class Table:
    def __init__(self, rows):
        self.rows = rows

    def __getitem__(self, item):
        if isinstance(item, slice):
            return Table(self.rows[item])
        return self.rows[item]

    def __iter__(self):
        yield from self.rows

    def get_row_value(self, key: str) -> Optional[str]:
        """
        Assumes vertical table (each value has a separate row).
        """
        row = [r for r in self.rows if r and r[0] == key]
        if not row:
            return None
        assert len(row) == 1
        return row[0][1]

    def check_value_row(self, key: str, value: str):
        row = self.get_row_value(key)
        if row is None:
            raise Exception(f"Key `{key!r}` not found in\n{self}")
        assert_equals(row, value)

    def get_column_value(self, key: str) -> Optional[List[str]]:
        """
        Assumes horizontal table (each value has a separate column).
        """
        header = self.rows[0]
        if key not in header:
            return None

        index = header.index(key)
        return [row[index] for row in self.rows[1:]]

    def check_value_column(self, key: str, index: int, value: str):
        column = self.get_column_value(key)
        if not column:
            raise Exception(f"Value for key `{key!r}` not found in\n{self}")
        row = column[index]
        assert_equals(row, value)

    def check_value_columns(self, keys: List[str], index: int, values: List[str]):
        assert len(keys) == len(values)
        for (key, val) in zip(keys, values):
            self.check_value_column(key, index, val)

    def print(self):
        for i, row in enumerate(self):
            print(i, row)

    def __len__(self):
        return len(self.rows)

    def __repr__(self):
        return "\n".join(" | ".join(val) for val in self.rows)


def assert_equals(a, b):
    """
    Workaround for pytest.
    Without this it won't show the differing values if an assert error happens inside some method.
    """
    if a != b:
        raise Exception(f"{a!r} does not equal {b!r}")


def parse_table(table_string: str) -> Table:
    lines = table_string.strip().split("\n")
    result = []
    new_row = True
    for line in lines:
        if line.startswith("+-"):
            new_row = True
            continue
        items = [x.strip() for x in line.split("|")[1:-1]]
        if new_row:
            result.append(items)
            new_row = False
        else:
            for i, item in enumerate(items):
                if item:
                    result[-1][i] += "\n" + item
    return Table(result)
