#!/bin/bash

function timestamp() {
  date +%Y.%m.%d-%H.%M.%S
}

function addtimestamp() {
  while IFS= read -r line; do
    printf '%s %s\n' "$(date +%Y.%m.%d-%H.%M.%S)" "$line";
  done
}

function log2() {
  echo $1 | awk '{print log($1)/log(2)}'
}

function sqrt() {
  echo "$1" | awk '{print sqrt($1)}'
}

function print_var() {
  echo "$1=${!1}"
}

function to_fullpath() {
  readlink -f "$1"
}

function this_file() {
  readlink -f "$0"
}

function this_file_name() {
  basename -- "$(this_file)"
}

function this_directory() {
  dirname "$(to_fullpath "$0")"
}

function this_directory_name() {
  basename -- "$(this_directory)"
}

function remove_ext() {
  echo "${1%.*}"
}

function determine_queue() {
  local nnodes=$1
  if [[ $nnodes -lt 32 ]]; then
    echo "gen_S"
  elif [[ $nnodes -lt 64 ]]; then
    echo "gen_M"
  else
    echo "gen_L"
  fi
}

time_json() {
    FORMAT=$(cat <<EOF
{
  "Command": "%C",
  "UnsharedDataSize(KB)": %D,
  "ElapsedTime": "%E",
  "MajorPageFaults": %F,
  "FileSystemInputs": %I,
  "TotalMemory(KB)": %K,
  "MaxResidentSetSize(KB)": %M,
  "FileSystemOutputs": %O,
  "CpuUsage": "%P",
  "MinorPageFaults": %R,
  "SystemCpuSeconds": %S,
  "UserCpuSeconds": %U,
  "Swaps": %W,
  "SharedTextSize(KB)": %X,
  "PageSize(Bytes)": %Z,
  "InvoluntaryContextSwitches": %c,
  "ElapsedTime(Seconds)": %e,
  "SignalsDelivered": %k,
  "UnsharedStackSize(KB)": %p,
  "SocketMessagesReceived": %r,
  "SocketMessagesSent": %s,
  "AverageResidentSetSize(KB)": %t,
  "VoluntaryContextSwitches": %w,
  "ExitStatus": %x
}
EOF
)
    /usr/bin/time -f "$FORMAT" "$@"
}
