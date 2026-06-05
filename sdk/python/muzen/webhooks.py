from __future__ import annotations

import json
from dataclasses import asdict, is_dataclass
from typing import Any, Dict, Mapping, Optional, Union

from .types import WebhookDelivery, WebhookHttpResponse

WEBHOOK_STATUS_ACCEPTED = 202
WEBHOOK_STATUS_OK = 200

WebhookDeliveryLike = Union[WebhookDelivery, Mapping[str, Any]]


def create_webhook_response(
    delivery: WebhookDeliveryLike,
    *,
    headers: Optional[Mapping[str, str]] = None,
) -> WebhookHttpResponse:
    response_headers: Dict[str, str] = dict(headers or {})
    if not _has_header(response_headers, "Content-Type"):
        response_headers["Content-Type"] = "application/json"
    body = _delivery_body(delivery)
    return WebhookHttpResponse(
        status_code=webhook_delivery_status(delivery),
        headers=response_headers,
        body=json.dumps(body, separators=(",", ":")),
    )


def webhook_delivery_status(delivery: WebhookDeliveryLike) -> int:
    delivery_type = _delivery_type(delivery)
    if delivery_type == "review_deduped":
        return WEBHOOK_STATUS_OK
    if delivery_type in ("review_created", "ignored"):
        return WEBHOOK_STATUS_ACCEPTED
    raise ValueError(f"unsupported webhook delivery type {delivery_type!r}")


def _delivery_body(delivery: WebhookDeliveryLike) -> Dict[str, Any]:
    if isinstance(delivery, WebhookDelivery):
        body = _camel_dict(asdict(delivery))
        return {key: value for key, value in body.items() if value is not None}
    if is_dataclass(delivery):
        body = _camel_dict(asdict(delivery))
        return {key: value for key, value in body.items() if value is not None}
    return dict(delivery)


def _delivery_type(delivery: WebhookDeliveryLike) -> Any:
    if isinstance(delivery, WebhookDelivery):
        return delivery.type
    if is_dataclass(delivery):
        return getattr(delivery, "type")
    return delivery.get("type")


def _camel_dict(value: Mapping[str, Any]) -> Dict[str, Any]:
    return {_camel_key(key): item for key, item in value.items()}


def _camel_key(key: str) -> str:
    parts = key.split("_")
    return parts[0] + "".join(part[:1].upper() + part[1:] for part in parts[1:])


def _has_header(headers: Mapping[str, str], name: str) -> bool:
    normalized = name.lower()
    return any(header.lower() == normalized for header in headers)
