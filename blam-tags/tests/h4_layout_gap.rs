//! Where a Halo 4 layout built from the definitions differs from the layout a
//! shipped Halo 4 tag carries in its own `blay`.
//!
//! ManagedBlam refuses a `TagFile::new`-built `particle` with
//! `cannot read tag struct 'particle_struct_definition', invalid chunk tag
//! ('' should be 'tgst')` - it walked the *root* struct and ran out of body
//! before it ran out of expected sub-chunks. A field that needs a sub-chunk and
//! is missing from our field list is exactly that, so this prints the two field
//! lists side by side and marks which entries carry a sub-chunk.

use blam_tags::convert::clean_field_key;
use blam_tags::{TagFile, TagStruct};
use std::path::PathBuf;

fn h4_tags() -> Option<PathBuf> {
    ["tog", "tags"]
        .into_iter()
        .map(|folder| PathBuf::from("D:/SteamLibrary/steamapps/common/H4EK").join(folder))
        .find(|path| std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_some()))
}

fn walk(root: &std::path::Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                pending.push(entry.path());
            } else if kind.is_file() {
                out.push(entry.path());
            }
        }
    }
    out
}

/// `(name, type, carries a sub-chunk)` for every field, in order.
fn shape(value: TagStruct<'_>) -> Vec<(String, String, bool)> {
    value
        .fields()
        .map(|field| {
            let key = clean_field_key(field.name());
            let name = if key.is_empty() {
                format!("<{}>", field.type_name())
            } else {
                key
            };
            let sub_chunk = field.as_block().is_some()
                || field.as_struct().is_some()
                || matches!(
                    field.value(),
                    Some(blam_tags::TagFieldData::Data(_))
                        | Some(blam_tags::TagFieldData::TagReference(_))
                );
            (name, format!("{:?}", field.field_type()), sub_chunk)
        })
        .collect()
}

fn dump(label: &str, rows: &[(String, String, bool)]) {
    eprintln!("    --- {label}: {} field(s) ---", rows.len());
    for (index, (name, ty, sub)) in rows.iter().enumerate() {
        eprintln!(
            "      {index:>3} {} {name}  ({ty})",
            if *sub { "*" } else { " " }
        );
    }
}

/// Compare what we build against a tag the editing kit itself authored from
/// nothing (`TagFile.New` + `Save` through ManagedBlam), which is the closest
/// thing to a specification for "what a from-definitions tag must look like".
///
/// Point `BLAM_PROBE_OUT` at the directory holding `new_<group>.<group>` (ours)
/// and `kitnew_<group>.<group>` (the kit's).
#[test]
#[ignore = "diagnostic"]
fn built_tag_against_a_tag_the_kit_authored() {
    let Some(dir) = std::env::var_os("BLAM_PROBE_OUT").map(PathBuf::from) else {
        eprintln!("skipping: set BLAM_PROBE_OUT");
        return;
    };
    for group in ["particle", "tracer_system", "effect", "decal_system"] {
        eprintln!("=== {group}");
        let ours = dir.join(format!("new_{group}.{group}"));
        let theirs = dir.join(format!("kitnew_{group}.{group}"));
        let (Ok(ours), Ok(theirs)) = (TagFile::read(&ours), TagFile::read(&theirs)) else {
            eprintln!("    one side missing or unreadable");
            continue;
        };
        let mut differences = Vec::new();
        compare(ours.root(), theirs.root(), "root", &mut differences);
        eprintln!("    {} difference(s)", differences.len());
        for line in differences.iter().take(24) {
            eprintln!("      {line}");
        }
    }
}

/// Does the fold agree with Halo Reach too, or only Halo 4?
///
/// The fold fires wherever a `tmpl` target's root struct is declared short, and
/// Reach's `particle` is one of those. Reach ships that group at several layout
/// revisions, so this reports every shipped size rather than picking one.
#[test]
#[ignore = "diagnostic"]
fn reach_particle_shader_struct_against_shipped() {
    let reach = PathBuf::from("D:/SteamLibrary/steamapps/common/HREK/tags");
    if !reach.is_dir() {
        eprintln!("skipping: needs HREK");
        return;
    }
    let definitions = PathBuf::from("../../blam-tag-gui/definitions");
    let schema = definitions.join("haloreach_mcc/particle.json");
    let Ok(ours) = TagFile::new(&schema) else {
        eprintln!("skipping: cannot build a Reach particle");
        return;
    };
    let describe = |tag: &TagFile| {
        let root = tag.root().definition().size();
        let shader = tag
            .root()
            .fields()
            .find(|f| clean_field_key(f.name()).starts_with("actual shader"))
            .and_then(|f| f.as_struct())
            .map(|s| (s.definition().size(), s.fields().count()));
        (root, shader)
    };
    eprintln!("ours: {:?}", describe(&ours));
    let mut seen = std::collections::BTreeMap::new();
    for path in walk(&reach) {
        if path.extension().and_then(|e| e.to_str()) != Some("particle") {
            continue;
        }
        if let Ok(tag) = TagFile::read(&path) {
            *seen.entry(format!("{:?}", describe(&tag))).or_insert(0usize) += 1;
        }
        if seen.values().sum::<usize>() >= 400 {
            break;
        }
    }
    for (shape, count) in seen {
        eprintln!("  shipped {shape}  x{count}");
    }
}

