#!/bin/sh
set -e
mkdir -p src
{
  i=0
  while [ $i -lt 1400 ]; do
    echo "# padding line $i, nothing to see here"
    i=$((i + 1))
  done
  echo "def compute_offset(index, stride):"
  echo '    """Byte offset of a record in the packed page."""'
  echo "    return 64 + index * stride"
  i=0
  while [ $i -lt 1400 ]; do
    echo "# more padding, line $i"
    i=$((i + 1))
  done
} > src/pages.py
cat > src/scheduler.py <<'EOF'
from pages import compute_offset


def schedule_window(records, stride):
    return [compute_offset(i, stride) for i, _ in enumerate(records)]
EOF
