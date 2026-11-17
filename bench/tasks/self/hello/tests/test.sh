#!/bin/bash
mkdir -p /logs/verifier
fail() { echo 0 > /logs/verifier/reward.txt; exit 0; }
cd /app || fail
if [ "$(cat hello.txt 2>/dev/null)" = "hello" ]; then echo 1 > /logs/verifier/reward.txt; else echo 0 > /logs/verifier/reward.txt; fi
