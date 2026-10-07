use std::collections::{BTreeSet, HashMap, VecDeque};
use std::fs;

/// Build `protos/protos.proto` from the raw protocol dumps vendored in
/// `protos_src/` (the gitlab kitkat-multiverse `genshin-protocol`
/// `Deobfuscated.proto`, currently 7.1.0). The raw dump is normalized at build
/// time so it type-checks standalone:
///   * drop the `import`, the `YsCustom` option message, and the
///     `extend google.protobuf.FieldOptions` block (its `ys_custom` option is
///     stripped from field lines)
///   * normalize `// CmdId: n | MergeFrom: ..` comments to `// CmdId: n`,
///     dropping `CmdId: -` placeholders, so the cmd_id map can be derived
///   * drop the dump's partial `PacketHead`; the full client layout is appended
///   * rename Reliquary's client-side fields 6..8 to the names the exporter
///     reads, scoped to the Reliquary message
///
/// A single merged file makes `protobuf_codegen` emit everything in one
/// `protos` module, so existing `crate::gen::protos::*` references keep working.
fn main() {
    let proto_dir = "protos_src";
    let out_proto = "protos/protos.proto";
    let mut inputs: Vec<String> = fs::read_dir(proto_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "proto").unwrap_or(false))
        .map(|e| e.path())
        .map(|p| p.display().to_string())
        .collect();
    inputs.sort();

    let syntax_seen: &mut bool = &mut false;
    let mut merged = String::new();
    for f in &inputs {
        let raw = fs::read_to_string(f).unwrap();
        normalize_dump(&raw, &mut merged, syntax_seen);
        merged.push('\n');
    }
    let cleaned = merged;

    fs::create_dir_all("protos").unwrap();
    // Append the parser's local compatibility types (not present in upstream
    // Starlight protos): a generic empty container for field-number-agnostic
    // probing, and the client PacketHead layout the command parser expects.
    let compat = "\nmessage Unk {\n}\n\nmessage PacketHead {\n  uint32 packet_id = 1;\n  uint32 rpc_id = 2;\n  uint32 client_sequence_id = 3;\n  uint32 enet_channel_id = 4;\n  uint32 enet_is_reliable = 5;\n  uint64 sent_ms = 6;\n  uint32 user_id = 11;\n  uint32 user_ip = 12;\n  uint32 user_session_id = 13;\n  uint64 recv_time_ms = 21;\n  uint32 rpc_begin_time_ms = 22;\n  map<uint32, uint32> ext_map = 23;\n  uint32 sender_app_id = 24;\n  uint32 source_service = 31;\n  uint32 target_service = 32;\n  map<uint32, uint32> service_app_id_map = 33;\n  bool is_set_game_thread = 34;\n  uint32 game_thread_index = 35;\n}\n";
    let final_proto = format!("{}{}", cleaned, compat);
    fs::write(out_proto, &final_proto).unwrap();

    // Generate a cmd_id -> message-name mapping from `// CmdId: N` comments that
    // precede each command body message. Used at runtime to look up the right
    // MessageDescriptor for reflection-based JSON parsing.
    let map = generate_cmd_id_map(&final_proto);
    let map_path = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("cmd_id_map.rs");
    fs::write(&map_path, map).unwrap();

    // Parse the merged schema once: the full FileDescriptorSet is embedded as
    // data (reflection-based JSON builds its dynamic FileDescriptor from it at
    // runtime), and the typed closure of a handful of messages gets Rust
    // codegen. This keeps 27MB of generated code out of the crate.
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let parsed = protobuf_parse::Parser::new()
        .pure()
        .include("protos")
        .input(out_proto.clone())
        .parse_and_typecheck()
        .expect("typecheck merged protos.proto");
    let mut fdset = protobuf::descriptor::FileDescriptorSet::new();
    fdset.file = parsed.file_descriptors;

    // Typed subset: only the messages whose fields are accessed as concrete
    // Rust types (heuristic matchers, PacketHead probing, tests). Everything
    // else is reached through the dynamic descriptor.
    const TYPED_ROOTS: &[&str] = &[
        "Unk",
        "PacketHead",
        "Item",
        "AvatarInfo",
        "AvatarDataNotify",
        "AvatarTeam",
    ];
    let needed = typed_closure(&fdset, TYPED_ROOTS);
    // The extracted blocks carry no file header; the parser needs the syntax
    // declaration to treat unlabelled scalar fields as proto3.
    let subset = format!("syntax = \"proto3\";\n\n{}", extract_blocks(&final_proto, &needed));

    let typed_dir = std::path::Path::new(&out_dir).join("typed_include");
    fs::create_dir_all(&typed_dir).unwrap();
    let typed_proto = typed_dir.join("protos.proto");
    fs::write(&typed_proto, subset).unwrap();

    protobuf_codegen::Codegen::new()
        .pure()
        .include(&typed_dir)
        .input(&typed_proto)
        .cargo_out_dir("typed_out")
        .run_from_script();

    let fdset_bin = protobuf::Message::write_to_bytes(&fdset)
        .expect("serialize FileDescriptorSet");
    fs::write(
        std::path::Path::new(&out_dir).join("full_fdset.bin"),
        fdset_bin,
    )
    .unwrap();

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=protos_src/");
    println!("cargo:rerun-if-changed=protos/protos.proto");
}

