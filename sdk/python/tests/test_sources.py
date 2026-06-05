import unittest

from muzen import github, gitlab, local, parse_review_source, source_key
from muzen.sources import MuzenSourceError


class SourceTests(unittest.TestCase):
    def test_parse_github_source_shorthand(self) -> None:
        source = parse_review_source("github:maskdotdev/heimdaal#123")

        self.assertEqual(source.type, "github_pull_request")
        self.assertEqual(source.owner, "maskdotdev")
        self.assertEqual(source.repo, "heimdaal")
        self.assertEqual(source.number, 123)
        self.assertEqual(source_key(source), "github:maskdotdev/heimdaal#123")

    def test_parse_gitlab_source_shorthand_with_nested_owner(self) -> None:
        source = parse_review_source("gitlab:platform/reviews/heimdaal!42")

        self.assertEqual(source.type, "gitlab_merge_request")
        self.assertEqual(source.owner, "platform/reviews")
        self.assertEqual(source.repo, "heimdaal")
        self.assertEqual(source.number, 42)
        self.assertEqual(source_key(source), "gitlab:platform/reviews/heimdaal!42")

    def test_build_typed_sources(self) -> None:
        self.assertEqual(
            source_key(github.pull_request(owner="maskdotdev", repo="heimdaal", number=1)),
            "github:maskdotdev/heimdaal#1",
        )
        self.assertEqual(
            source_key(gitlab.merge_request(owner="maskdotdev", repo="heimdaal", number=2)),
            "gitlab:maskdotdev/heimdaal!2",
        )
        self.assertEqual(
            local(".", changed_files=["Cargo.toml"]).changed_files,
            ["Cargo.toml"],
        )

    def test_rejects_invalid_source_shorthand(self) -> None:
        with self.assertRaisesRegex(MuzenSourceError, "missing # review number delimiter"):
            parse_review_source("github:maskdotdev/heimdaal")


if __name__ == "__main__":
    unittest.main()
