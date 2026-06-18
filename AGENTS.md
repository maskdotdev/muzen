+# Agent Rules
+
+- Muzen is unreleased. Do not preserve backwards compatibility, fallback paths, aliases, legacy field names, or migration shims unless the user explicitly asks for them.
+- Do not add callback-based escape hatches or compatibility adapters as a default design choice. Prefer one direct product path with explicit contracts, and delete obsolete paths instead of routing around them.
+- If a change would create a second mode only to preserve older behavior, make the stronger behavior the only behavior and let existing budgets, limits, and tests define scope.
+- Do not tailor Muzen review logic to individual benchmark PRs, golden failures, or narrow bug examples. Treat benchmark misses as diagnostic evidence for general reviewer behavior only. Do not add PR-specific predicates, fixtures, heuristics, scoring hacks, golden-aware prompts, or prompt/schema examples copied from a benchmark PR. If a prompt needs an example, make it domain-neutral or draw it from a synthetic case that is not part of the scored corpus.
