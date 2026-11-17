#!/bin/bash
mkdir -p /logs/verifier
fail() { echo 0 > /logs/verifier/reward.txt; exit 0; }
cd /app || fail
git apply --whitespace=nowarn /tests/test_patch.patch || fail
if node --test add.test.mjs > /dev/null 2>&1; then echo 1 > /logs/verifier/reward.txt; else echo 0 > /logs/verifier/reward.txt; fi
