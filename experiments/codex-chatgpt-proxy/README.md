# Codex ChatGPT Responses Proxy

Eval-only proxy for testing Muzen's agent loop with ChatGPT/Codex subscription
auth. This intentionally stays out of Muzen core because it targets the Codex
ChatGPT backend, not the stable OpenAI Platform API.

## Login

```sh
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs login
```

The default device flow prints a URL and code. Credentials are stored in
`experiments/codex-chatgpt-proxy/.auth.json` with file mode `0600`.

## Serve

To use a CodexBar-managed ChatGPT account, list the available accounts:

```sh
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs accounts
```

Then serve with one account selected by email, CodexBar account id, or workspace
account id:

```sh
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs serve \
  --codexbar-account maskdotdev@gmail.com
```

This reads the selected CodexBar managed home auth file directly and preserves
that file's Codex token shape when refreshing access tokens.

```sh
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs serve
```

Then run Muzen with the local proxy as the OpenAI-compatible endpoint:

```sh
OPENAI_BASE_URL=http://127.0.0.1:4141/v1 \
OPENAI_API_KEY=muzen-codex-proxy \
MODEL=gpt-5.4-mini \
node bench/review-quality/run-martian-suite.mjs \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/codex-chatgpt-live \
  --trace-output-dir bench/results-review-quality/traces/codex-chatgpt-live
```

`OPENAI_API_KEY` is a dummy value for Muzen's existing credential contract; the
proxy replaces it with the ChatGPT OAuth access token.

For a `gpt-5.5` low-reasoning eval, start the proxy with:

```sh
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs serve \
  --reasoning-effort low
```

Then run a single direct-session anti-cheat leg:

```sh
OPENAI_BASE_URL=http://127.0.0.1:4141/v1 \
OPENAI_API_KEY=muzen-codex-proxy \
MODEL=gpt-5.5 \
node bench/review-quality/run-anti-cheat.mjs \
  --fixture safe-sms-retry-cleanup \
  --mode direct_sessions \
  --sessions 1 \
  --runner-path target/release/muzen-runner \
  --output-dir bench/results-review-quality/codex-chatgpt-gpt-5.5-low-single \
  --trace-output-dir bench/results-review-quality/traces/codex-chatgpt-gpt-5.5-low-single
```

## Preflight

```sh
node experiments/codex-chatgpt-proxy/codex-chatgpt-responses-proxy.mjs preflight
```

The proxy normalizes the request for the Codex backend by setting `store: false`,
forcing upstream streaming, and dropping `max_output_tokens`, which this backend
rejects. If the caller requested `stream: true`, SSE is passed through. Otherwise
the proxy aggregates upstream SSE back into normal Responses JSON for simple
preflights and ad hoc checks.
