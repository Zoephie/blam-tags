//! Which reference fields does shipped content never leave empty?
//!
//! A `cheap_particle_emitter` crashed Halo 4's tools until its
//! `global type library!` pointed at a real `cheap_particle_type_library`. That
//! is not a layout problem, it is a *required reference* problem, and it is
//! measurable: a reference field that every shipped tag of a group populates is
//! one the tools expect to resolve.

use blam_tags::convert::*;
use blam_tags::{TagFieldData, TagFile, TagStruct};
use std::collections::BTreeMap;
use std::path::PathBuf;

fn kit(name: &str, folder: &str) -> Option<PathBuf> {
    let path = PathBuf::from("D:/SteamLibrary/steamapps/common")
        .join(name)
        .join(folder);
    path.is_dir().then_some(path)
}

/// (path, filled) for every tag_reference in the tag, by field path.
fn references(value: TagStruct<'_>, prefix: &str, out: &mut Vec<(String, bool)>) {
    for field in value.fields() {
        let key = clean_field_key(field.name());
        let name = if key.is_empty() {
            field.type_name().to_owned()
        } else {
            key
        };
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        if let Some(TagFieldData::TagReference(reference)) = field.value() {
            let filled = reference
                .group_tag_and_name
                .as_ref()
                .is_some_and(|(_, tag_path)| !tag_path.is_empty());
            out.push((path.clone(), filled));
        }
        if let Some(child) = field.as_struct() {
            references(child, &path, out);
        }
        if let Some(block) = field.as_block() {
            for index in 0..block.len() {
                if let Some(element) = block.element(index) {
                    // Collapse the index: what matters is the field, not which
                    // element it was in.
                    references(element, &format!("{path}[]"), out);
                }
            }
        }
    }
}

#[test]
#[ignore = "diagnostic"]
fn what_shipped_content_never_leaves_empty() {
    let (Some(h4), Some(reach)) = (kit("H4EK", "tog"), kit("HREK", "tags")) else {
        eprintln!("skipping: needs H4EK (tags or tog) and HREK");
        return;
    };
    let definitions = PathBuf::from("../../blam-tag-gui/definitions");

    for (group, fourcc) in [
        ("effect", "effe"),
        ("particle", "prt3"),
        ("cheap_particle_emitter", "cpem"),
    ] {
        let Some(group_tag) = blam_tags::parse_group_tag(fourcc) else {
            eprintln!("=== {group}: unknown four-cc");
            continue;
        };
        // filled / total per field path across the shipped corpus.
        let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        let mut tags = 0usize;
        for path in walk_files(&h4) {
            if path.extension().and_then(|e| e.to_str()) != Some(group) {
                continue;
            }
            let Ok(tag) = read_tag_for_conversion(&path, Some("halo4_mcc"), Some(&definitions), group_tag)
            else {
                continue;
            };
            tags += 1;
            let mut found = Vec::new();
            references(tag.root(), "", &mut found);
            for (field, filled) in found {
                let entry = counts.entry(field).or_insert((0, 0));
                entry.1 += 1;
                if filled {
                    entry.0 += 1;
                }
            }
            if tags >= 300 {
                break;
            }
        }
        eprintln!("=== halo4_mcc {group}: {tags} shipped tags");
        // Always-filled fields are the candidates for "the tools expect this".
        let mut always: Vec<(&String, &(usize, usize))> = counts
            .iter()
            .filter(|(_, (filled, total))| *total > 0 && filled == total)
            .collect();
        always.sort_by(|a, b| b.1.1.cmp(&a.1.1));
        for (field, (filled, total)) in always.iter().take(8) {
            eprintln!("    ALWAYS {field} ({filled}/{total})");
        }

        // And what a conversion produces for the same fields, with no kit.
        let Some(source_path) = walk_files(&reach).into_iter().find(|path| {
            path.extension().and_then(|e| e.to_str()) == Some(group)
                && read_tag_for_conversion(path, Some("haloreach_mcc"), Some(&definitions), group_tag)
                    .is_ok()
        }) else {
            continue;
        };
        let source =
            read_tag_for_conversion(&source_path, Some("haloreach_mcc"), Some(&definitions), group_tag)
                .unwrap();
        let Ok(draft) = analyze_conversion_with_templates(
            &source,
            "haloreach_mcc",
            "halo4_mcc",
            &definitions,
            None,
        ) else {
            eprintln!("    (converted: refused)");
            continue;
        };
        let mut ours = Vec::new();
        references(draft.tag.root(), "", &mut ours);
        let ours: BTreeMap<String, bool> = ours.into_iter().collect();
        for (field, _) in always {
            match ours.get(field) {
                Some(true) => {}
                Some(false) => eprintln!("    -> converted leaves {field} EMPTY"),
                None => eprintln!("    -> converted has no {field} at all"),
            }
        }
    }
}

#[allow(unused_imports)]
use blam_tags::TagFile as _Unused;
