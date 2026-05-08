#!/usr/bin/env bash

set -euo pipefail

exec docker build -o type=local,dest=./debian_package --file debian_package.Dockerfile .
