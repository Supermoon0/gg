"""Versioned JSON envelope and validation rules for local process IPC."""

from __future__ import annotations

import json


PROTOCOL_VERSION = 1
BUILD_ID = "gg-renderer-ipc-v1"
MAX_CONTROL_BYTES = 1024 * 1024
MAX_URL_BYTES = 16 * 1024
MAX_HEADER_BYTES = 64 * 1024
MAX_IN_FLIGHT = 64

ENVELOPE_FIELDS = {
    "v", "type", "msg_id", "reply_to", "renderer_id",
    "document_token", "generation", "payload",
}

MESSAGE_TYPES = {
    "hello", "hello_ack", "protocol_error",
    "renderer.commit", "renderer.tick", "renderer.settle",
    "renderer.frame", "renderer.run", "renderer.query",
    "renderer.snapshot", "renderer.export", "renderer.click",
    "renderer.event", "renderer.set_attr", "renderer.remove_attr",
    "renderer.state", "renderer.has_pending", "renderer.set_focus",
    "renderer.set_hover", "renderer.set_layout_rects",
    "renderer.restyle_diff", "renderer.close", "renderer.shutdown",
    "renderer.test_crash", "renderer.test_hang", "renderer.test_stale",
    "renderer.response", "renderer.error",
    "broker.bind_context", "broker.drop_context",
    "broker.request_text", "broker.request", "broker.request_raw",
    "broker.script_fetch", "broker.cookies_for", "broker.cookie_set",
    "broker.response", "broker.error",
}


class ProtocolError(RuntimeError):
    def __init__(self, code, message):
        super().__init__(message)
        self.code = str(code)


def make_envelope(message_type, msg_id, *, renderer_id,
                  document_token="", generation=0, payload=None,
                  reply_to=None):
    message = {
        "v": PROTOCOL_VERSION,
        "type": message_type,
        "msg_id": str(msg_id),
        "reply_to": None if reply_to is None else str(reply_to),
        "renderer_id": str(renderer_id),
        "document_token": str(document_token),
        "generation": int(generation),
        "payload": {} if payload is None else payload,
    }
    validate_envelope(message)
    return message


def _payload_limits(value, key=""):
    if isinstance(value, dict):
        for child_key, child in value.items():
            if not isinstance(child_key, str):
                raise ProtocolError("invalid_payload", "payload keys must be strings")
            _payload_limits(child, child_key.casefold())
        headers = value.get("headers")
        if isinstance(headers, dict):
            total = sum(
                len(str(name).encode("utf-8"))
                + len(str(item).encode("utf-8"))
                for name, item in headers.items())
            if total > MAX_HEADER_BYTES:
                raise ProtocolError("headers_too_large", "header block exceeds limit")
    elif isinstance(value, list):
        for child in value:
            _payload_limits(child, key)
    elif key in {"url", "final_url", "document_url", "base_url"}:
        if len(str(value).encode("utf-8")) > MAX_URL_BYTES:
            raise ProtocolError("url_too_large", "URL exceeds limit")


def validate_envelope(message):
    if not isinstance(message, dict):
        raise ProtocolError("invalid_envelope", "message must be an object")
    fields = set(message)
    if fields != ENVELOPE_FIELDS:
        unknown = sorted(fields - ENVELOPE_FIELDS)
        missing = sorted(ENVELOPE_FIELDS - fields)
        raise ProtocolError(
            "invalid_fields", f"unknown={unknown!r} missing={missing!r}")
    if message["v"] != PROTOCOL_VERSION:
        raise ProtocolError("bad_version", "protocol version mismatch")
    if message["type"] not in MESSAGE_TYPES:
        raise ProtocolError("unknown_type", f"unknown type {message['type']!r}")
    if not isinstance(message["msg_id"], str) or not message["msg_id"]:
        raise ProtocolError("invalid_msg_id", "msg_id must be a non-empty string")
    if message["reply_to"] is not None and not isinstance(
            message["reply_to"], str):
        raise ProtocolError("invalid_reply", "reply_to must be a string or null")
    if not isinstance(message["renderer_id"], str):
        raise ProtocolError("invalid_renderer", "renderer_id must be a string")
    if not isinstance(message["document_token"], str):
        raise ProtocolError("invalid_document", "document_token must be a string")
    generation = message["generation"]
    if isinstance(generation, bool) or not isinstance(generation, int) \
            or generation < 0:
        raise ProtocolError("invalid_generation", "generation must be non-negative")
    if not isinstance(message["payload"], dict):
        raise ProtocolError("invalid_payload", "payload must be an object")
    _payload_limits(message["payload"])
    return message


def encode_envelope(message):
    validate_envelope(message)
    try:
        raw = json.dumps(
            message, ensure_ascii=False, separators=(",", ":"),
            allow_nan=False).encode("utf-8")
    except (TypeError, ValueError) as exc:
        raise ProtocolError("not_json", str(exc)) from exc
    if len(raw) > MAX_CONTROL_BYTES:
        raise ProtocolError("message_too_large", "control message exceeds limit")
    return raw


def decode_envelope(raw):
    if len(raw) > MAX_CONTROL_BYTES:
        raise ProtocolError("message_too_large", "control message exceeds limit")
    try:
        message = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ProtocolError("malformed_json", str(exc)) from exc
    return validate_envelope(message)
