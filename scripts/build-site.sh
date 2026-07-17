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
fi

echo "✓ built with typewriter.js injected"
