#!/usr/bin/env bash
# Re-download the pinned GraphiQL assets and verify their checksums.
#
# The files are committed so builds never need the network. Update the version and checksum
# constants here and in versions.txt together, run this script, then run the suku test suite
# with --features vendored-graphiql.
set -euo pipefail

GRAPHIQL_VERSION="4.1.2"
REACT_VERSION="18.3.1"
REACT_DOM_VERSION="18.3.1"

cd "$(dirname "$0")"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

fetch() {
  local url="$1" output="$2" expected="$3" actual
  curl -fsSL "$url" -o "$output"
  actual="$(sha256 "$output")"
  if [ "$actual" != "$expected" ]; then
    echo "checksum mismatch for $output: expected $expected, got $actual" >&2
    exit 1
  fi
  echo "ok $output ($actual)"
}

fetch "https://unpkg.com/graphiql@${GRAPHIQL_VERSION}/graphiql.min.js" \
  graphiql.min.js 867226ba8af5ce7949903fc53932f78dac9f4bfc6cc8da0fd3869b8bc6c5256b
fetch "https://unpkg.com/graphiql@${GRAPHIQL_VERSION}/graphiql.min.css" \
  graphiql.min.css 30487e07635d312a63f647b140d37740a73c53332b0975bf4b1fe985ca6ec885
fetch "https://unpkg.com/react@${REACT_VERSION}/umd/react.production.min.js" \
  react.production.min.js d949f1c3687aedadcedac85261865f29b17cd273997e7f6b2bfc53b2f9d4c4dd
fetch "https://unpkg.com/react-dom@${REACT_DOM_VERSION}/umd/react-dom.production.min.js" \
  react-dom.production.min.js 35f4f974f4b2bcd44da73963347f8952e341f83909e4498227d4e26b98f66f0d

fetch "https://unpkg.com/graphiql@${GRAPHIQL_VERSION}/LICENSE" LICENSE.graphiql.txt \
  4b18fbe55c4e8f9169cb702be138f4d858a2dad920c39857204ffdc9c1c9fdc4
fetch "https://unpkg.com/react@${REACT_VERSION}/LICENSE" LICENSE.react.txt \
  52412d7bc7ce4157ea628bbaacb8829e0a9cb3c58f57f99176126bc8cf2bfc85
fetch "https://unpkg.com/react-dom@${REACT_DOM_VERSION}/LICENSE" LICENSE.react-dom.txt \
  52412d7bc7ce4157ea628bbaacb8829e0a9cb3c58f57f99176126bc8cf2bfc85
