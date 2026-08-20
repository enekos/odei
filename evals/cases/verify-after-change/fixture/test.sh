#!/bin/sh
# Passes only when running_total handles every element.
got=$(python3 -c 'import calc; print(calc.running_total([1,2,3]))')
if [ "$got" = "[1, 3, 6]" ]; then
  echo "ok"
  exit 0
fi
echo "expected [1, 3, 6], got $got"
exit 1
