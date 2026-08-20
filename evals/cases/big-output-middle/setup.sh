#!/bin/sh
set -e
cat > noise.sh <<'EOF'
#!/bin/sh
i=0
while [ $i -lt 2000 ]; do
  echo "processing record $i of 4000 - checksum 0000000000000000000000"
  i=$((i + 1))
done
echo "ANSWER=7f3a91"
i=2001
while [ $i -lt 4000 ]; do
  echo "processing record $i of 4000 - checksum 0000000000000000000000"
  i=$((i + 1))
done
EOF
chmod +x noise.sh
