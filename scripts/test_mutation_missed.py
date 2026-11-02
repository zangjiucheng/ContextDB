import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import mutation_missed as mm  # noqa: E402


class MutationMissedTest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.waivers = os.path.join(self.dir.name, "waivers.txt")
        with open(self.waivers, "w") as f:
            f.write("# comment\n\n")
            f.write(r"crates/a\.rs: replace -= with (\+=|/=) in KeyTable::remove  # P6 G3: len only drives resizing" "\n")

    def run_with(self, missed):
        out = os.path.join(self.dir.name, "out")
        os.makedirs(os.path.join(out, "mutants.out"), exist_ok=True)
        with open(os.path.join(out, "mutants.out", "missed.txt"), "w") as f:
            f.write("".join(m + "\n" for m in missed))
        return mm.main(["x", out, self.waivers])

    def test_waived_at_any_line_passes(self):
        self.assertEqual(self.run_with([
            "crates/a.rs:61:18: replace -= with += in KeyTable::remove",
            "crates/a.rs:90:3: replace -= with /= in KeyTable::remove",
        ]), 0)

    def test_unwaived_fails(self):
        self.assertEqual(self.run_with([
            "crates/a.rs:61:18: replace -= with *= in KeyTable::remove",
        ]), 1)
        self.assertEqual(self.run_with([
            "crates/b.rs:61:18: replace -= with += in KeyTable::remove",
        ]), 1)

    def test_no_missed_file_passes(self):
        out = os.path.join(self.dir.name, "empty")
        os.makedirs(out)
        self.assertEqual(mm.main(["x", out, self.waivers]), 0)

    def test_waiver_needs_reason(self):
        with open(self.waivers, "a") as f:
            f.write("crates/c\\.rs: anything\n")
        with self.assertRaises(ValueError):
            mm.load_waivers(self.waivers)


if __name__ == "__main__":
    unittest.main()
