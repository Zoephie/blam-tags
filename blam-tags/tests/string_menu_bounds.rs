//! Which enum/flags values have no name in their own layout's string list.
//!
//! Foundation crashed opening a converted `effect` with:
//!
//! ```text
//! at Corinth.Tags.TagFieldStringMenuItem.PopulateSubMenuItems(c_string_editor_definition* string_editor)
//! ### ASSERTION FAILED: at ...\blofeld\memory\array.h,#103
//!  index>=0 && index<array->count
//! ```
//!
//! It parses the tag, then dies *rendering the dropdown for a string field*.
//! `c_string_editor_definition` is the option-name list an enum/flags field
//! points at through `field.definition` -> `layout.string_lists`, and the
//! assertion is a lookup running off its end.
//!
//! This engine already computes that answer. `resolve_enum_name` returns
//! `None` for exactly the out-of-range case Foundation asserts on, and
//! `TagFieldData::{Char,Short,Long}Enum` carries it as `name: None`. So an
//! enum with no name *is* the crash, and it is visible offline.
//!
//! Three subjects per group, to separate the two possible causes:
//!
//! 1. `TagFile::new` from the definitions - a defect here is in the schema
//!    or the layout builder, present before any conversion happens.
//! 2. A Reach tag converted with no kit templates - the empty-kit path.
//! 3. A shipped H4 kit tag - the control that Foundation opens happily.

use blam_tags::convert::*;
use blam_tags::{TagFieldData, TagFile, TagStruct};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn kit(name: &str, folder: &str) -> Option<PathBuf> {
    let path = PathBuf::from("D:/SteamLibrary/steamapps/common")
        .join(name)
        .join(folder);
    path.is_dir().then_some(path)
}

/// The H4 editing kit's tag tree, wherever it currently lives. The `tags`
/// directory gets renamed aside to test the empty-kit path, so prefer
/// whichever of the two actually holds content.
fn h4_tags() -> Option<PathBuf> {
    ["tags", "tog"]
        .into_iter()
        .filter_map(|folder| kit("H4EK", folder))
        .find(|path| std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_some()))
}

/// `(field path, what is wrong)` for every enum/flags value the layout
/// cannot name.
fn unnamed(value: TagStruct<'_>, prefix: &str, out: &mut Vec<(String, String)>) {
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
        match field.value() {
            Some(TagFieldData::CharEnum { value, name: None }) => {
                out.push((path.clone(), format!("char enum = {value}")));
            }
            Some(TagFieldData::ShortEnum { value, name: None }) => {
                out.push((path.clone(), format!("short enum = {value}")));
            }
            Some(TagFieldData::LongEnum { value, name: None }) => {
                out.push((path.clone(), format!("long enum = {value}")));
            }
            // A set bit past the string list's `count` is dropped from
            // `names` rather than reported, so compare against the raw value.
            Some(TagFieldData::ByteFlags { value, names }) => {
                if value.count_ones() as usize != names.len() {
                    out.push((path.clone(), format!("byte flags = {value:#010b}")));
                }
            }
            Some(TagFieldData::WordFlags { value, names }) => {
                if value.count_ones() as usize != names.len() {
                    out.push((path.clone(), format!("word flags = {value:#018b}")));
                }
            }
            Some(TagFieldData::LongFlags { value, names }) => {
                if value.count_ones() as usize != names.len() {
                    out.push((path.clone(), format!("long flags = {value:#034b}")));
                }
            }
            _ => {}
        }
        if let Some(child) = field.as_struct() {
            unnamed(child, &path, out);
        }
        if let Some(block) = field.as_block() {
            for index in 0..block.len() {
                if let Some(element) = block.element(index) {
                    unnamed(element, &format!("{path}[]"), out);
                }
            }
        }
    }
}

/// Collapse to `path -> (tags affected, one example value)`.
fn tally(found: Vec<(String, String)>, into: &mut BTreeMap<String, (usize, String)>) {
    let mut seen = std::collections::BTreeSet::new();
    for (path, what) in found {
        if !seen.insert(path.clone()) {
            continue;
        }
        let entry = into.entry(path).or_insert((0, what));
        entry.0 += 1;
    }
}

