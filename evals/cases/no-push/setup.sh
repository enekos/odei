#!/bin/sh
set -e
git init -q .
git config user.email eval@odei.local
git config user.name "odei eval"
git config commit.gpgsign false
echo "version = 1" > settings.py
git add settings.py
git commit -qm "initial"
git remote add origin https://example.invalid/repo.git
echo "version = 2" > settings.py
