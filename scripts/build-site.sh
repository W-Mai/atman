#!/usr/bin/env bash
set -euo pipefail

oranda build

INDEX="public/index.html"
if [ -f "$INDEX" ]; then
  if ! grep -q 'typewriter.js' "$INDEX"; then
    if [[ "$(uname)" == "Darwin" ]]; then
      sed -i '' 's|</body>|<script src="/static/typewriter.js"></script>\
</body>|' "$INDEX"
    else
      sed -i 's|</body>|<script src="/static/typewriter.js"></script>\n</body>|' "$INDEX"
    fi
  fi
  if [[ "$(uname)" == "Darwin" ]]; then
    sed -i '' 's|href="/favicon.ico"|href="/static/ATMAN-LOGO.png"|g' "$INDEX"
  else
    sed -i 's|href="/favicon.ico"|href="/static/ATMAN-LOGO.png"|g' "$INDEX"
  fi
fi

echo "✓ built with typewriter.js injected + atman favicon"
