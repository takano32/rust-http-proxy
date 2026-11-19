"""`scripts/check-spec-names.py` の試験 (T38.5)。

名前の当て方 (環境変数の前後・エンドポイントの前後と `/snapshots/<date>`) を作り物の文で見て、
仕様に無い名前と、使っていない除外の行が NG になることを、一時のリポジトリで `main()` を回して見る。
"""

import contextlib
import io
import os
import tempfile
import unittest

import _testing

csn = _testing.load("check_spec_names", "check-spec-names.py")


class Matching(unittest.TestCase):
    def test_env_needs_whole_name(self):
        self.assertTrue(csn.env_found("PROXY_DNS_TTL_SECS", "| `PROXY_DNS_TTL_SECS` | `60` |"))
        self.assertFalse(csn.env_found("PROXY_DNS_TTL_SECS", "`PROXY_DNS_TTL_SECS_X` だけ"))
        self.assertFalse(csn.env_found("PROXY_DNS_TTL", "`PROXY_DNS_TTL_SECS`"))
        self.assertFalse(csn.env_found("PROXY_CANARY", "`NO_PROXY_CANARY`"))

    def test_endpoint_boundaries(self):
        self.assertTrue(csn.endpoint_found("/hosts", "`/hosts?limit=1000`"))
        self.assertTrue(csn.endpoint_found("/hosts", "`/hosts` と `/dns`"))
        self.assertFalse(csn.endpoint_found("/hosts", "`/hosts/series?top=16` だけ"))
        self.assertTrue(csn.endpoint_found("/hosts/series", "`/hosts/series?top=16`"))
        self.assertFalse(csn.endpoint_found("/log", "`/login` と `/catalog`"))
        self.assertTrue(csn.endpoint_found("/probe.html", "`/probe.html` (「端末から測る」)"))
        self.assertFalse(csn.endpoint_found("/probe.html", "`/probe.htmlx`"))

    def test_snapshots_date(self):
        self.assertTrue(csn.endpoint_found("/snapshots/<date>", "`/snapshots/<YYYY-MM-DD>` は"))
        self.assertFalse(csn.endpoint_found("/snapshots/<date>", "`/snapshots` だけ"))

    def test_check_reports_missing_and_unused(self):
        text = "`PROXY_A` と `/status`"
        total, missing, unused = csn.check(["PROXY_A", "PROXY_B"], ["/status", "/gone"], text,
                                           skip=[("/gone", "理由"), ("PROXY_OLD", "古い")])
        self.assertEqual(total, 4)
        self.assertEqual(missing, [("環境変数", "PROXY_B")])
        self.assertEqual(unused, ["PROXY_OLD"])


class Main(unittest.TestCase):
    def run_in(self, spec, specs_sn, envs, eps):
        with tempfile.TemporaryDirectory() as root:
            with open(os.path.join(root, "SPECS.md"), "w", encoding="utf-8") as f:
                f.write(spec)
            os.mkdir(os.path.join(root, "specs"))
            with open(os.path.join(root, "specs", "S7.md"), "w", encoding="utf-8") as f:
                f.write(specs_sn)
            env_file = os.path.join(root, "env")
            ep_file = os.path.join(root, "ep")
            with open(env_file, "w", encoding="utf-8") as f:
                f.write("\n".join(envs) + "\n")
            with open(ep_file, "w", encoding="utf-8") as f:
                f.write("\n".join(eps) + "\n")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                code = csn.main(["check-spec-names.py", env_file, ep_file, root])
            return code, out.getvalue()

    def test_ok_when_every_name_is_in_some_spec(self):
        code, out = self.run_in("## S1\n`/status` を返す\n", "| `PROXY_X` | `1` |\n", ["PROXY_X"], ["/status"])
        self.assertEqual(code, 0, out)
        self.assertNotIn("NG", out)

    def test_ng_names_the_missing_one(self):
        code, out = self.run_in("## S1\n`/status`\n", "", ["PROXY_X"], ["/status", "/new"])
        self.assertEqual(code, 1)
        self.assertIn("NG [仕様に載っているか]: 環境変数 PROXY_X", out)
        self.assertIn("NG [仕様に載っているか]: エンドポイント /new", out)


if __name__ == "__main__":
    unittest.main()
