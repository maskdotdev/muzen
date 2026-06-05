import type { ReviewStatus } from "./types.js";

export const WEBHOOK_STATUS_ACCEPTED = 202;
export const WEBHOOK_STATUS_OK = 200;

export type WebhookDelivery =
  | WebhookReviewCreatedDelivery
  | WebhookReviewDedupedDelivery
  | WebhookIgnoredDelivery;

export interface WebhookReviewCreatedDelivery {
  type: "review_created";
  deliveryId: string;
  reviewId: string;
  status: ReviewStatus;
}

export interface WebhookReviewDedupedDelivery {
  type: "review_deduped";
  deliveryId: string;
  reviewId: string;
  status: ReviewStatus;
}

export interface WebhookIgnoredDelivery {
  type: "ignored";
  deliveryId?: string | null;
  reason: string;
}

export interface WebhookHttpResponse {
  statusCode: number;
  headers: Record<string, string>;
  body: string;
}

export interface WebhookResponseOptions {
  headers?: HeadersInit;
}

export function createWebhookHttpResponse(
  delivery: WebhookDelivery,
  options: WebhookResponseOptions = {},
): WebhookHttpResponse {
  const headers = responseHeaders(options.headers);
  return {
    statusCode: webhookDeliveryStatus(delivery),
    headers: headersToRecord(headers),
    body: JSON.stringify(delivery),
  };
}

export function createWebhookResponse(
  delivery: WebhookDelivery,
  options: WebhookResponseOptions = {},
): Response {
  const response = createWebhookHttpResponse(delivery, options);
  return new Response(response.body, {
    status: response.statusCode,
    headers: response.headers,
  });
}

export function webhookDeliveryStatus(delivery: WebhookDelivery): number {
  switch (delivery.type) {
    case "review_deduped":
      return WEBHOOK_STATUS_OK;
    case "review_created":
    case "ignored":
      return WEBHOOK_STATUS_ACCEPTED;
  }
}

function responseHeaders(headers: HeadersInit | undefined): Headers {
  const result = new Headers(headers);
  if (!result.has("Content-Type")) {
    result.set("Content-Type", "application/json");
  }
  return result;
}

function headersToRecord(headers: Headers): Record<string, string> {
  const result: Record<string, string> = {};
  headers.forEach((value, key) => {
    result[key] = value;
  });
  return result;
}
