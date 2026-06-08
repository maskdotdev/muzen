+# Agent Rules
+
+- Muzen is unreleased. Do not preserve backwards compatibility, fallback paths, aliases, legacy field names, or migration shims unless the user explicitly asks for them.
+- Do not add callback-based escape hatches or compatibility adapters as a default design choice. Prefer one direct product path with explicit contracts, and delete obsolete paths instead of routing around them.
+- If a change would create a second mode only to preserve older behavior, make the stronger behavior the only behavior and let existing budgets, limits, and tests define scope.
