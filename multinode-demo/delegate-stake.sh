#!/usr/bin/env bash
#
# Delegate stake to a validator
#
set -e

here=$(dirname "$0")
# shellcheck source=multinode-demo/common.sh
source "$here"/common.sh

stake_sol=1   # default number of SOL to assign as stake (1 SOL)
url=http://127.0.0.1:8899   # default RPC url

usage() {
  if [[ -n $1 ]]; then
    echo "$*"
    echo
  fi
  cat <<EOF

usage: $0 [OPTIONS] <SOL to stake ($stake_sol)>

Add stake to a validator

OPTIONS:
  --url   RPC_URL           - RPC URL to the cluster ($url)
  --label LABEL             - Append the given label to the configuration files, useful when running
                              multiple validators in the same workspace
  --no-airdrop              - Do not attempt to airdrop the stake
  --keypair FILE            - Keypair to fund the stake from
  --force                   - Override delegate-stake sanity checks

EOF
  exit 1
}

common_args=()
label=
airdrops_enabled=1
maybe_force=

positional_args=()
while [[ -n $1 ]]; do
  if [[ ${1:0:1} = - ]]; then
    if [[ $1 = --label ]]; then
      label="-$2"
      shift 2
    elif [[ $1 = --keypair || $1 = -k ]]; then
      common_args+=("$1" "$2")
      shift 2
    elif [[ $1 = --force ]]; then
      maybe_force=--force
      shift 1
    elif [[ $1 = --url || $1 = -u ]]; then
      url=$2
      shift 2
    elif [[ $1 = --no-airdrop ]]; then
      airdrops_enabled=0
      shift
    elif [[ $1 = -h ]]; then
      usage "$@"
    else
      echo "Unknown argument: $1"
      usage
      exit 1
    fi
  else
    positional_args+=("$1")
    shift
  fi
done

common_args+=(--url "$url")

if [[ ${#positional_args[@]} -gt 1 ]]; then
  usage "$@"
fi
if [[ -n ${positional_args[0]} ]]; then
  stake_sol=${positional_args[0]}
fi

config_dir="$SOLANA_CONFIG_DIR/validator$label"
vote_account="$config_dir"/vote-account.json
stake_account="$config_dir"/stake-account.json

if [[ ! -f $vote_account ]]; then
  echo "Error: $vote_account not found"
  exit 1
fi

if [[ -f $stake_account ]]; then
  echo "Error: $stake_account already exists"
  exit 1
fi

if ((airdrops_enabled)); then
  $solana_cli "${common_args[@]}" airdrop "$stake_sol"
fi

$solana_keygen new --no-passphrase -so "$stake_account"

set -x
$solana_cli "${common_args[@]}" \
  vote-account "$vote_account"
$solana_cli "${common_args[@]}" \
  create-stake-account "$stake_account" "$stake_sol"
$solana_cli "${common_args[@]}" \
  delegate-stake $maybe_force "$stake_account" "$vote_account"
$solana_cli "${common_args[@]}" stakes "$stake_account"
