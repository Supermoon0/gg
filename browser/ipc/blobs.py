"""Browser-created shared-memory blobs for non-control payload bytes."""

from __future__ import annotations

import base64
import hashlib
from multiprocessing import shared_memory


INLINE_LIMIT = 64 * 1024
MAX_BLOB_BYTES = 64 * 1024 * 1024


class BlobError(RuntimeError):
    pass


class BlobStore:
    def __init__(self):
        self._leases = []

    def pack(self, data, media_type="application/octet-stream"):
        data = bytes(data)
        if len(data) <= INLINE_LIMIT:
            return {
                "kind": "inline", "size": len(data),
                "sha256": hashlib.sha256(data).hexdigest(),
                "media_type": media_type,
                "data": base64.b64encode(data).decode("ascii"),
            }
        if len(data) > MAX_BLOB_BYTES:
            raise BlobError("blob exceeds process quota")
        block = shared_memory.SharedMemory(create=True, size=len(data))
        block.buf[:len(data)] = data
        self._leases.append(block)
        return {
            "kind": "shared_memory", "size": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
            "media_type": media_type, "name": block.name,
        }

    def release_all(self):
        while self._leases:
            block = self._leases.pop()
            try:
                block.close()
            finally:
                try:
                    block.unlink()
                except FileNotFoundError:
                    pass


def read_blob(reference):
    if not isinstance(reference, dict):
        raise BlobError("invalid blob reference")
    size = reference.get("size")
    if isinstance(size, bool) or not isinstance(size, int) \
            or size < 0 or size > MAX_BLOB_BYTES:
        raise BlobError("invalid blob size")
    kind = reference.get("kind")
    if kind == "inline":
        try:
            data = base64.b64decode(reference.get("data", ""), validate=True)
        except Exception as exc:
            raise BlobError("invalid inline blob") from exc
    elif kind == "shared_memory":
        block = shared_memory.SharedMemory(name=reference.get("name"))
        try:
            if len(block.buf) < size:
                raise BlobError("mapping smaller than declared blob")
            data = bytes(block.buf[:size])
        finally:
            block.close()
    else:
        raise BlobError("unknown blob transport")
    if len(data) != size:
        raise BlobError("blob size mismatch")
    if hashlib.sha256(data).hexdigest() != reference.get("sha256"):
        raise BlobError("blob hash mismatch")
    return data
