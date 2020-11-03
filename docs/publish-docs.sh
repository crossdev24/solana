#!/usr/bin/env bash

set -e

if [[ -d .vercel ]]; then
  rm -r .vercel
fi

CONFIG_FILE=vercel.json

if [[ -n $CI_TAG ]]; then
  PROJECT_NAME=docs-solana-com
else
  eval "$(../ci/channel-info.sh)"
  case $CHANNEL in
  edge)
    PROJECT_NAME=edge-docs-solana-com
    ;;
  beta)
    PROJECT_NAME=beta-docs-solana-com
    ;;
  *)
    PROJECT_NAME=docs
    ;;
  esac
fi

cat > "$CONFIG_FILE" <<EOF
{
  "name": "$PROJECT_NAME",
  "scope": "solana-labs"
}
{
  "redirects": [
    { "source": "/apps", "destination": "/developing/programming-model/overview" },
    { "source": "/apps/bakcwards-compatibility/", "destination": "/developing/backwards-compatibility" },
    { "source": "/apps/break/", "destination": "/developing/deployed-programs/examples" },
    { "source": "/apps/builtins/", "destination": "/developing/builtin-programs" },
    { "source": "/apps/drones/", "destination": "/developing/deployed-programs/examples" },
    { "source": "/apps/hello-world/", "destination": "/developing/deployed-programs/examples" },
    { "source": "/apps/javascript-api/", "destination": "/developing/clients/javascript-api" },
    { "source": "/apps/jasonrpc-api/", "destination": "/developing/clients/jsonrpc-api" },
    { "source": "/apps/programming-faq/", "destination": "/developing/deployed-programs/faq" },
    { "source": "/apps/rent/", "destination": "/developing/programming-model/accounts" },
    { "source": "/apps/sysvars/", "destination": "/developing/programming-model/sysvars" },
    { "source": "/apps/webwallet/", "destination": "/developing/deployed-programs/examples" },
    { "source": "/implemented-proposals/cross-program-invocation", "destination": "/developing/programming-model/cross-program-invocations" },
    { "source": "/implemented-proposals/program-derived-addresses", "destination": "/developing/" },
    { "source": "/implemented-proposals/secp256k1_instruction", "destination": "/developing/" },
  ]
}
EOF

[[ -n $VERCEL_TOKEN ]] || {
  echo "VERCEL_TOKEN is undefined.  Needed for Vercel authentication."
  exit 1
}
vercel deploy . --local-config="$CONFIG_FILE" --confirm --token "$VERCEL_TOKEN" --prod
