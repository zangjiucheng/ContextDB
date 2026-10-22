"""Protect human ground truth while reviewing AI proposals; all writes use copies."""

import contextlib
import importlib.util
import io
from pathlib import Path
import shutil
import tempfile
import unittest
from unittest import mock


SPEC = importlib.util.spec_from_file_location("p5a_review", Path(__file__).with_name("p5a_review.py"))
review = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(review)


class HumanReviewTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.data = self.base / "ai-review"
        self.data.mkdir()
        for name in ["manifest.json", "evidence.json.gz", "proposals.tsv", "excluded.tsv"]:
            shutil.copy2(review.DEFAULT_DATA / name, self.data / name)
        for name in review.SOURCES:
            shutil.copy2(review.DEFAULT_DATA.parent / name, self.base / name)
            # Real review can already be in progress. Only reset the temporary
            # test fixture, never the user's canonical labels.
            path = self.base / name
            _, lines, rows = review.source_rows(path)
            for index, fields in rows:
                fields[8:] = ["", "", ""]
                lines[index] = "\t".join(fields) + "\n"
            path.write_text("".join(lines))
        self.proposals, self.evidence = review.load(self.data)
        self.pair = self.proposals[0]
        self.ev = self.evidence["pairs"][self.pair["id"]]

    def source_bytes(self):
        return {name: (self.base / name).read_bytes() for name in review.SOURCES}

    def test_loading_and_skipping_create_no_human_labels(self):
        before = self.source_bytes()
        with mock.patch("builtins.input", side_effect=["", "s", "q"]), contextlib.redirect_stdout(io.StringIO()):
            saved = review.review(self.data, self.proposals, self.evidence, "test-human", 0)
        self.assertEqual(saved, 0)
        self.assertEqual(self.source_bytes(), before)

    def test_explicit_answer_changes_one_row_preserves_identity_and_provenance(self):
        before = self.source_bytes()
        answer = "y" if self.pair["ai_invalidates"] == "n" else "n"
        review.record(self.data, self.pair, self.ev, "test-human", answer, "different\tcontract\nreviewed")
        after = self.source_bytes()
        changed = [name for name in before if before[name] != after[name]]
        self.assertEqual(changed, [self.pair["source"]])
        old = before[changed[0]].decode().splitlines()
        new = after[changed[0]].decode().splitlines()
        differences = [(a, b) for a, b in zip(old, new) if a != b]
        self.assertEqual(len(old), len(new))
        self.assertEqual(len(differences), 1)
        a, b = (line.split("\t") for line in differences[0])
        self.assertEqual(a[:8], b[:8])
        self.assertEqual(b[8:10], [answer, "test-human"])
        self.assertIn("overridden", b[10])
        self.assertIn("AI rationale:", b[10])
        self.assertIn("different contract reviewed", b[10])
        self.assertEqual(len(b), 11)
        review.load(self.data)  # human answers do not invalidate immutable evidence hashes

    def test_accept_and_resume_do_not_overwrite_existing_human_answer(self):
        with mock.patch("builtins.input", side_effect=["a", "q"]), contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(review.review(self.data, self.proposals, self.evidence, "test-human", 0), 1)
        fields = review.locate(self.data, self.pair, self.ev)[4]
        self.assertEqual(fields[8], self.pair["ai_invalidates"])
        self.assertIn("accepted", fields[10])
        before = self.source_bytes()
        with mock.patch("builtins.input", side_effect=["q"]), contextlib.redirect_stdout(io.StringIO()) as output:
            review.review(self.data, self.proposals, self.evidence, "other-human", 0)
        self.assertNotIn(self.pair["id"], output.getvalue())
        self.assertEqual(self.source_bytes(), before)
        with self.assertRaisesRegex(ValueError, "not overwriting"):
            review.record(self.data, self.pair, self.ev, "other-human", "y")

    def test_identity_drift_and_evidence_tampering_are_rejected(self):
        path = self.base / self.pair["source"]
        original = path.read_text()
        path.write_text(original.replace(self.pair["commit"], "0" * 40, 1))
        with self.assertRaisesRegex(ValueError, "identity changed"):
            review.load(self.data)
        path.write_text(original)
        with (self.data / "proposals.tsv").open("a") as stream:
            stream.write("tampered\n")
        with self.assertRaisesRegex(ValueError, "hash mismatch"):
            review.load(self.data)

    def test_concurrent_edit_is_preserved_and_temp_cleaned(self):
        path = self.base / self.pair["source"]
        expected = path.read_bytes()
        updated = expected + b"# concurrent note\n"
        path.write_bytes(updated)
        with self.assertRaisesRegex(ValueError, "source changed"):
            review.atomic_write(path, expected, b"would overwrite")
        self.assertEqual(path.read_bytes(), updated)
        self.assertEqual(list(self.base.glob(".g6-review-*")), [])

    def test_invalid_reviewer_and_non_answers_cannot_write(self):
        before = self.source_bytes()
        for reviewer, answer in [("", "y"), ("two\nlines", "y"), ("human", "a")]:
            with self.assertRaises(ValueError):
                review.record(self.data, self.pair, self.ev, reviewer, answer)
        self.assertEqual(self.source_bytes(), before)

    def test_terminal_control_bytes_are_escaped(self):
        self.assertEqual(review.display("\x1b[2J\x00\x9b\ntext\t"), "\\u001b[2J\\u0000\\u009b\ntext\t")


if __name__ == "__main__":
    unittest.main()
