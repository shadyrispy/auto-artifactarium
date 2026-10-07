#!/usr/bin/env python3
"""Convert gitlab genshin-protocol 7.1.0 Deobfuscated.proto (single-file dump)
into a schema the auto-artifactarium build.rs can merge from protos_src/."""
import re
import sys

src = open(sys.argv[1], encoding="utf-8").read()
lines = src.split("\n")

out = []
i = 0
n = len(lines)

RELIQ_RENAME = {
    "_is_relic_starred": "starred",
    "_purchased_append_prop_id_list": "elixer_choices",
    "_definite_append_prop_id_list": "unactivated_prop_id_list",
}


def block_range(lines, start):
    """Return end index (exclusive) of the {}-balanced block starting at start."""
    depth = 0
    i = start
    seen = False
    while i < len(lines):
        depth += lines[i].count("{") - lines[i].count("}")
        if "{" in lines[i]:
            seen = True
        if seen and depth <= 0:
            return i + 1
        i += 1
    raise RuntimeError("unbalanced block")


# 1. Drop the header: import, `message YsCustom {...}`, and
#    `extend google.protobuf.FieldOptions {...}` (everything before the first
#    `// CmdId` comment).
first_cmd = next(k for k, l in enumerate(lines) if l.startswith("// CmdId"))
lines = lines[first_cmd:]

i = 0
while i < len(lines):
    l = lines[i]
    t = l.strip()

    # 2. Drop the dump's own incomplete PacketHead; build.rs appends the full one.
    if t.startswith("message PacketHead {") or t.startswith("message PacketHead{"):
        i = block_range(lines, i)
        continue

    # 3. Normalize `// CmdId: 27799 | MergeFrom: ...` -> `// CmdId: 27799`
    m = re.match(r"// CmdId:\s*(-?\d+)\b", t)
    if m and t.startswith("// CmdId"):
        if m.group(1) != "-":
            out.append(f"// CmdId: {m.group(1)}")
        i += 1
        continue

    # 4. Strip inline [(ys_custom)...] field options (single-line).
    if "(ys_custom)" in l:
        l = re.sub(r"\s*\[\(ys_custom\)[^\]]*\]", "", l)

    # 5. Align Reliquary client-side fields 6..8 with the exporter's names.
    for old, new in RELIQ_RENAME.items():
        l = re.sub(rf"\b{re.escape(old)}\b", new, l)

    out.append(l)
    i += 1

header = 'syntax = "proto3";\n'
sys.stdout.write(header + "\n".join(out))
