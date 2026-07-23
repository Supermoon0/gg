"""Length-bounded ``send_bytes``/``recv_bytes`` JSON channel."""

from __future__ import annotations

import itertools
import threading

from .protocol import (
    MAX_CONTROL_BYTES,
    ProtocolError,
    decode_envelope,
    encode_envelope,
    make_envelope,
)


class JsonChannel:
    def __init__(self, connection, renderer_id):
        self.connection = connection
        self.renderer_id = str(renderer_id)
        self._ids = itertools.count(1)
        self._send_lock = threading.Lock()
        self._seen_requests = set()

    def next_id(self):
        return str(next(self._ids))

    def send(self, message_type, *, payload=None, reply_to=None,
             document_token="", generation=0, msg_id=None):
        message = make_envelope(
            message_type, msg_id or self.next_id(),
            renderer_id=self.renderer_id,
            document_token=document_token, generation=generation,
            payload=payload, reply_to=reply_to)
        raw = encode_envelope(message)
        with self._send_lock:
            self.connection.send_bytes(raw)
        return message

    def recv(self):
        try:
            raw = self.connection.recv_bytes(MAX_CONTROL_BYTES)
        except OSError as exc:
            raise ProtocolError(
                "message_too_large", "peer exceeded control message limit") from exc
        message = decode_envelope(raw)
        if message["renderer_id"] != self.renderer_id:
            raise ProtocolError("wrong_renderer", "renderer identity mismatch")
        if message["reply_to"] is None:
            msg_id = message["msg_id"]
            if msg_id in self._seen_requests:
                raise ProtocolError("duplicate_msg_id", f"duplicate request {msg_id}")
            self._seen_requests.add(msg_id)
        return message

    def poll(self, timeout=0.0):
        return self.connection.poll(timeout)

    def close(self):
        try:
            self.connection.close()
        except OSError:
            pass
