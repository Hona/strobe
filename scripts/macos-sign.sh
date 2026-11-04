#!/bin/bash
# Signs a strobe binary with a Developer ID Application certificate under the hardened runtime and
# notarizes it (the Release workflow, macOS jobs). macOS keeps a privacy grant (Accessibility, Screen
# Recording) for the code's designated requirement: for an ad-hoc signature that is its hash, which
# every release changes, so each update silently lost the grants; for a Developer ID signature it is
# the identifier and the team, which stay.
#
# Environment (repository secrets):
#   MACOS_CERTIFICATE_P12       the Developer ID Application certificate with its private key, a .p12, base64
#   MACOS_CERTIFICATE_PASSWORD  the .p12's password
#   MACOS_NOTARY_KEY_P8         an App Store Connect API key (.p8 file contents, the PEM text)
#   MACOS_NOTARY_KEY_ID         that key's id
#   MACOS_NOTARY_ISSUER_ID      the issuer id of the App Store Connect team
set -euo pipefail
bin="$1"
tmp="${RUNNER_TEMP:-$(mktemp -d)}"
keychain="$tmp/strobe-signing.keychain-db"
pass="$(uuidgen)"
cleanup() {
  security delete-keychain "$keychain" 2>/dev/null || true
  rm -f "$tmp/cert.p12" "$tmp/notary.p8" "$tmp/strobe.zip"
}
trap cleanup EXIT

security create-keychain -p "$pass" "$keychain"
security set-keychain-settings -lut 3600 "$keychain"
security unlock-keychain -p "$pass" "$keychain"
printf '%s' "$MACOS_CERTIFICATE_P12" | base64 --decode > "$tmp/cert.p12"
security import "$tmp/cert.p12" -k "$keychain" -P "$MACOS_CERTIFICATE_PASSWORD" -T /usr/bin/codesign >/dev/null
security set-key-partition-list -S apple-tool:,apple: -s -k "$pass" "$keychain" >/dev/null
# shellcheck disable=SC2046
security list-keychains -d user -s "$keychain" $(security list-keychains -d user | tr -d '"')
identity="$(security find-identity -v -p codesigning "$keychain" | awk -F'"' '/Developer ID Application/ { print $2; exit }')"
if [ -z "$identity" ]; then
  echo "::error::MACOS_CERTIFICATE_P12 holds no Developer ID Application identity"
  exit 1
fi

# A fixed identifier: with the team, it is what the grants are kept for.
codesign --force --timestamp --options runtime --identifier dev.hona.strobe \
  --entitlements packaging/macos/strobe.entitlements --sign "$identity" "$bin"
codesign --verify --strict --verbose=2 "$bin"
codesign --display --requirements - "$bin"

# A bare executable is notarized inside a zip; no ticket can be stapled to it, so Gatekeeper looks the
# ticket up online the first time the binary runs.
printf '%s\n' "$MACOS_NOTARY_KEY_P8" > "$tmp/notary.p8"
ditto -c -k --keepParent "$bin" "$tmp/strobe.zip"
out="$(xcrun notarytool submit "$tmp/strobe.zip" --key "$tmp/notary.p8" --key-id "$MACOS_NOTARY_KEY_ID" --issuer "$MACOS_NOTARY_ISSUER_ID" --wait --timeout 30m --output-format json)"
echo "$out"
if ! printf '%s' "$out" | grep -q '"status" *: *"Accepted"'; then
  id="$(printf '%s' "$out" | sed -n 's/.*"id" *: *"\([^"]*\)".*/\1/p' | head -1)"
  [ -n "$id" ] && xcrun notarytool log "$id" --key "$tmp/notary.p8" --key-id "$MACOS_NOTARY_KEY_ID" --issuer "$MACOS_NOTARY_ISSUER_ID" || true
  echo "::error::notarization was not accepted"
  exit 1
fi