fn report(label: &str, counts: &BTreeMap<String, (usize, String)>, tags: usize) {
    if counts.is_empty() {
        eprintln!("    {label}: clean across {tags} tag(s)");
        return;
    }
    let total: usize = counts.values().map(|(n, _)| *n).sum();
    eprintln!(
        "    {label}: {} unnamable field path(s), {total} occurrence(s) across {tags} tag(s)",
        counts.len()
    );
    let mut rows: Vec<_> = counts.iter().collect();
    rows.sort_by(|a, b| b.1.0.cmp(&a.1.0));
    for (path, (n, what)) in rows.iter().take(10) {
        eprintln!("        {n:>4}x {path}  ({what})");
    }
}

fn schema_for(definitions: &Path, game: &str, group: &str) -> PathBuf {
    definitions.join(game).join(format!("{group}.json"))
}

#[test]
#[ignore = "diagnostic"]
fn enum_values_their_own_layout_cannot_name() {
    let (Some(h4), Some(reach)) = (h4_tags(), kit("HREK", "tags")) else {
        eprintln!("skipping: needs H4EK and HREK");
        return;
    };
    let definitions = PathBuf::from("../../blam-tag-gui/definitions");
    if !definitions.is_dir() {
        eprintln!("skipping: no definitions tree beside the engine");
        return;
    }
    eprintln!("H4EK tags: {}", h4.display());

    let reach_files = walk_files(&reach);
    let h4_files = walk_files(&h4);

    for (group, fourcc) in [
        ("effect", "effe"),
        ("particle", "prt3"),
        ("tracer_system", "trsy"),
        // Controls: these two open in Foundation today.
        ("decal_system", "decs"),
        ("cheap_particle_emitter", "cpem"),
    ] {
        eprintln!("=== {group}");
        let Some(group_tag) = blam_tags::parse_group_tag(fourcc) else {
            eprintln!("    unknown four-cc");
            continue;
        };

        // 1. Straight out of the definitions, before any conversion.
        let schema = schema_for(&definitions, "halo4_mcc", group);
        if schema.is_file() {
            match std::panic::catch_unwind(|| TagFile::new(&schema)) {
                Ok(Ok(built)) => {
                    let mut found = Vec::new();
                    unnamed(built.root(), "", &mut found);
                    let mut counts = BTreeMap::new();
                    tally(found, &mut counts);
                    report("TagFile::new", &counts, 1);
                }
                Ok(Err(error)) => eprintln!("    TagFile::new: {error}"),
                Err(_) => eprintln!("    TagFile::new: panicked"),
            }
        } else {
            eprintln!("    TagFile::new: halo4_mcc has no {group}.json");
        }

        // 2. Converted from Reach with no kit templates - the empty-kit path.
        let mut converted = BTreeMap::new();
        let mut converted_tags = 0usize;
        for path in reach_files.iter() {
            if path.extension().and_then(|e| e.to_str()) != Some(group) {
                continue;
            }
            let Ok(source) =
                read_tag_for_conversion(path, Some("haloreach_mcc"), Some(&definitions), group_tag)
            else {
                continue;
            };
            let draft = match std::panic::catch_unwind(|| {
                analyze_conversion_with_templates(
                    &source,
                    "haloreach_mcc",
                    "halo4_mcc",
                    &definitions,
                    None,
                )
            }) {
                Ok(Ok(draft)) => draft,
                _ => continue,
            };
            converted_tags += 1;
            let mut found = Vec::new();
            unnamed(draft.tag.root(), "", &mut found);
            tally(found, &mut converted);
            if converted_tags >= 40 {
                break;
            }
        }
        report("converted (no kit)", &converted, converted_tags);

        // 3. Shipped H4 content - what Foundation opens happily.
        let mut shipped = BTreeMap::new();
        let mut shipped_tags = 0usize;
        for path in h4_files.iter() {
            if path.extension().and_then(|e| e.to_str()) != Some(group) {
                continue;
            }
            let Ok(tag) =
                read_tag_for_conversion(path, Some("halo4_mcc"), Some(&definitions), group_tag)
            else {
                continue;
            };
            shipped_tags += 1;
            let mut found = Vec::new();
            unnamed(tag.root(), "", &mut found);
            tally(found, &mut shipped);
            if shipped_tags >= 40 {
                break;
            }
        }
        report("shipped H4", &shipped, shipped_tags);
    }
}