/// Compare any two tag files structurally. `BLAM_CMP_A` is the suspect,
/// `BLAM_CMP_B` the reference; both are absolute paths.
#[test]
#[ignore = "diagnostic"]
fn compare_two_tags() {
    let (Some(a), Some(b)) = (
        std::env::var_os("BLAM_CMP_A").map(PathBuf::from),
        std::env::var_os("BLAM_CMP_B").map(PathBuf::from),
    ) else {
        eprintln!("skipping: set BLAM_CMP_A and BLAM_CMP_B");
        return;
    };
    let (ours, theirs) = match (TagFile::read(&a), TagFile::read(&b)) {
        (Ok(x), Ok(y)) => (x, y),
        (x, y) => {
            eprintln!("    unreadable: a={:?} b={:?}", x.err(), y.err());
            return;
        }
    };
    let mut differences = Vec::new();
    compare(ours.root(), theirs.root(), "root", &mut differences);
    eprintln!(
        "{} vs {}: {} difference(s)",
        a.file_name().unwrap().to_string_lossy(),
        b.file_name().unwrap().to_string_lossy(),
        differences.len()
    );
    for line in differences.iter().take(30) {
        eprintln!("    {line}");
    }
}

/// Structural comparison that descends struct fields and the first element of
/// any block both sides populate.
fn compare(ours: TagStruct<'_>, theirs: TagStruct<'_>, path: &str, out: &mut Vec<String>) {
    let a = shape(ours);
    let b = shape(theirs);
    if ours.definition().size() != theirs.definition().size() || a.len() != b.len() {
        out.push(format!(
            "{path}: ours {}B/{} fields, kit {}B/{} fields",
            ours.definition().size(),
            a.len(),
            theirs.definition().size(),
            b.len()
        ));
    }
    if ours.definition().name() != theirs.definition().name() {
        out.push(format!(
            "{path}: struct name ours {:?} kit {:?}",
            ours.definition().name(),
            theirs.definition().name()
        ));
    }
    if ours.definition().version() != theirs.definition().version() {
        out.push(format!(
            "{path}: struct version ours {} kit {}",
            ours.definition().version(),
            theirs.definition().version()
        ));
    }
    for index in 0..a.len().max(b.len()) {
        match (a.get(index), b.get(index)) {
            (Some(x), Some(y)) if x != y => {
                out.push(format!("{path}[{index}]: ours {x:?}, kit {y:?}"))
            }
            (None, Some(y)) => out.push(format!("{path}[{index}]: missing, kit {y:?}")),
            (Some(x), None) => out.push(format!("{path}[{index}]: ours {x:?}, kit has none")),
            _ => {}
        }
    }
    for field in theirs.fields() {
        let key = clean_field_key(field.name());
        if let Some(their_child) = field.as_struct() {
            let Some(our_child) = ours
                .fields()
                .find(|candidate| clean_field_key(candidate.name()) == key)
                .and_then(|candidate| candidate.as_struct())
            else {
                out.push(format!("{path}/{key}: struct absent from ours"));
                continue;
            };
            compare(our_child, their_child, &format!("{path}/{key}"), out);
        }
        if let Some(their_block) = field.as_block() {
            let our_block = ours
                .fields()
                .find(|candidate| clean_field_key(candidate.name()) == key)
                .and_then(|candidate| candidate.as_block());
            let Some(our_block) = our_block else {
                out.push(format!("{path}/{key}: block absent from ours"));
                continue;
            };
            if our_block.len() != their_block.len() {
                out.push(format!(
                    "{path}/{key}: ours {} element(s), kit {}",
                    our_block.len(),
                    their_block.len()
                ));
            }
            if let (Some(x), Some(y)) = (our_block.element(0), their_block.element(0)) {
                compare(x, y, &format!("{path}/{key}[0]"), out);
            }
        }
    }
}

#[test]
#[ignore = "diagnostic"]
fn built_h4_root_against_shipped_h4_root() {
    let Some(h4) = h4_tags() else {
        eprintln!("skipping: needs H4EK");
        return;
    };
    let definitions = PathBuf::from("../../blam-tag-gui/definitions");
    if !definitions.is_dir() {
        eprintln!("skipping: no definitions tree");
        return;
    }
    let files = walk(&h4);

    for group in ["particle", "tracer_system", "decal_system", "effect"] {
        eprintln!("=== halo4_mcc {group}");
        let schema = definitions.join("halo4_mcc").join(format!("{group}.json"));
        let ours = match std::panic::catch_unwind(|| TagFile::new(&schema)) {
            Ok(Ok(tag)) => tag,
            Ok(Err(error)) => {
                eprintln!("    ours: {error}");
                continue;
            }
            Err(_) => {
                eprintln!("    ours: panicked");
                continue;
            }
        };
        // Compare against a shipped tag whose root size matches what we declare,
        // so this is not comparing two different layout revisions.
        let declared = ours.root().definition().size();
        let mut chosen = None;
        for path in files.iter() {
            if path.extension().and_then(|e| e.to_str()) != Some(group) {
                continue;
            }
            if let Ok(tag) = TagFile::read(path) {
                if tag.root().definition().size() == declared {
                    chosen = Some((path.clone(), tag));
                    break;
                }
            }
        }
        let Some((path, theirs)) = chosen else {
            eprintln!("    no shipped {group} declares a {declared}-byte root");
            continue;
        };
        let a = shape(ours.root());
        let b = shape(theirs.root());
        eprintln!(
            "    ours {declared}B/{} fields ({} with sub-chunk); kit {}B/{} fields ({} with sub-chunk)  [{}]",
            a.len(),
            a.iter().filter(|f| f.2).count(),
            theirs.root().definition().size(),
            b.len(),
            b.iter().filter(|f| f.2).count(),
            path.file_name().unwrap().to_string_lossy(),
        );
        if a == b {
            eprintln!("    root field lists identical");
        } else {
            dump("ours", &a);
            dump("kit", &b);
        }
    }
}
