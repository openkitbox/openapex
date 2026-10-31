"""Self-test for compare.py: identical grids diff to 0, a changed cell is located."""
import copy
import json
import os
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
GOLD = os.path.join(HERE, "gold", "gold-home-24x80.json")
COMPARE = os.path.join(HERE, "compare.py")


def run(a, b):
    return subprocess.run(
        [sys.executable, COMPARE, a, b, "--max", "5"], capture_output=True, text=True
    )


class CompareTest(unittest.TestCase):
    def test_identical_grids_have_no_diff(self):
        r = run(GOLD, GOLD)
        self.assertEqual(r.returncode, 0, r.stdout)
        self.assertIn("0/1920 cells differ", r.stdout)

    def test_changed_cell_is_reported_with_position(self):
        grid = json.load(open(GOLD))
        mutated = copy.deepcopy(grid)
        mutated["cells"][9][4][0] = "X"
        with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
            json.dump(mutated, f)
        try:
            r = run(GOLD, f.name)
        finally:
            os.unlink(f.name)
        self.assertEqual(r.returncode, 1)
        self.assertIn("(10,  5) char", r.stdout)
        self.assertIn("1/1920 cells differ", r.stdout)


if __name__ == "__main__":
    unittest.main()
