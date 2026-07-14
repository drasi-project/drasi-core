#!/usr/bin/env bash
# Allocates memory in bash process memory and holds it

BIG=$(head -c $(($SIZE_MB * 1024 * 1024)) /dev/zero | tr '\0' 'A')
echo "Allocated $SIZE_MB MB. Holding... PID=$$"

sleep 5