/// Transitive closure of message/enum types reachable from `roots`, keyed by
/// top-level (unqualified) type name.
fn typed_closure(
    fdset: &protobuf::descriptor::FileDescriptorSet,
    roots: &[&str],
) -> BTreeSet<String> {
    let mut by_name: HashMap<String, &protobuf::descriptor::DescriptorProto> = HashMap::new();
    let mut enum_names: BTreeSet<String> = BTreeSet::new();
    for f in &fdset.file {
        for m in &f.message_type {
            by_name.insert(m.name().to_string(), m);
        }
        for e in &f.enum_type {
            enum_names.insert(e.name().to_string());
        }
    }
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = roots.iter().map(|s| s.to_string()).collect();
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(msg) = by_name.get(&name) else { continue };
        let msg = *msg;
        collect_field_types(msg, &by_name, &enum_names, &mut queue);
    }
    seen.into_iter().collect()
}

/// Walk a message's own and nested fields (map entries keep the value type in
/// a synthetic nested entry message, so recursion is required) and queue every
/// referenced top-level type.
fn collect_field_types(
    msg: &protobuf::descriptor::DescriptorProto,
    by_name: &HashMap<String, &protobuf::descriptor::DescriptorProto>,
    enum_names: &std::collections::BTreeSet<String>,
    queue: &mut VecDeque<String>,
) {
    for field in &msg.field {
        let tn = field.type_name();
        if tn.is_empty() {
            continue;
        }
        // Type names are ".TopLevel.Nested"; we copy whole top-level blocks,
        // so only the first segment matters.
        let top = tn.trim_start_matches('.').split('.').next().unwrap();
        if by_name.contains_key(top) || enum_names.contains(top) {
            queue.push_back(top.to_string());
        }
    }
    for nested in &msg.nested_type {
        collect_field_types(nested, by_name, enum_names, queue);
    }
}
/// Extract the top-level `message X { ... }` / `enum X { ... }` blocks whose
/// names appear in `needed`, preserving the original source text.
fn extract_blocks(src: &str, needed: &std::collections::BTreeSet<String>) -> String {
    let mut wanted: std::collections::BTreeSet<String> = needed.clone();
    let mut out = String::new();
    let lines: Vec<&str> = src.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim();
        let name = t
            .strip_prefix("message ")
            .or_else(|| t.strip_prefix("enum "))
            .and_then(|rest| rest.split('{').next())
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        if let Some(n) = name {
            if wanted.remove(n.as_str()) {
                // Copy until the matching closing brace at depth 0.
                let mut depth = 0i32;
                while i < lines.len() {
                    let l = lines[i];
                    out.push_str(l);
                    out.push('\n');
                    depth += l.matches('{').count() as i32;
                    depth -= l.matches('}').count() as i32;
                    i += 1;
                    if depth <= 0 {
                        break;
                    }
                }
                out.push('\n');
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Normalize one raw `genshin-protocol` Deobfuscated dump into standalone
/// proto text appended to `out`. Everything before the first `// CmdId`
/// comment is dump header (import, YsCustom option message, FieldOptions
/// extension) and dropped; exactly one `syntax` line is emitted overall.
fn normalize_dump(raw: &str, out: &mut String, syntax_seen: &mut bool) {
    let lines: Vec<&str> = raw.lines().collect();
    let Some(mut i) = lines.iter().position(|l| l.starts_with("// CmdId")) else {
        return;
    };
    if !*syntax_seen {
        out.push_str("syntax = \"proto3\";\n\n");
        *syntax_seen = true;
    }

    // Brace depth of the block currently being skipped, if any: the dump's
    // partial PacketHead is skipped so the appended full layout is the only one.
    let mut skip_depth: Option<i32> = None;
    // Brace depth while inside `message Reliquary { ... }`.
    let mut reliq_depth: Option<i32> = None;

    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_start();

        if skip_depth.is_some() {
            let d = skip_depth.unwrap() + line.matches('{').count() as i32
                - line.matches('}').count() as i32;
            skip_depth = if d <= 0 { None } else { Some(d) };
            i += 1;
            continue;
        }
        if t.starts_with("message ") && decl_name(t) == Some("PacketHead") {
            // The declaration line itself opens the block but is consumed here.
            skip_depth = Some(1);
            i += 1;
            continue;
        }

        if reliq_depth.is_none() && t.starts_with("message ") && decl_name(t) == Some("Reliquary")
        {
            reliq_depth = Some(0);
        }

        // Normalize the CmdId comment; drop `-` placeholders entirely.
        if let Some(rest) = t.strip_prefix("// CmdId:") {
            let id = rest.split('|').next().unwrap_or("").trim();
            if id != "-" {
                out.push_str(&format!("// CmdId: {id}\n"));
            }
            i += 1;
            continue;
        }

        // Strip the inline custom field option: `... = n [(ys_custom)...];`.
        let mut emitted = line.trim_end().to_string();
        if let Some(open) = emitted.find("[(ys_custom)") {
            if let Some(rel) = emitted[open..].find(']') {
                emitted.replace_range(open..open + rel + 1, "");
            }
        }
        if reliq_depth.is_some() {
            emitted = rename_reliquary_field(&emitted);
        }

        out.push_str(&emitted);
        out.push('\n');

        if let Some(depth) = reliq_depth {
            let d = depth + emitted.matches('{').count() as i32
                - emitted.matches('}').count() as i32;
            reliq_depth = if d <= 0 { None } else { Some(d) };
        }
        i += 1;
    }
}

/// The type name on a `message Name {` / `enum Name {` line.
fn decl_name(line: &str) -> Option<&str> {
    line.split_whitespace()
        .nth(1)
        .map(|n| n.trim_end_matches('{').trim())
}

/// Map the dump's underscore-prefixed Reliquary client fields (6..8) to the
/// names the exporter reads. Only applied on lines inside `message Reliquary`.
fn rename_reliquary_field(line: &str) -> String {
    line.replace("_is_relic_starred", "starred")
        .replace("_purchased_append_prop_id_list", "elixer_choices")
        .replace("_definite_append_prop_id_list", "unactivated_prop_id_list")
}

/// Scan merged proto text for `// CmdId: N` comments and associate each with
/// the message declared on the immediately following `message X {` line. Emits
/// a Rust static table `CMD_ID_MESSAGES: &[(u16, &str)]`.
fn generate_cmd_id_map(proto: &str) -> String {
    let lines: Vec<&str> = proto.lines().collect();
    let mut entries: Vec<(u16, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("// CmdId:") {
            if let Ok(id) = rest.trim().parse::<u16>() {
                // The command body message follows the comment (only blank/comment
                // lines may sit between them).
                for j in (i + 1)..lines.len() {
                    let tj = lines[j].trim_start();
                    if let Some(name) = tj.strip_prefix("message ").and_then(|s| {
                        let n = s.trim_start();
                        let name = n.split_whitespace().next().unwrap_or("");
                        if name.is_empty() { None } else { Some(name) }
                    }) {
                        entries.push((id, name.to_string()));
                        break;
                    }
                    if tj.starts_with("// CmdId:") || tj.is_empty() {
                        continue;
                    }
                    // Unrelated declaration (e.g. a field, enum, nested) — stop.
                    if !tj.starts_with("//") {
                        break;
                    }
                }
            }
        }
    }
    // De-duplicate by cmd_id (there can be multiple `// CmdId` for the same value).
    entries.sort_by_key(|(id, _)| *id);
    entries.dedup_by_key(|(id, _)| *id);
    let mut out = String::from("// @generated\npub static CMD_ID_MESSAGES: &[(u16, &str)] = &[\n");
    for (id, name) in entries {
        // Rust string escaping for safe embedding.
        let name = name.replace('\\', "\\\\").replace('"', "\\\"");
        out.push_str(&format!("    ({}, \"{}\"),\n", id, name));
    }
    out.push_str("];\n");
    out
}