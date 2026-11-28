#!/usr/bin/env bash

add() {
  echo $(($1 + $2))
}

broken() {
  add 1 $@
}

broken 2
