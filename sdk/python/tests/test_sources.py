import unittest

from muzen import (
    custom_source,
    github,
    gitlab,
    local,
    parse_review_source,
    perforce,
    raw_snapshot,
    ReviewSource,
    source_key,
)
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
        self.assertEqual(local("."), ReviewSource(type="local", repo="."))
        self.assertEqual(
            source_key(raw_snapshot("/tmp/snapshot")),
            "raw_snapshot:/tmp/snapshot",
        )
        self.assertEqual(
            source_key(perforce("perforce.example:1666", "12345")),
            "perforce:perforce.example:1666@12345",
        )
        self.assertEqual(
            source_key(custom_source("acme", "review-1")),
            "custom:acme:review-1",
        )

    def test_parse_non_git_source_shorthands(self) -> None:
        self.assertEqual(
            parse_review_source("raw_snapshot:/tmp/snapshot").type,
            "raw_snapshot",
        )
        self.assertEqual(
            parse_review_source("perforce:perforce.example:1666@12345").changelist,
            "12345",
        )
        self.assertEqual(
            parse_review_source("custom:acme:review-1").provider,
            "acme",
        )

    def test_rejects_invalid_source_shorthand(self) -> None:
        with self.assertRaisesRegex(MuzenSourceError, "missing # review number delimiter"):
            parse_review_source("github:maskdotdev/heimdaal")


if __name__ == "__main__":
    unittest.main()
