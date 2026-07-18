#!/usr/bin/env bash
set -euo pipefail

elf=${1:-target/thumbv6m-none-eabi/release/looptic}
uf2=${2:-looptic.uf2}
storage_xip_start=$((0x10600000))

if [[ ! -f "$elf" ]]; then
    echo "missing firmware ELF: $elf" >&2
    exit 1
fi

while read -r physical file_size; do
    size=$((file_size))
    (( size == 0 )) && continue
    start=$((physical))
    end=$((start + size))
    if (( end > storage_xip_start )); then
        printf 'ELF load image reaches song storage: 0x%08x..0x%08x\n' "$start" "$end" >&2
        exit 1
    fi
done < <(readelf -lW "$elf" | awk '$1 == "LOAD" { print $4, $5 }')

if [[ ! -f "$uf2" ]]; then
    echo "missing firmware UF2: $uf2" >&2
    exit 1
fi

perl -e '
    use strict;
    use warnings;
    my ($path, $limit) = @ARGV;
    open my $fh, "<:raw", $path or die "open $path: $!\n";
    my $blocks = 0;
    while (read($fh, my $block, 512)) {
        length($block) == 512 or die "truncated UF2 block $blocks\n";
        my @header = unpack("V8", substr($block, 0, 32));
        $header[0] == 0x0a324655 && $header[1] == 0x9e5d5157
            or die "invalid UF2 header in block $blocks\n";
        unpack("V", substr($block, 508, 4)) == 0x0ab16f30
            or die "invalid UF2 trailer in block $blocks\n";
        my ($target, $payload) = @header[3, 4];
        $payload <= 476 or die "oversized UF2 payload in block $blocks\n";
        my $end = $target + $payload;
        $end <= $limit
            or die sprintf("UF2 block %u reaches song storage: 0x%08x..0x%08x\n",
                $blocks, $target, $end);
        $blocks++;
    }
    $blocks > 0 or die "UF2 contains no blocks\n";
    print "verified $blocks UF2 blocks below 0x10600000\n";
' "$uf2" "$storage_xip_start"

echo "verified all ELF load images below 0x10600000"
