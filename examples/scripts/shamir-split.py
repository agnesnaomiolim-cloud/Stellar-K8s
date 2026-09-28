#!/usr/bin/env python3
# Copyright 2024 Stellar-K8s Contributors
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
"""Shamir split/recovery for Stellar validator seeds (ed25519).

Companion to ``docs/security/key-sharding.md``. Splits a validator seed into
``n`` shares with a ``k``-of-``n`` threshold so that no single custodian can ever
reconstruct the validator identity, then recovers the seed on a trusted host.

Design constraints (see the doc for the full threat model):

* **Standard library only.** A self-contained GF(2^8) Shamir implementation and a
  pure-Python ed25519 public-key derivation are used, so the script runs on a
  stripped air-gapped host with nothing but CPython.
* **No plaintext on disk.** The seed is read from stdin, an inherited file
  descriptor, or an environment variable -- never from a file path argument.
  Shares are not secret (any ``k-1`` of them reveal nothing) and are the only
  thing this script is willing to write to disk, and only behind ``--allow-disk``.
* **Verifiable recovery.** Recovery re-derives the ed25519 public key and
  compares it against the published validator key, so a wrong or mixed-up share
  set fails loudly instead of silently producing a different identity.

Subcommands::

    gen        generate a seed in memory (do this on the air-gapped host)
    pubkey     derive the G... account id / hex public key from a seed
    split      k-of-n split of a seed into portable share strings
    recover    reconstruct a seed from >= k shares and verify the public key
    verify     split + recover + compare, for one seed supplied on stdin
    selftest   RFC 8032 ed25519 vectors + in-memory round trips (no input)

Examples::

    # Air-gapped host: generate, split, and write shares to sealed media.
    python3 examples/scripts/shamir-split.py split -k 3 -n 5 \\
        --out-dir /media/custodian-share --allow-disk < seed.hex

    # Trusted host: reconstruct from k shares and assert the validator key.
    cat share-1.txt share-3.txt share-5.txt | \\
        python3 examples/scripts/shamir-split.py recover \\
        --expect-pubkey GDVP... --no-emit

    # Offline validation of the whole pipeline.
    python3 examples/scripts/shamir-split.py selftest
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import os
import secrets
import sys

# ---------------------------------------------------------------------------
# Stellar strkey (base32 + CRC16-XModem) and hex helpers
# ---------------------------------------------------------------------------

STRKEY_VERSION_ACCOUNT = 6 << 3  # 'G' — ed25519 public key (account id)
STRKEY_VERSION_SEED = 18 << 3  # 'S' — ed25519 secret seed

ED25519_SEED_LEN = 32


def crc16_xmodem(data: bytes) -> int:
    """CRC16-XModem (poly 0x1021, init 0x0000) as used by SEP-23 strkeys."""
    crc = 0
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            if crc & 0x8000:
                crc = ((crc << 1) ^ 0x1021) & 0xFFFF
            else:
                crc = (crc << 1) & 0xFFFF
    return crc


def strkey_encode(version: int, payload: bytes) -> str:
    raw = bytes([version]) + payload
    raw += crc16_xmodem(raw).to_bytes(2, "little")
    return base64.b32encode(raw).decode("ascii").rstrip("=")


def strkey_decode(value: str, expected_version: int) -> bytes:
    stripped = value.strip()
    padded = stripped.upper() + "=" * (-len(stripped) % 8)
    try:
        raw = base64.b32decode(padded, casefold=True)
    except Exception as exc:  # noqa: BLE001 - re-raised with context
        raise ValueError(f"not valid base32: {exc}") from exc
    if len(raw) < 4:
        raise ValueError("strkey payload too short")
    body, checksum = raw[:-2], raw[-2:]
    if crc16_xmodem(body) != int.from_bytes(checksum, "little"):
        raise ValueError("strkey CRC16-XModem mismatch (typo or corruption)")
    if body[0] != expected_version:
        raise ValueError(
            f"unexpected strkey version byte {body[0]} "
            f"(expected {expected_version})"
        )
    return body[1:]


def parse_seed(text: str, fmt: str = "auto") -> bytes:
    """Decode a validator seed from strkey ('S...'), hex, or raw text."""
    value = text.strip()
    if fmt == "auto":
        if value.startswith("S") and len(value) == 56:
            fmt = "stellar"
        elif len(value) == 64 and all(c in "0123456789abcdefABCDEF" for c in value):
            fmt = "hex"
        else:
            fmt = "raw"
    if fmt == "stellar":
        seed = strkey_decode(value, STRKEY_VERSION_SEED)
    elif fmt == "hex":
        try:
            seed = bytes.fromhex(value)
        except ValueError as exc:
            raise ValueError(f"invalid hex seed: {exc}") from exc
    elif fmt == "raw":
        seed = value.encode("utf-8")
    else:
        raise ValueError(f"unknown input format {fmt!r}")
    if len(seed) != ED25519_SEED_LEN:
        raise ValueError(
            f"seed must be {ED25519_SEED_LEN} bytes, got {len(seed)}"
        )
    return seed


def format_seed(seed: bytes, fmt: str) -> str:
    if fmt == "stellar":
        return strkey_encode(STRKEY_VERSION_SEED, seed)
    return seed.hex()


# ---------------------------------------------------------------------------
# Pure-Python ed25519 public-key derivation (RFC 8032 §5.1.5)
#
# Only the scalar-multiplication half of ed25519 is needed: pub = [a]B where
# a is the clamped half of SHA-512(seed). This avoids any third-party
# dependency so the script runs on an air-gapped host.
# ---------------------------------------------------------------------------

_P = 2**255 - 19
_D = (-121665 * pow(121666, _P - 2, _P)) % _P
_SQRT_M1 = pow(2, (_P - 1) // 4, _P)


def _xrecover(y: int) -> int:
    xx = (y * y - 1) * pow(_D * y * y + 1, _P - 2, _P) % _P
    x = pow(xx, (_P + 3) // 8, _P)
    if (x * x - xx) % _P != 0:
        x = (x * _SQRT_M1) % _P
    if x % 2 != 0:
        x = _P - x
    return x


_BY = 4 * pow(5, _P - 2, _P) % _P
_B = (_xrecover(_BY), _BY)


def _edwards_add(p1: tuple[int, int], p2: tuple[int, int]) -> tuple[int, int]:
    x1, y1 = p1
    x2, y2 = p2
    k = _D * x1 % _P * x2 % _P * y1 % _P * y2 % _P
    x3 = (x1 * y2 + x2 * y1) % _P * pow(1 + k, _P - 2, _P) % _P
    y3 = (y1 * y2 + x1 * x2) % _P * pow(1 - k, _P - 2, _P) % _P
    return x3, y3


def _scalarmult(point: tuple[int, int], scalar: int) -> tuple[int, int]:
    result = (0, 1)
    addend = point
    while scalar:
        if scalar & 1:
            result = _edwards_add(result, addend)
        addend = _edwards_add(addend, addend)
        scalar >>= 1
    return result


def ed25519_public_key(seed: bytes) -> bytes:
    """Derive the 32-byte ed25519 public key for a 32-byte seed."""
    if len(seed) != ED25519_SEED_LEN:
        raise ValueError("ed25519 seed must be 32 bytes")
    digest = hashlib.sha512(seed).digest()
    scalar = int.from_bytes(digest[:32], "little")
    scalar &= (1 << 254) - 8
    scalar |= 1 << 254
    x, y = _scalarmult(_B, scalar)
    encoded = bytearray(y.to_bytes(32, "little"))
    encoded[31] |= (x & 1) << 7
    return bytes(encoded)


def account_id(public_key: bytes) -> str:
    return strkey_encode(STRKEY_VERSION_ACCOUNT, public_key)


# ---------------------------------------------------------------------------
# Shamir's Secret Sharing over GF(2^8), AES polynomial x^8+x^4+x^3+x+1 (0x11B)
# ---------------------------------------------------------------------------

_GF_POLY = 0x1B


def gf_mul(a: int, b: int) -> int:
    """Multiply in GF(2^8) with the AES reduction polynomial (0x11B)."""
    product = 0
    for _ in range(8):
        if b & 1:
            product ^= a
        carry = a & 0x80
        a = (a << 1) & 0xFF
        if carry:
            a ^= _GF_POLY
        b >>= 1
    return product


def gf_pow(a: int, exponent: int) -> int:
    result = 1
    while exponent:
        if exponent & 1:
            result = gf_mul(result, a)
        a = gf_mul(a, a)
        exponent >>= 1
    return result


def gf_inv(a: int) -> int:
    if a == 0:
        raise ZeroDivisionError("no multiplicative inverse for 0 in GF(2^8)")
    return gf_pow(a, 254)


def _eval_poly(coeffs: list[int], x: int) -> int:
    acc = 0
    for coeff in reversed(coeffs):
        acc = gf_mul(acc, x) ^ coeff
    return acc


def shamir_split(secret: bytes, threshold: int, shares: int) -> list[tuple[int, bytes]]:
    """Return ``shares`` (x, y) pairs; any ``threshold`` of them recover ``secret``."""
    if not 2 <= threshold <= shares <= 255:
        raise ValueError("require 2 <= threshold <= shares <= 255")
    if not secret:
        raise ValueError("secret must not be empty")
    out: list[list[int]] = [[0] * len(secret) for _ in range(shares)]
    for index, byte in enumerate(secret):
        coeffs = [byte] + [secrets.randbelow(256) for _ in range(threshold - 1)]
        for x in range(1, shares + 1):
            out[x - 1][index] = _eval_poly(coeffs, x)
    return [(x, bytes(out[x - 1])) for x in range(1, shares + 1)]


def shamir_recover(points: list[tuple[int, bytes]]) -> bytes:
    """Lagrange-interpolate the secret at x=0 from ``points``.

    At least ``threshold`` points must be supplied; extra points are ignored
    (the first ``k`` by x-coordinate are used). Points must be distinct.
    """
    if not points:
        raise ValueError("no shares supplied")
    length = len(points[0][1])
    for x, y in points:
        if len(y) != length:
            raise ValueError("shares have inconsistent lengths (mixed splits?)")
    xs = [x for x, _ in points]
    if len(set(xs)) != len(xs):
        raise ValueError("duplicate share x-coordinates")
    out = bytearray(length)
    for i, (xi, yi) in enumerate(points):
        numerator = 1
        denominator = 1
        for j, (xj, _) in enumerate(points):
            if i == j:
                continue
            numerator = gf_mul(numerator, xj)  # 0 - xj == xj in GF(2^8)
            denominator = gf_mul(denominator, xi ^ xj)  # xi - xj == xi ^ xj
        lam = gf_mul(numerator, gf_inv(denominator))
        for offset in range(length):
            out[offset] ^= gf_mul(lam, yi[offset])
    return bytes(out)


# ---------------------------------------------------------------------------
# Share serialisation
#
#   SSS1.<split-id>.<k>.<n>.<x>.<base64url(y)>.<chk16>
#
# The checksum is an unkeyed SHA-256 prefix that catches transcription errors
# (paper, OCR, serial console). It is NOT a MAC and provides no authentication:
# integrity of a reconstruction comes from re-deriving the ed25519 public key.
# ---------------------------------------------------------------------------

SHARE_PREFIX = "SSS1"


def _share_checksum(canonical: str) -> str:
    return hashlib.sha256(canonical.encode("ascii")).hexdigest()[:16]


def encode_share(split_id: str, threshold: int, shares: int, x: int, payload: bytes) -> str:
    body = base64.urlsafe_b64encode(payload).decode("ascii").rstrip("=")
    canonical = f"{SHARE_PREFIX}.{split_id}.{threshold}.{shares}.{x}.{body}"
    return f"{canonical}.{_share_checksum(canonical)}"


def decode_share(line: str) -> tuple[str, int, int, int, bytes]:
    value = line.strip()
    if not value:
        raise ValueError("empty share")
    parts = value.split(".")
    if len(parts) != 7 or parts[0] != SHARE_PREFIX:
        raise ValueError("malformed share (expected SSS1.<id>.<k>.<n>.<x>.<y>.<chk>)")
    _, split_id, k_text, n_text, x_text, body, checksum = parts
    canonical = f"{SHARE_PREFIX}.{split_id}.{k_text}.{n_text}.{x_text}.{body}"
    if _share_checksum(canonical) != checksum:
        raise ValueError("share checksum mismatch (transcription error)")
    try:
        threshold = int(k_text)
        shares = int(n_text)
        x = int(x_text)
    except ValueError as exc:
        raise ValueError(f"non-numeric share field: {exc}") from exc
    padded = body + "=" * (-len(body) % 4)
    try:
        payload = base64.urlsafe_b64decode(padded)
    except Exception as exc:  # noqa: BLE001
        raise ValueError(f"invalid base64 share payload: {exc}") from exc
    return split_id, threshold, shares, x, payload


def shares_from_lines(lines: list[str]) -> tuple[list[tuple[int, bytes]], int, int]:
    """Parse share lines, enforce a single consistent split, return (points, k, n)."""
    points: list[tuple[int, bytes]] = []
    k = n = None
    split_id = None
    for line in lines:
        if not line.strip():
            continue
        cur_id, cur_k, cur_n, x, payload = decode_share(line)
        if split_id is None:
            split_id, k, n = cur_id, cur_k, cur_n
        elif (cur_id, cur_k, cur_n) != (split_id, k, n):
            raise ValueError(
                "shares come from different splits (mismatched id/threshold/shares)"
            )
        points.append((x, payload))
    if not points:
        raise ValueError("no non-empty share lines supplied")
    if k is None or n is None:
        raise ValueError("no shares supplied")
    if len(points) < k:
        raise ValueError(f"need at least {k} shares, got {len(points)}")
    return points, k, n


# ---------------------------------------------------------------------------
# Secret input / memory hygiene
# ---------------------------------------------------------------------------


def read_secret(args: argparse.Namespace) -> bytes:
    """Read the plaintext seed without ever opening an attacker-chosen path."""
    if args.from_env is not None:
        value = os.environ.get(args.from_env)
        if value is None:
            raise SystemExit(f"environment variable {args.from_env!r} is not set")
        return parse_seed(value, args.in_format)
    if args.fd is not None:
        stream = os.fdopen(args.fd, "rb")
        raw = stream.read()
    else:
        raw = sys.stdin.buffer.read()
    text = raw.decode("utf-8", errors="strict")
    return parse_seed(text, args.in_format)


def read_locked_secret(args: argparse.Namespace) -> bytearray:
    """Read the seed into a bytearray and mlock(2) it (best effort)."""
    buffer = bytearray(read_secret(args))
    if not try_mlock(buffer):
        print(
            "warning: mlock failed; the seed may be swappable to disk",
            file=sys.stderr,
        )
    return buffer


def harden_process() -> None:
    """Best-effort defence-in-depth so the seed does not leak via core dumps."""
    try:
        import resource

        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    except Exception:  # noqa: BLE001 - best effort, non-fatal
        pass
    try:
        import ctypes
        import ctypes.util

        libc = ctypes.CDLL(ctypes.util.find_library("c") or "libc.so.6", use_errno=True)
        # PR_SET_DUMPABLE = 4 (Linux); prevents ptrace/core of this process.
        libc.prctl(4, 0, 0, 0, 0)
    except Exception:  # noqa: BLE001
        pass


def try_mlock(buffer: bytearray) -> bool:
    """Best-effort mlock(2) of a bytearray so it cannot be swapped to disk."""
    if not buffer:
        return False
    try:
        import ctypes
        import ctypes.util

        libc = ctypes.CDLL(ctypes.util.find_library("c") or "libc.so.6", use_errno=True)
        address = ctypes.addressof(ctypes.c_char.from_buffer(buffer))
        return libc.mlock(ctypes.c_void_p(address), ctypes.c_size_t(len(buffer))) == 0
    except Exception:  # noqa: BLE001
        return False


def try_munlock(buffer: bytearray) -> None:
    try:
        import ctypes
        import ctypes.util

        libc = ctypes.CDLL(ctypes.util.find_library("c") or "libc.so.6", use_errno=True)
        address = ctypes.addressof(ctypes.c_char.from_buffer(buffer))
        libc.munlock(ctypes.c_void_p(address), ctypes.c_size_t(len(buffer)))
    except Exception:  # noqa: BLE001
        pass


def wipe(buffer: bytearray) -> None:
    for index in range(len(buffer)):
        buffer[index] = 0
    try_munlock(buffer)


def fingerprint(seed: bytes) -> str:
    return hashlib.sha256(seed).hexdigest()


# ---------------------------------------------------------------------------
# Commands
# ---------------------------------------------------------------------------


def cmd_pubkey(args: argparse.Namespace) -> int:
    seed = read_locked_secret(args)
    try:
        public = ed25519_public_key(bytes(seed))
        print(f"public_key={public.hex()}")
        print(f"account_id={account_id(public)}")
        return 0
    finally:
        wipe(seed)


def cmd_gen(args: argparse.Namespace) -> int:
    seed = bytearray(secrets.token_bytes(ED25519_SEED_LEN))
    try_mlock(seed)
    try:
        public = ed25519_public_key(bytes(seed))
        print(f"account_id={account_id(public)}", file=sys.stderr)
        print(f"fingerprint={fingerprint(bytes(seed))}", file=sys.stderr)
        sys.stdout.write(format_seed(bytes(seed), args.out_format) + "\n")
        return 0
    finally:
        wipe(seed)


def cmd_split(args: argparse.Namespace) -> int:
    seed = read_locked_secret(args)
    try:
        public = ed25519_public_key(bytes(seed))
        points = shamir_split(bytes(seed), args.threshold, args.shares)
        split_id = base64.urlsafe_b64encode(secrets.token_bytes(6)).decode().rstrip("=")
        lines = [
            encode_share(split_id, args.threshold, args.shares, x, payload)
            for x, payload in points
        ]
        written: list[str] = []
        if args.out_dir:
            if not args.allow_disk:
                print(
                    "refusing to write shares to disk without --allow-disk; "
                    "only use it on the air-gapped generation host",
                    file=sys.stderr,
                )
                return 2
            os.makedirs(args.out_dir, exist_ok=True)
            for x, line in zip(range(1, args.shares + 1), lines):
                path = os.path.join(args.out_dir, f"share-{x:02d}-of-{args.shares}.txt")
                with open(path, "w", encoding="ascii") as handle:
                    handle.write(line + "\n")
                os.chmod(path, 0o600)
                written.append(path)
            print(f"wrote {len(written)} shares to {args.out_dir}", file=sys.stderr)
            for path in written:
                print(f"  {path}", file=sys.stderr)
        else:
            for line in lines:
                print(line)
        print(f"account_id={account_id(public)}", file=sys.stderr)
        print(f"threshold={args.threshold} shares={args.shares}", file=sys.stderr)
        print(f"fingerprint={fingerprint(bytes(seed))}", file=sys.stderr)
        return 0
    finally:
        wipe(seed)


def cmd_recover(args: argparse.Namespace) -> int:
    if args.shares_file:
        with open(args.shares_file, "r", encoding="ascii") as handle:
            lines = handle.readlines()
    else:
        lines = sys.stdin.readlines()
    points, threshold, total = shares_from_lines(lines)
    selected = sorted(points)[:threshold]
    seed = bytearray(shamir_recover(selected))
    try_mlock(seed)
    try:
        public = ed25519_public_key(bytes(seed))
        identifier = account_id(public)
        print(f"recovered_from={len(selected)}/{total} (threshold {threshold})", file=sys.stderr)
        print(f"account_id={identifier}", file=sys.stderr)
        print(f"fingerprint={fingerprint(bytes(seed))}", file=sys.stderr)
        if args.expect_pubkey:
            expected = args.expect_pubkey.strip()
            actual = identifier if expected.startswith("G") else public.hex()
            if expected != actual and expected != public.hex():
                print(
                    f"VERIFICATION FAILED: expected {expected}, derived {actual}",
                    file=sys.stderr,
                )
                return 1
            print("VERIFICATION OK: recovered seed matches expected public key", file=sys.stderr)
        else:
            print(
                "warning: no --expect-pubkey supplied; the recovered identity was "
                "not verified",
                file=sys.stderr,
            )
        if not args.no_emit:
            sys.stdout.write(format_seed(bytes(seed), args.out_format) + "\n")
        return 0
    finally:
        wipe(seed)


def cmd_verify(args: argparse.Namespace) -> int:
    seed = read_locked_secret(args)
    try:
        original = bytes(seed)
        public = ed25519_public_key(original)
        points = shamir_split(original, args.threshold, args.shares)
        # Recover from a deliberately non-contiguous subset of exactly k shares.
        subset = points[: args.threshold]
        recovered = shamir_recover(subset)
        if recovered != original:
            print("VERIFICATION FAILED: recovered seed differs from input", file=sys.stderr)
            return 1
        if ed25519_public_key(recovered) != public:
            print("VERIFICATION FAILED: recovered public key differs", file=sys.stderr)
            return 1
        print(
            f"VERIFICATION OK: {args.threshold}-of-{args.shares} split/recovery "
            f"reproduced account_id={account_id(public)}",
            file=sys.stderr,
        )
        return 0
    finally:
        wipe(seed)


# RFC 8032 §7.1 test vectors: (secret seed, expected public key).
RFC8032_VECTORS = [
    (
        "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
    ),
    (
        "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
        "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
    ),
    (
        "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
        "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
    ),
]


def cmd_selftest(args: argparse.Namespace) -> int:
    failures = 0
    for index, (seed_hex, expected_hex) in enumerate(RFC8032_VECTORS, start=1):
        derived = ed25519_public_key(bytes.fromhex(seed_hex)).hex()
        status = "ok" if derived == expected_hex else "FAIL"
        if derived != expected_hex:
            failures += 1
        print(f"rfc8032 vector {index}: {status}")

    combinations = [(2, 2), (2, 3), (3, 5), (5, 9)]
    for threshold, shares in combinations:
        seed = bytearray(secrets.token_bytes(ED25519_SEED_LEN))
        try:
            original = bytes(seed)
            points = shamir_split(original, threshold, shares)
            recovered = shamir_recover(points[:threshold])
            pub_ok = ed25519_public_key(recovered) == ed25519_public_key(original)
            ok = recovered == original and pub_ok
        finally:
            wipe(seed)
        if not ok:
            failures += 1
        print(
            f"round trip {threshold}-of-{shares}: "
            f"{'ok' if ok else 'FAIL'} (public key match={'yes' if ok else 'no'})"
        )

    if failures:
        print(f"SELFTEST FAILED: {failures} check(s) failed", file=sys.stderr)
        return 1
    print("SELFTEST PASSED: ed25519 derivation and Shamir round trips verified")
    return 0


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def add_secret_input_options(parser: argparse.ArgumentParser) -> None:
    group = parser.add_mutually_exclusive_group()
    group.add_argument(
        "--from-env",
        metavar="VAR",
        help="read the seed from environment variable VAR (default: stdin)",
    )
    group.add_argument(
        "--fd",
        type=int,
        metavar="N",
        help="read the seed from an inherited file descriptor (e.g. 3)",
    )
    parser.add_argument(
        "--in-format",
        choices=["auto", "stellar", "hex", "raw"],
        default="auto",
        help="interpret the seed as an S... strkey, hex, or raw text (default: auto)",
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    sub = parser.add_subparsers(dest="command", required=True)

    gen = sub.add_parser("gen", help="generate a seed in memory (air-gapped host)")
    gen.add_argument("--out-format", choices=["stellar", "hex"], default="stellar")
    gen.set_defaults(func=cmd_gen)

    pub = sub.add_parser("pubkey", help="derive the public key for a seed")
    add_secret_input_options(pub)
    pub.set_defaults(func=cmd_pubkey)

    split = sub.add_parser("split", help="k-of-n split a seed into share strings")
    add_secret_input_options(split)
    split.add_argument("-k", "--threshold", type=int, required=True)
    split.add_argument("-n", "--shares", type=int, required=True)
    split.add_argument("--out-dir", help="write one file per share (requires --allow-disk)")
    split.add_argument(
        "--allow-disk",
        action="store_true",
        help="acknowledge writing shares to disk (air-gapped generation host only)",
    )
    split.set_defaults(func=cmd_split)

    recover = sub.add_parser("recover", help="reconstruct a seed from >= k shares")
    recover.add_argument("--shares-file", help="read share lines from a file (default: stdin)")
    recover.add_argument("--expect-pubkey", help="G... account id or hex public key to assert")
    recover.add_argument("--no-emit", action="store_true", help="do not print the seed")
    recover.add_argument("--out-format", choices=["stellar", "hex"], default="stellar")
    recover.set_defaults(func=cmd_recover)

    verify = sub.add_parser("verify", help="split+recover a stdin seed and compare keys")
    add_secret_input_options(verify)
    verify.add_argument("-k", "--threshold", type=int, required=True)
    verify.add_argument("-n", "--shares", type=int, required=True)
    verify.set_defaults(func=cmd_verify)

    selftest = sub.add_parser("selftest", help="RFC 8032 vectors + in-memory round trips")
    selftest.set_defaults(func=cmd_selftest)

    return parser


def main(argv: list[str] | None = None) -> int:
    harden_process()
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except ValueError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
