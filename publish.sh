#!/bin/sh
# Republish the incident console to greptile.drums.sh.
# Run after a demo session so the public record shows what just happened.
set -e
cd "$(dirname "$0")"
node heal/export.js
cd site
vercel deploy --prod --yes --scope drums-92ba6b2c | grep -m1 "url"
echo "live: https://greptile.drums.sh"
