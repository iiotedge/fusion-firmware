#!/usr/bin/env bash
# The configurable attestation, end to end:
#
#   1. `--matter-test-attestation DIR` writes the test set as the four files.
#   2. A firmware booted with `attestation = "files"` and a custom setup passcode and
#      discriminator is added by a real controller (attestation.mjs): the files really
#      were what it presented, and the custom code is the one that works.
#   3. `--check-config` accepts that config and names what it loaded, and REFUSES, with
#      the file and the reason, a wrong key, another vendor id, a missing file and a
#      test-mode config that asks for a vendor id only real certificates allow.
#
#   tests/matter-controller/attestation-check.sh
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../.." && pwd)"
BIN="${FUSION_BIN:-$REPO/target/debug/fusion-firmware}"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/fusion-attestation-check.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
FAILED=0
say() { printf '%s  %s\n' "$1" "$2"; [ "$1" = PASS ] || FAILED=1; }

"$BIN" --matter-test-attestation "$WORK/att" >/dev/null
for f in dac.der dac.key pai.der cd.der; do [ -s "$WORK/att/$f" ] || { say FAIL "--matter-test-attestation wrote $f"; exit 1; }; done
say PASS "--matter-test-attestation writes dac.der, dac.key, pai.der and cd.der"

KEYS="attestation = \"files\"
dac_file = \"$WORK/att/dac.der\"
dac_key_file = \"$WORK/att/dac.key\"
pai_file = \"$WORK/att/pai.der\"
cd_file = \"$WORK/att/cd.der\"
setup_passcode = 31415926
discriminator = 2020"

echo "--- a real controller adds a node that presents the files, with the custom setup code"
set +e
MATTER_KEYS="$KEYS" MATTER_SCRIPT=attestation.mjs MATTER_SCRIPT_ARGS="--passcode 31415926 --discriminator 2020" MATTER_EXTRA_TOML= "$HERE/run.sh"
[ $? -eq 0 ] || FAILED=1
set -e

# A config file for --check-config: the repo default with these [matter] keys.
config_with() {
  python3 - "$REPO/config/iiotedge_default.toml" "$1" "$2" <<'PY'
import re, sys
src, dst, keys = sys.argv[1:4]
s = open(src).read()
assert "[matter]\nenabled = false" in s
if "attestation" in keys:
    s = re.sub(r'(?m)^attestation = "test".*\n', "", s, count=1)  # the template sets it; the keys replace it
open(dst, "w").write(s.replace("[matter]\nenabled = false", "[matter]\nenabled = true\n" + keys, 1))
PY
}
expect_ok() {
  config_with "$WORK/ok.toml" "$KEYS"
  out="$("$BIN" --check-config "$WORK/ok.toml" 2>&1)" && case "$out" in
    *"attestation=files vid=0xFFF1 pid=0x8001"*) say PASS "--check-config accepts the files and says what it loaded" ;;
    *) say FAIL "--check-config output: $out" ;;
  esac || say FAIL "--check-config refused a good config: $out"
}
expect_refused() { # description, keys, text the refusal must contain
  config_with "$WORK/bad.toml" "$2"
  if out="$("$BIN" --check-config "$WORK/bad.toml" 2>&1)"; then
    say FAIL "$1: accepted ($out)"
  else
    case "$out" in *"$3"*) say PASS "$1: refused ('$3')" ;; *) say FAIL "$1: refused for another reason: $out" ;; esac
  fi
}
expect_ok

python3 - "$WORK/att/dac.key" "$WORK/att/other.key" <<'PY'
import sys
b = bytearray(open(sys.argv[1], "rb").read())
# flip a base64 character in the middle of the private scalar: a different, still valid, key
i = b.index(b"\n") + 1 + 30
b[i] = ord("A") if b[i] != ord("A") else ord("B")
open(sys.argv[2], "wb").write(bytes(b))
PY
expect_refused "another device's key" "${KEYS/dac.key/other.key}" "does not belong to the DAC"
expect_refused "a vendor id the DAC was not issued for" "$KEYS
vendor_id = 0x1234" "0xFFF1"
expect_refused "a missing file" "${KEYS/cd.der/nope.der}" "nope.der"
expect_refused "files without all four paths" "attestation = \"files\"
dac_file = \"$WORK/att/dac.der\"" "matter.dac_key_file"
expect_refused "a vendor id with test attestation" "vendor_id = 0x1234" "attestation = \"files\""
expect_refused "an invalid setup passcode" "setup_passcode = 11111111" "not a valid Matter passcode"

[ "$FAILED" -eq 0 ] && echo "attestation check: all passed" || { echo "attestation check: FAILED"; exit 1; }
