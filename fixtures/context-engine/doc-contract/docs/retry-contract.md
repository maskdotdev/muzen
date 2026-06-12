# Retry Contract

The retry budget is enforced by [retry-policy](../src/runtime/retry-policy.ts).

Regression coverage lives in [retry-policy.test](../tests/retry-policy.test.ts).

Operators should not infer retry behavior from unrelated cache or queue docs.
