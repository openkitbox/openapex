import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
import score


class ScoreAggregationTest(unittest.TestCase):
    def test_aggregates_explicit_verifier_outcomes_and_wall_time(self):
        trials = [
            {"status": "completed", "verifier": {"status": "pass"}, "wall_time_seconds": 1.0},
            {"status": "completed", "verifier": {"status": "fail"}, "wall_time_seconds": 3.0},
            {"status": "completed", "verifier": {"status": "timeout"}, "wall_time_seconds": 5.0},
            {"status": "completed", "verifier": {"status": "infra"}, "wall_time_seconds": 7.0},
            {"status": "completed", "verifier": {"status": "incomplete"}, "wall_time_seconds": 9.0},
        ]

        summary = score.aggregate_trials(trials)

        self.assertEqual(summary["counts"], {
            "pass": 1,
            "fail": 1,
            "timeout": 1,
            "infra": 1,
            "incomplete": 1,
        })
        self.assertEqual(summary["total_trials"], 5)
        self.assertEqual(summary["valid_trials"], 2)
        self.assertEqual(summary["valid_coverage"], 0.4)
        self.assertEqual(summary["pass_rate"], 0.5)
        self.assertEqual(summary["wall_time_seconds"], {
            "count": 5,
            "min": 1.0,
            "max": 9.0,
            "mean": 5.0,
            "median": 5.0,
        })

    def test_runner_status_cannot_be_overridden_by_verifier(self):
        trials = [
            {"status": "timeout", "verifier": {"status": "pass"}},
            {"status": "process_error", "verifier": {"status": "fail"}},
            {"status": "protocol_error", "verifier": {"status": "pass"}},
            {"status": "completed", "verifier": {"status": "fail"}},
        ]

        summary = score.aggregate_trials(trials)

        self.assertEqual(summary["counts"], {
            "pass": 0,
            "fail": 1,
            "timeout": 1,
            "infra": 2,
            "incomplete": 0,
        })

    def test_rejects_non_finite_and_negative_wall_times(self):
        trials = [
            {"status": "completed", "verifier": {"status": "pass"}, "wall_time_seconds": -1},
            {"status": "completed", "verifier": {"status": "pass"}, "wall_time_seconds": float("nan")},
            {"status": "completed", "verifier": {"status": "pass"}, "wall_time_seconds": float("inf")},
            {"status": "completed", "verifier": {"status": "pass"}, "wall_time_seconds": 1},
        ]

        summary = score.aggregate_trials(trials)

        self.assertEqual(summary["wall_time_seconds"]["count"], 1)
        self.assertEqual(summary["wall_time_seconds"]["mean"], 1.0)

    def test_deduplicates_directory_and_file_inputs_and_rejects_malformed_json(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            trial_path = root / "trial.json"
            trial_path.write_text(json.dumps({"status": "completed"}), encoding="utf-8")
            self.assertEqual(len(score.load_trials([str(root), str(trial_path)])), 1)
            trial_path.write_text("not json", encoding="utf-8")
            with self.assertRaises(ValueError):
                score.load_trials([str(trial_path)])

    def test_unverified_completed_trial_is_incomplete_and_runner_failure_is_infra(self):
        trials = [
            {"status": "completed", "verifier": {"status": "unverified"}, "wall_time_seconds": 2.0},
            {"status": "infra", "verifier": {"status": "unverified"}, "wall_time_seconds": 4.0},
        ]

        summary = score.aggregate_trials(trials)

        self.assertEqual(summary["counts"]["incomplete"], 1)
        self.assertEqual(summary["counts"]["infra"], 1)
        self.assertIsNone(summary["pass_rate"])
        self.assertEqual(summary["valid_coverage"], 0.0)


if __name__ == "__main__":
    unittest.main()
