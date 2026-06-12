from __future__ import annotations

from typing import Optional, Union

from .types import (
    AnthropicReviewModelSpec,
    OpenAIReviewModelSpec,
    ReviewModelCredential,
)


def openai(
    model: str,
    *,
    credential: Optional[Union[ReviewModelCredential, dict]] = None,
    base_url: Optional[str] = None,
    api_protocol: str = "responses",
    max_input_tokens: Optional[int] = None,
    max_output_tokens: Optional[int] = None,
    temperature: Optional[float] = None,
    top_p: Optional[float] = None,
) -> OpenAIReviewModelSpec:
    model = model.strip()
    if not model:
        raise ValueError("openai(...) requires a non-empty model")
    if api_protocol not in ("responses", "chat_completions"):
        raise ValueError("openai(...) api_protocol must be responses or chat_completions")
    resolved_credential = _credential(credential)
    _validate_positive_integer(max_input_tokens, "max_input_tokens")
    _validate_positive_integer(max_output_tokens, "max_output_tokens")
    _validate_range(temperature, "temperature", 0, 2)
    _validate_range(top_p, "top_p", 0, 1)
    return OpenAIReviewModelSpec(
        kind="provider",
        provider="openai",
        model=model,
        credential=resolved_credential,
        base_url=base_url.strip() if base_url and base_url.strip() else None,
        api_protocol=api_protocol,  # type: ignore[arg-type]
        max_input_tokens=max_input_tokens,
        max_output_tokens=max_output_tokens,
        temperature=temperature,
        top_p=top_p,
    )


def anthropic(
    model: str,
    *,
    credential: Optional[Union[ReviewModelCredential, dict]] = None,
    base_url: Optional[str] = None,
    max_input_tokens: Optional[int] = None,
    max_output_tokens: Optional[int] = None,
    temperature: Optional[float] = None,
    top_p: Optional[float] = None,
) -> AnthropicReviewModelSpec:
    model = model.strip()
    if not model:
        raise ValueError("anthropic(...) requires a non-empty model")
    resolved_credential = _credential(credential, default_env="ANTHROPIC_API_KEY")
    _validate_positive_integer(max_input_tokens, "max_input_tokens")
    _validate_positive_integer(max_output_tokens, "max_output_tokens")
    _validate_range(temperature, "temperature", 0, 2)
    _validate_range(top_p, "top_p", 0, 1)
    return AnthropicReviewModelSpec(
        kind="provider",
        provider="anthropic",
        model=model,
        credential=resolved_credential,
        base_url=base_url.strip() if base_url and base_url.strip() else None,
        api_protocol="messages",
        max_input_tokens=max_input_tokens,
        max_output_tokens=max_output_tokens,
        temperature=temperature,
        top_p=top_p,
    )


def _credential(
    credential: Optional[Union[ReviewModelCredential, dict]],
    *,
    default_env: str = "OPENAI_API_KEY",
) -> ReviewModelCredential:
    if credential is None:
        return ReviewModelCredential(env=default_env)
    if isinstance(credential, ReviewModelCredential):
        resolved = credential
    else:
        resolved = ReviewModelCredential(
            env=credential.get("env"),
            secret_ref=credential.get("secretRef") or credential.get("secret_ref"),
        )
    if bool(resolved.env) == bool(resolved.secret_ref):
        raise ValueError("model credential must be exactly one of env or secret_ref")
    return resolved


def _validate_positive_integer(value: Optional[int], field: str) -> None:
    if value is not None and (not isinstance(value, int) or value <= 0):
        raise ValueError(f"openai(...) {field} must be a positive integer")


def _validate_range(
    value: Optional[float], field: str, minimum: float, maximum: float
) -> None:
    if value is not None and not (minimum <= value <= maximum):
        raise ValueError(f"openai(...) {field} must be between {minimum} and {maximum}")
