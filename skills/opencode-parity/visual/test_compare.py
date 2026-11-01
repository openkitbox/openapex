"""Self-test for compare.py: identical grids diff to 0, a changed cell is located."""
import copy
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
COMPARE = os.path.join(HERE, "compare.py")


def blank_grid(rows=24, cols=80):
    cell = [" ", "default", "default", ""]
    return {"rows": rows, "cols": cols, "cells": [[list(cell) for _ in range(cols)] for _ in range(rows)]}


def dump(grid):
    with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
        json.dump(grid, f)
    return f.name


def run(a, b):
    return subprocess.run(
        [sys.executable, COMPARE, a, b, "--max", "5"], capture_output=True, text=True
    )


class CompareTest(unittest.TestCase):
    def test_identical_grids_have_no_diff(self):
        gold = dump(blank_grid())
        try:
            r = run(gold, gold)
        finally:
            os.unlink(gold)
        self.assertEqual(r.returncode, 0, r.stdout)
        self.assertIn("0/1920 cells differ", r.stdout)

    def test_changed_cell_is_reported_with_position(self):
        grid = blank_grid()
        mutated = copy.deepcopy(grid)
        mutated["cells"][9][4][0] = "X"
        gold, actual = dump(grid), dump(mutated)
        try:
            r = run(gold, actual)
        finally:
            os.unlink(gold)
            os.unlink(actual)
        self.assertEqual(r.returncode, 1)
        self.assertIn("(10,  5) char", r.stdout)
        self.assertIn("1/1920 cells differ", r.stdout)


if __name__ == "__main__":
    unittest.main()
