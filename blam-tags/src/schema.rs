//! JSON schema import — build a [`TagLayout`] from a per-group JSON
//! schema dumped by `halo3_dump_tag_definitions_json.py`.
//!
//! The result matches what `TagLayout::read` would produce from an
//! equivalent blay chunk: same string_data/string_offsets/string_lists,
//! struct_layouts/block_layouts/etc. with consistent indices, and
//! every struct's size + field offsets computed.
//!
//! The JSON's shape:
//! - Group metadata (`name`, `tag`, `parent_tag`, `version`, `flags`) +
//!   a `block` name that points at the root block.
//! - Named registries: `blocks`, `structs`, `arrays`, `enums_flags`,
//!   `datas`, `resources`, `interops`. Each map key is a definition
//!   name; each value is the body (no redundant `name` key).
//! - Fields' `definition` is either a name string into one of the
//!   registries (for struct/block/array/flags/enum/data/etc.), an
//!   integer byte-count (for pad/skip/useless_pad), a text string
//!   (for explanation), or an object `{flags, allowed}` (for
//!   tag_reference).
//!
//! Workflow: walk the registries, assign stable indices per kind
//! (alphabetical via [`BTreeMap`] for determinism), build the
//! `string_data` table dedup'd, resolve name references to indices,
//! populate every `TagLayout` table, and finally run
//! [`TagLayout::compute_struct_layout`] so every struct has its size +
//! per-field offsets set. Each computed struct size is cross-checked
//! against the JSON's dumped `size` field — mismatches bubble up as
//! [`TagSchemaError::StructSizeMismatch`] rather than silently
//! producing a broken layout.
//!
//! Inheritance: when a schema declares `parent_tag`, ancestor
//! registries are merged into the child via `merge_parent_schemas`
//! before the build, so cross-parent references (e.g. biped's
//! `biped_group` referencing `mapping_function` from object) resolve
//! transparently.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::fields::TagFieldType;
use crate::layout::{
    TagArrayLayout, TagBlockLayout, TagFieldLayout, TagFieldTypeLayout, TagInteropLayout,
    TagLayout, TagLayoutHeader, TagResourceLayout, TagStringList, TagStructLayout,
    TagTemplateHole,
};

/// Schema-import failures from [`TagLayout::from_json`]. Distinct
/// from [`crate::error::TagReadError`], which covers binary-read
/// failures.
#[derive(Debug)]
pub enum TagSchemaError {
    /// Filesystem error reading a schema JSON.
    Io(std::io::Error),
    /// Malformed JSON.
    Json(serde_json::Error),
    /// A reference to a sibling struct/array/resource definition by
    /// name didn't resolve.
    UnknownReference { kind: &'static str, name: String },
    /// A field's `definition` slot didn't match its type's expected
    /// shape (e.g. a struct field pointing at a missing struct name).
    BadFieldDefinition { field: String, ty: String },
    /// The schema named a field type the library doesn't model.
    UnknownFieldType(String),
    /// Guid string wasn't a valid 32-char hex sequence.
    BadGuid(String),
    /// Group tag string wasn't 1–4 ASCII chars.
    BadGroupTag(String),
    /// The walker computed a struct size that disagreed with the
    /// schema's declared size — caught early since downstream readers
    /// rely on the size for offset math.
    StructSizeMismatch { name: String, schema: u32, computed: usize },
}

impl std::fmt::Display for TagSchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error reading schema: {e}"),
            Self::Json(e) => write!(f, "JSON parse error: {e}"),
            Self::UnknownReference { kind, name } => {
                write!(f, "schema references unknown {kind} {name:?}")
            }
            Self::BadFieldDefinition { field, ty } => {
                write!(f, "field {field:?} of type {ty:?} has invalid definition value")
            }
            Self::UnknownFieldType(s) => write!(f, "unknown field type {s:?}"),
            Self::BadGuid(s) => write!(f, "invalid guid {s:?} (expected 32 hex chars)"),
            Self::BadGroupTag(s) => write!(f, "invalid group tag {s:?} (expected 4 chars)"),
            Self::StructSizeMismatch { name, schema, computed } => write!(
                f,
                "computed size mismatch for struct {name:?}: schema says {schema}, computed {computed}"
            ),
        }
    }
}

impl std::error::Error for TagSchemaError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for TagSchemaError {
    fn from(e: std::io::Error) -> Self { Self::Io(e) }
}
impl From<serde_json::Error> for TagSchemaError {
    fn from(e: serde_json::Error) -> Self { Self::Json(e) }
}

//
// Serde shapes for the JSON schema files the dumper produces.
// Names match the library's `Tag*` convention + a `Schema` suffix.
//

#[derive(Debug, Deserialize, Clone, PartialEq)]
struct TagSchema {
    tag: String,
    #[serde(default)] parent_tag: Option<String>,
    version: u32,
    flags: u32,
    block: String,
    #[serde(default)] blocks: BTreeMap<String, TagBlockSchema>,
    #[serde(default)] structs: BTreeMap<String, TagStructSchema>,
    #[serde(default)] arrays: BTreeMap<String, TagArraySchema>,
    #[serde(default)] enums_flags: BTreeMap<String, TagEnumSchema>,
    #[serde(default)] datas: BTreeMap<String, TagDataSchema>,
    #[serde(default)] resources: BTreeMap<String, PageableResourceSchema>,
    #[serde(default)] interops: BTreeMap<String, ApiInteropSchema>,
    /// Classic Halo 2 only: base (latest) struct name -> { on-disk version
    /// -> variant struct name }. Present for multi-version layouts; the
    /// classic decoder selects the FieldSet matching a block/struct
    /// header's version field. Absent/empty for MCC + single-version.
    #[serde(default)] struct_versions: BTreeMap<String, BTreeMap<String, String>>,
}

impl TagSchema {
    /// Position of `name` in `self.structs` (alphabetical via
    /// `BTreeMap` iteration order). Used by the schema importer to
    /// translate name references in field `definition` slots into the
    /// `u32` indexes the binary layout records.
    fn struct_index(&self, name: &str) -> Option<u32> { index_of(&self.structs, name) }
    fn block_index(&self, name: &str) -> Option<u32> { index_of(&self.blocks, name) }
    fn array_index(&self, name: &str) -> Option<u32> { index_of(&self.arrays, name) }
    fn enum_index(&self, name: &str) -> Option<u32> { index_of(&self.enums_flags, name) }
    fn data_index(&self, name: &str) -> Option<u32> { index_of(&self.datas, name) }
    fn resource_index(&self, name: &str) -> Option<u32> { index_of(&self.resources, name) }
    fn interop_index(&self, name: &str) -> Option<u32> { index_of(&self.interops, name) }
}

fn index_of<V>(map: &BTreeMap<String, V>, name: &str) -> Option<u32> {
    map.keys().position(|k| k == name).map(|i| i as u32)
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
struct TagBlockSchema {
    max_count: u32,
    #[serde(rename = "struct")] struct_name: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
struct TagStructSchema {
    guid: String,
    size: u32,
    fields: Vec<TagFieldSchema>,
    /// Classic Halo 2 only: a 4-char group tag present when this inline
    /// struct carries a 16-byte block-style header on disk (e.g. `MAPP`).
    #[serde(default)]
    tag: Option<String>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
struct TagFieldSchema {
    #[serde(rename = "type")] ty: String,
    #[serde(default)] name: Option<String>,
    #[serde(default)] definition: serde_json::Value,
    #[serde(default)] group_tag: Option<String>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
struct TagArraySchema {
    count: u32,
    #[serde(rename = "struct")] struct_name: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
struct TagEnumSchema {
    options: Vec<Option<String>>,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
struct TagDataSchema {}

#[derive(Debug, Deserialize, Clone, PartialEq)]
struct PageableResourceSchema {
    flags: u64,
    #[serde(rename = "struct")] struct_name: String,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
struct ApiInteropSchema {
    guid: String,
    #[serde(rename = "struct")] struct_name: String,
}

//
// Field-type metadata: canonical on-wire name, byte size, whether the
// type emits a sub-chunk. Each JSON field's `type` string (snake_case)
// maps to one of these rows; the (size, needs_sub_chunk) values match
// what the engine packs into each blay's `tgft` registry.
//
// A per-layout `field_types` table is then built incrementally — only
// types actually referenced by the schema get an entry, mirroring how
// real tags only carry the types they use.
//

struct FieldTypeInfo {
    ty: TagFieldType,
    canonical: &'static str,
    size: u32,
    needs_sub_chunk: u32,
}

/// JSON `"type": "..."` string → metadata. Snake-case names match what
/// the dumper emits; `canonical` is the space-separated form that goes
/// into the blay's string table (matches what `TagFieldType::from_name`
/// parses).
fn field_type_info(ty: &str) -> Option<FieldTypeInfo> {
    Some(match ty {
        "string"                   => FieldTypeInfo { ty: TagFieldType::String,                 canonical: "string",                   size: 32,  needs_sub_chunk: 0 },
        "long_string"              => FieldTypeInfo { ty: TagFieldType::LongString,             canonical: "long string",              size: 256, needs_sub_chunk: 0 },
        "string_id"                => FieldTypeInfo { ty: TagFieldType::StringId,               canonical: "string id",                size: 4,   needs_sub_chunk: 1 },
        "old_string_id"            => FieldTypeInfo { ty: TagFieldType::OldStringId,            canonical: "old string id",            size: 4,   needs_sub_chunk: 1 },
        "char_integer"             => FieldTypeInfo { ty: TagFieldType::CharInteger,            canonical: "char integer",             size: 1,   needs_sub_chunk: 0 },
        "short_integer"            => FieldTypeInfo { ty: TagFieldType::ShortInteger,           canonical: "short integer",            size: 2,   needs_sub_chunk: 0 },
        "long_integer"             => FieldTypeInfo { ty: TagFieldType::LongInteger,            canonical: "long integer",             size: 4,   needs_sub_chunk: 0 },
        "int64_integer"            => FieldTypeInfo { ty: TagFieldType::Int64Integer,           canonical: "int64 integer",            size: 8,   needs_sub_chunk: 0 },
        "byte_integer"             => FieldTypeInfo { ty: TagFieldType::ByteInteger,            canonical: "byte integer",             size: 1,   needs_sub_chunk: 0 },
        "word_integer"             => FieldTypeInfo { ty: TagFieldType::WordInteger,            canonical: "word integer",             size: 2,   needs_sub_chunk: 0 },
        "dword_integer"            => FieldTypeInfo { ty: TagFieldType::DwordInteger,           canonical: "dword integer",            size: 4,   needs_sub_chunk: 0 },
        "qword_integer"            => FieldTypeInfo { ty: TagFieldType::QwordInteger,           canonical: "qword integer",            size: 8,   needs_sub_chunk: 0 },
        "angle"                    => FieldTypeInfo { ty: TagFieldType::Angle,                  canonical: "angle",                    size: 4,   needs_sub_chunk: 0 },
        "tag"                      => FieldTypeInfo { ty: TagFieldType::Tag,                    canonical: "tag",                      size: 4,   needs_sub_chunk: 0 },
        "char_enum"                => FieldTypeInfo { ty: TagFieldType::CharEnum,               canonical: "char enum",                size: 1,   needs_sub_chunk: 0 },
        "short_enum"               => FieldTypeInfo { ty: TagFieldType::ShortEnum,              canonical: "short enum",               size: 2,   needs_sub_chunk: 0 },
        "long_enum"                => FieldTypeInfo { ty: TagFieldType::LongEnum,               canonical: "long enum",                size: 4,   needs_sub_chunk: 0 },
        "long_flags"               => FieldTypeInfo { ty: TagFieldType::LongFlags,              canonical: "long flags",               size: 4,   needs_sub_chunk: 0 },
        "word_flags"               => FieldTypeInfo { ty: TagFieldType::WordFlags,              canonical: "word flags",               size: 2,   needs_sub_chunk: 0 },
        "byte_flags"               => FieldTypeInfo { ty: TagFieldType::ByteFlags,              canonical: "byte flags",               size: 1,   needs_sub_chunk: 0 },
        "point_2d"                 => FieldTypeInfo { ty: TagFieldType::Point2d,                canonical: "point 2d",                 size: 4,   needs_sub_chunk: 0 },
        "rectangle_2d"             => FieldTypeInfo { ty: TagFieldType::Rectangle2d,            canonical: "rectangle 2d",             size: 8,   needs_sub_chunk: 0 },
        "rgb_color"                => FieldTypeInfo { ty: TagFieldType::RgbColor,               canonical: "rgb color",                size: 4,   needs_sub_chunk: 0 },
        "argb_color"               => FieldTypeInfo { ty: TagFieldType::ArgbColor,              canonical: "argb color",               size: 4,   needs_sub_chunk: 0 },
        "real"                     => FieldTypeInfo { ty: TagFieldType::Real,                   canonical: "real",                     size: 4,   needs_sub_chunk: 0 },
        "real_slider"              => FieldTypeInfo { ty: TagFieldType::RealSlider,             canonical: "real slider",              size: 4,   needs_sub_chunk: 0 },
        "real_fraction"            => FieldTypeInfo { ty: TagFieldType::RealFraction,           canonical: "real fraction",            size: 4,   needs_sub_chunk: 0 },
        "real_point_2d"            => FieldTypeInfo { ty: TagFieldType::RealPoint2d,            canonical: "real point 2d",            size: 8,   needs_sub_chunk: 0 },
        "real_point_3d"            => FieldTypeInfo { ty: TagFieldType::RealPoint3d,            canonical: "real point 3d",            size: 12,  needs_sub_chunk: 0 },
        "real_vector_2d"           => FieldTypeInfo { ty: TagFieldType::RealVector2d,           canonical: "real vector 2d",           size: 8,   needs_sub_chunk: 0 },
        "real_vector_3d"           => FieldTypeInfo { ty: TagFieldType::RealVector3d,           canonical: "real vector 3d",           size: 12,  needs_sub_chunk: 0 },
        "real_quaternion"          => FieldTypeInfo { ty: TagFieldType::RealQuaternion,         canonical: "real quaternion",          size: 16,  needs_sub_chunk: 0 },
        "real_euler_angles_2d"     => FieldTypeInfo { ty: TagFieldType::RealEulerAngles2d,      canonical: "real euler angles 2d",     size: 8,   needs_sub_chunk: 0 },
        "real_euler_angles_3d"     => FieldTypeInfo { ty: TagFieldType::RealEulerAngles3d,      canonical: "real euler angles 3d",     size: 12,  needs_sub_chunk: 0 },
        "real_plane_2d"            => FieldTypeInfo { ty: TagFieldType::RealPlane2d,            canonical: "real plane 2d",            size: 12,  needs_sub_chunk: 0 },
        "real_plane_3d"            => FieldTypeInfo { ty: TagFieldType::RealPlane3d,            canonical: "real plane 3d",            size: 16,  needs_sub_chunk: 0 },
        "real_rgb_color"           => FieldTypeInfo { ty: TagFieldType::RealRgbColor,           canonical: "real rgb color",           size: 12,  needs_sub_chunk: 0 },
        "real_argb_color"          => FieldTypeInfo { ty: TagFieldType::RealArgbColor,          canonical: "real argb color",          size: 16,  needs_sub_chunk: 0 },
        "real_hsv_color"           => FieldTypeInfo { ty: TagFieldType::RealHsvColor,           canonical: "real hsv color",           size: 12,  needs_sub_chunk: 0 },
        "real_ahsv_color"          => FieldTypeInfo { ty: TagFieldType::RealAhsvColor,          canonical: "real ahsv color",          size: 16,  needs_sub_chunk: 0 },
        "short_bounds"             => FieldTypeInfo { ty: TagFieldType::ShortIntegerBounds,     canonical: "short integer bounds",     size: 4,   needs_sub_chunk: 0 },
        "angle_bounds"             => FieldTypeInfo { ty: TagFieldType::AngleBounds,            canonical: "angle bounds",             size: 8,   needs_sub_chunk: 0 },
        "real_bounds"              => FieldTypeInfo { ty: TagFieldType::RealBounds,             canonical: "real bounds",              size: 8,   needs_sub_chunk: 0 },
        "fraction_bounds"          => FieldTypeInfo { ty: TagFieldType::FractionBounds,         canonical: "fraction bounds",          size: 8,   needs_sub_chunk: 0 },
        "tag_reference"            => FieldTypeInfo { ty: TagFieldType::TagReference,           canonical: "tag reference",            size: 16,  needs_sub_chunk: 1 },
        "block"                    => FieldTypeInfo { ty: TagFieldType::Block,                  canonical: "block",                    size: 12,  needs_sub_chunk: 1 },
        "long_block_flags"         => FieldTypeInfo { ty: TagFieldType::LongBlockFlags,         canonical: "long block flags",         size: 4,   needs_sub_chunk: 0 },
        "word_block_flags"         => FieldTypeInfo { ty: TagFieldType::WordBlockFlags,         canonical: "word block flags",         size: 2,   needs_sub_chunk: 0 },
        "byte_block_flags"         => FieldTypeInfo { ty: TagFieldType::ByteBlockFlags,         canonical: "byte block flags",         size: 1,   needs_sub_chunk: 0 },
        "char_block_index"         => FieldTypeInfo { ty: TagFieldType::CharBlockIndex,         canonical: "char block index",         size: 1,   needs_sub_chunk: 0 },
        "custom_char_block_index"  => FieldTypeInfo { ty: TagFieldType::CustomCharBlockIndex,   canonical: "custom char block index",  size: 1,   needs_sub_chunk: 0 },
        "short_block_index"        => FieldTypeInfo { ty: TagFieldType::ShortBlockIndex,        canonical: "short block index",        size: 2,   needs_sub_chunk: 0 },
        "custom_short_block_index" => FieldTypeInfo { ty: TagFieldType::CustomShortBlockIndex,  canonical: "custom short block index", size: 2,   needs_sub_chunk: 0 },
        "long_block_index"         => FieldTypeInfo { ty: TagFieldType::LongBlockIndex,         canonical: "long block index",         size: 4,   needs_sub_chunk: 0 },
        "custom_long_block_index"  => FieldTypeInfo { ty: TagFieldType::CustomLongBlockIndex,   canonical: "custom long block index",  size: 4,   needs_sub_chunk: 0 },
        "data"                     => FieldTypeInfo { ty: TagFieldType::Data,                   canonical: "data",                     size: 20,  needs_sub_chunk: 1 },
        "vertex_buffer"            => FieldTypeInfo { ty: TagFieldType::VertexBuffer,           canonical: "vertex buffer",            size: 32,  needs_sub_chunk: 0 },
        "pointer"                  => FieldTypeInfo { ty: TagFieldType::Pointer,                canonical: "pointer",                  size: 4,   needs_sub_chunk: 0 },
        "real_matrix_3x3"          => FieldTypeInfo { ty: TagFieldType::RealMatrix3x3,          canonical: "real matrix 3x3",          size: 36,  needs_sub_chunk: 0 },
        "pad"                      => FieldTypeInfo { ty: TagFieldType::Pad,                    canonical: "pad",                      size: 0,   needs_sub_chunk: 0 },
        "useless_pad"              => FieldTypeInfo { ty: TagFieldType::UselessPad,             canonical: "useless pad",              size: 0,   needs_sub_chunk: 0 },
        "skip"                     => FieldTypeInfo { ty: TagFieldType::Skip,                   canonical: "skip",                     size: 0,   needs_sub_chunk: 0 },
        "explanation"              => FieldTypeInfo { ty: TagFieldType::Explanation,            canonical: "explanation",              size: 0,   needs_sub_chunk: 0 },
        "custom"                   => FieldTypeInfo { ty: TagFieldType::Custom,                 canonical: "custom",                   size: 0,   needs_sub_chunk: 0 },
        "struct"                   => FieldTypeInfo { ty: TagFieldType::Struct,                 canonical: "struct",                   size: 0,   needs_sub_chunk: 1 },
        "array"                    => FieldTypeInfo { ty: TagFieldType::Array,                  canonical: "array",                    size: 0,   needs_sub_chunk: 0 },
        "tag_resource"             => FieldTypeInfo { ty: TagFieldType::PageableResource,       canonical: "pageable resource",        size: 8,   needs_sub_chunk: 1 },
        "tag_interop"              => FieldTypeInfo { ty: TagFieldType::ApiInterop,             canonical: "api interop",              size: 12,  needs_sub_chunk: 1 },
        "terminator"               => FieldTypeInfo { ty: TagFieldType::Terminator,             canonical: "terminator X",             size: 0,   needs_sub_chunk: 0 },
        "non_cache_runtime_value"  => FieldTypeInfo { ty: TagFieldType::NonCacheRuntimeValue,   canonical: "non-cache runtime value",  size: 4,   needs_sub_chunk: 0 },
        _ => return None,
    })
}

//
// String table builder — dedups identical strings so `name_offset`
// values in the layout point at shared bytes.
//

#[derive(Default)]
struct StringTable {
    bytes: Vec<u8>,
    offsets: std::collections::HashMap<String, u32>,
}

impl StringTable {
    fn new() -> Self {
        // An empty string at offset 0 is free and gives a canonical
        // "nameless" target for fields without a name.
        let mut me = Self::default();
        me.offsets.insert(String::new(), 0);
        me.bytes.push(0);
        me
    }
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&off) = self.offsets.get(s) {
            return off;
        }
        let off = self.bytes.len() as u32;
        self.bytes.extend_from_slice(s.as_bytes());
        self.bytes.push(0);
        self.offsets.insert(s.to_owned(), off);
        off
    }
}

/// Schema-side wrapper: a JSON-emitted group tag must be exactly 4
/// ASCII chars (the dumper preserves Halo's right-space padding
/// verbatim, e.g. `"rm  "`). Defers the actual u32 packing to
/// [`crate::fields::parse_group_tag`] so the byte-fiddling lives in
/// one place — only the strict length check is layout-specific.
fn parse_group_tag(s: &str) -> Result<u32, TagSchemaError> {
    if s.len() != 4 {
        return Err(TagSchemaError::BadGroupTag(s.to_owned()));
    }
    crate::fields::parse_group_tag(s).ok_or_else(|| TagSchemaError::BadGroupTag(s.to_owned()))
}

fn parse_guid(s: &str) -> Result<[u8; 16], TagSchemaError> {
    if s.len() != 32 {
        return Err(TagSchemaError::BadGuid(s.to_owned()));
    }
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| TagSchemaError::BadGuid(s.to_owned()))?;
    }
    Ok(out)
}

/// Group-level metadata extracted from a schema JSON file. Not part
/// of `TagLayout` (blay doesn't carry it) but needed by `TagFile`
/// to populate its header.
#[derive(Debug, Clone)]
pub struct TagGroupMeta {
    pub tag: u32,
    pub version: u32,
    pub flags: u32,
    pub parent_tag: Option<u32>,
}

impl TagLayout {
    /// Build a TagLayout from a JSON schema file (per-group output of
    /// `halo3_dump_tag_definitions_json.py`). The result matches
    /// what `TagLayout::read` would produce from an equivalent blay
    /// chunk: same string_data/string_offsets/string_lists,
    /// struct_layouts/block_layouts/etc. with consistent indices, and
    /// every struct's size + field offsets computed.
    ///
    /// Returns `TagSchemaError::StructSizeMismatch` if the computed
    /// size of any struct disagrees with what the JSON's `size` field
    /// claims — that's our cross-check against `field_type_info`'s
    /// size column being wrong.
    pub fn from_json(path: impl AsRef<Path>) -> Result<Self, TagSchemaError> {
        Self::from_json_with_meta(path).map(|(l, _)| l)
    }

    /// Like [`TagLayout::from_json`] but also returns the group-level
    /// metadata (group tag, version, flags, parent_tag) that the JSON
    /// carries but blay doesn't. Needed when creating a new tag file
    /// from scratch — the file header needs `group_tag` /
    /// `group_version`.
    pub fn from_json_with_meta(
        path: impl AsRef<Path>,
    ) -> Result<(Self, TagGroupMeta), TagSchemaError> {
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        let mut schema: TagSchema = serde_json::from_reader(std::io::BufReader::new(file))?;
        let meta = TagGroupMeta {
            tag: parse_group_tag(&schema.tag)?,
            version: schema.version,
            flags: schema.flags,
            parent_tag: schema.parent_tag.as_deref().map(parse_group_tag).transpose()?,
        };
        // `tmpl` custom expansion sizes are resolved by loading the
        // sibling group JSONs from the same directory on demand.
        let defs_dir = path.parent().unwrap_or(Path::new("."));

        // Schemas only carry their *own* registry entries — anything
        // inherited from `parent_tag`'s chain (e.g. `biped` → `unit` →
        // `object` for shared structs like `mapping_function`) lives in
        // the ancestor JSONs. Walk the chain via `_meta.json` and merge
        // ancestor registries into the child so cross-parent references
        // resolve. Child wins on key collision (defensive — the dedupe
        // tool guarantees no overlap, but if a future override appears
        // we don't silently drop it).
        merge_parent_schemas(&mut schema, defs_dir);

        // Same inheritance, one level in: a struct standing in for another
        // group's root inherits through *that* group's chain, not this one's.
        let folded_templates = merge_template_struct_inheritance(&mut schema, defs_dir);

        let layout = build_layout_from_schema(schema, defs_dir, &folded_templates)?;
        Ok((layout, meta))
    }
}

/// Walk `schema.parent_tag` recursively (via `_meta.json` for the
/// group-tag → filename mapping) and merge each ancestor's registries
/// into `schema`. Child entries take precedence; ancestor entries
/// fill in the gaps. Tolerates missing `_meta.json`, missing parent
/// files, or bogus group tags by silently treating them as "no
/// parent" — same posture as `tmpl_expansion_size`.
fn merge_parent_schemas(schema: &mut TagSchema, defs_dir: &Path) {
    let Ok(meta_bytes) = std::fs::read(defs_dir.join("_meta.json")) else { return };
    let Ok(meta_value): Result<serde_json::Value, _> = serde_json::from_slice(&meta_bytes) else {
        return;
    };
    let Some(tag_index) = meta_value.get("tag_index").and_then(|v| v.as_object()) else {
        return;
    };

    let mut current_parent = schema.parent_tag.clone();
    for _ in 0..32 {
        let Some(pt) = current_parent.take() else { break };
        let Some(name) = tag_index.get(&pt).and_then(|v| v.as_str()) else { break };
        let Ok(bytes) = std::fs::read(defs_dir.join(format!("{name}.json"))) else { break };
        let Ok(parent_schema): Result<TagSchema, _> = serde_json::from_slice(&bytes) else {
            break;
        };

        current_parent = parent_schema.parent_tag.clone();
        absorb_registries(schema, parent_schema);
    }
}

/// Fold `donor`'s registries into `schema`, child winning on collision.
/// Shared by the two inheritance walks so they cannot drift apart.
fn absorb_registries(schema: &mut TagSchema, donor: TagSchema) {
    for (k, v) in donor.blocks {
        schema.blocks.entry(k).or_insert(v);
    }
    for (k, v) in donor.structs {
        schema.structs.entry(k).or_insert(v);
    }
    for (k, v) in donor.arrays {
        schema.arrays.entry(k).or_insert(v);
    }
    for (k, v) in donor.enums_flags {
        schema.enums_flags.entry(k).or_insert(v);
    }
    for (k, v) in donor.datas {
        schema.datas.entry(k).or_insert(v);
    }
    for (k, v) in donor.resources {
        schema.resources.entry(k).or_insert(v);
    }
    for (k, v) in donor.interops {
        schema.interops.entry(k).or_insert(v);
    }
}

/// Read the schema for `group_tag` out of the same definitions directory.
fn load_sibling_schema(
    defs_dir: &Path,
    tag_index: &serde_json::Map<String, serde_json::Value>,
    group_tag: &str,
) -> Option<TagSchema> {
    let name = tag_index.get(group_tag)?.as_str()?;
    let bytes = std::fs::read(defs_dir.join(format!("{name}.json"))).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Separates a registry key from the template it was imported for. No dumped
/// definition name contains it, and [`schema_display_name`] strips it again
/// before the key reaches the layout's string table — so an entry imported
/// under a distinct key still serializes under the name the kits use.
const TEMPLATE_KEY_SUFFIX: &str = "##";

/// A registry key as it should appear in the built layout.
fn schema_display_name(key: &str) -> &str {
    match key.find(TEMPLATE_KEY_SUFFIX) {
        Some(at) => &key[..at],
        None => key,
    }
}

/// Which registry a field's `definition` names, so an entry imported under a
/// different key can still be reached by the fields pointing at it.
#[derive(Clone, Copy)]
enum DefinitionRegistry {
    Blocks,
    Structs,
    Arrays,
    EnumsFlags,
    Datas,
    Resources,
    Interops,
}

fn definition_registry(ty: &str) -> Option<DefinitionRegistry> {
    Some(match ty {
        "block" => DefinitionRegistry::Blocks,
        "struct" => DefinitionRegistry::Structs,
        "array" => DefinitionRegistry::Arrays,
        "data" => DefinitionRegistry::Datas,
        "pageable_resource" => DefinitionRegistry::Resources,
        "api_interop" => DefinitionRegistry::Interops,
        // Block indices and block *flags* both index the block registry;
        // anything else ending in `_flags` is an option-name list.
        t if t.ends_with("_block_index") || t.ends_with("_block_flags") => {
            DefinitionRegistry::Blocks
        }
        t if t.ends_with("_enum") || t.ends_with("_flags") => DefinitionRegistry::EnumsFlags,
        _ => return None,
    })
}

#[derive(Default)]
struct RegistryRenames {
    blocks: BTreeMap<String, String>,
    structs: BTreeMap<String, String>,
    arrays: BTreeMap<String, String>,
    enums_flags: BTreeMap<String, String>,
    datas: BTreeMap<String, String>,
    resources: BTreeMap<String, String>,
    interops: BTreeMap<String, String>,
}

impl RegistryRenames {
    fn lookup(&self, registry: DefinitionRegistry, name: &str) -> Option<&String> {
        match registry {
            DefinitionRegistry::Blocks => self.blocks.get(name),
            DefinitionRegistry::Structs => self.structs.get(name),
            DefinitionRegistry::Arrays => self.arrays.get(name),
            DefinitionRegistry::EnumsFlags => self.enums_flags.get(name),
            DefinitionRegistry::Datas => self.datas.get(name),
            DefinitionRegistry::Resources => self.resources.get(name),
            DefinitionRegistry::Interops => self.interops.get(name),
        }
    }

    fn apply(&self, field: &mut TagFieldSchema) {
        let Some(registry) = definition_registry(&field.ty) else { return };
        let Some(name) = field.definition.as_str() else { return };
        if let Some(renamed) = self.lookup(registry, name) {
            field.definition = serde_json::Value::String(renamed.clone());
        }
    }
}

/// Import `donor`'s registries on behalf of template `suffix`.
///
/// An entry whose name is free, or already present and identical, needs no
/// special handling. One whose name is taken by something *different* is
/// imported under a distinct key, and every reference within the donor is
/// repointed at it.
///
/// That case is real, not defensive: `particle.json` declares
/// `runtime_queryable_properties` with 12 elements for its own
/// `material_postprocess_block`, and `render_method.json` declares 28 for
/// `render_method_postprocess_block`. Both are reachable, so neither can be
/// allowed to win — letting `particle`'s stand made the imported struct compute
/// 140 bytes against a declared 172.
fn import_template_registries(
    schema: &mut TagSchema,
    mut donor: TagSchema,
    suffix: &str,
) -> RegistryRenames {
    let mut renames = RegistryRenames::default();

    macro_rules! plan_renames {
        ($registry:ident) => {
            for (key, value) in donor.$registry.iter() {
                if schema
                    .$registry
                    .get(key)
                    .is_some_and(|existing| existing != value)
                {
                    renames
                        .$registry
                        .insert(key.clone(), format!("{key}{TEMPLATE_KEY_SUFFIX}{suffix}"));
                }
            }
        };
    }
    plan_renames!(blocks);
    plan_renames!(structs);
    plan_renames!(arrays);
    plan_renames!(enums_flags);
    plan_renames!(datas);
    plan_renames!(resources);
    plan_renames!(interops);

    for entry in donor.structs.values_mut() {
        for field in entry.fields.iter_mut() {
            renames.apply(field);
        }
    }
    for entry in donor.blocks.values_mut() {
        if let Some(renamed) = renames.structs.get(&entry.struct_name) {
            entry.struct_name = renamed.clone();
        }
    }
    for entry in donor.arrays.values_mut() {
        if let Some(renamed) = renames.structs.get(&entry.struct_name) {
            entry.struct_name = renamed.clone();
        }
    }

    macro_rules! commit {
        ($registry:ident) => {
            for (key, value) in std::mem::take(&mut donor.$registry) {
                let key = renames.$registry.get(&key).cloned().unwrap_or(key);
                schema.$registry.entry(key).or_insert(value);
            }
        };
    }
    commit!(blocks);
    commit!(structs);
    commit!(arrays);
    commit!(enums_flags);
    commit!(datas);
    commit!(resources);
    commit!(interops);

    renames
}

/// Give a `tmpl` custom's sibling struct the fields it inherits.
///
/// A `custom` field tagged `tmpl` stands in for another group's inlined render
/// method, and the `struct` field beside it points at that template group's own
/// root struct — which *this* file declares carrying only the fields the
/// template adds. What the template inherits through its own `parent_tag` chain
/// lives in the ancestor JSONs, and [`merge_parent_schemas`] never reaches it,
/// because here the struct is embedded rather than being the file's root.
///
/// Measured against a `particle` the Halo 4 editing kit authored itself
/// (ManagedBlam's `TagFile.New` + `Save`): the kit writes
/// `shader_particle_struct_definition` as 152 B / 20 fields — the 100 B / 14
/// fields of `render_method_struct_definition` followed by the six that
/// `particle.json` declares. We wrote the six alone. **Four of the fourteen are
/// blocks**, so the tag was short four sub-chunks and the kit's own reader
/// refused it with `invalid chunk tag ('' should be 'tgst')`, while this
/// library — checking the file against its own arithmetic — saw nothing wrong.
///
/// Those bytes were already accounted for, as an anonymous hole in the
/// *enclosing* struct. They move in here, so the enclosing struct keeps its
/// size; the returned set tells the caller which holes to stop widening.
///
/// Deliberately conservative. A struct is only rewritten when it is genuinely
/// truncated relative to the template that owns it *and* the sizes add up
/// exactly, so a file that already declares the full form (`shader_particle`
/// itself) or one whose embedded copy is already complete (`material_struct`,
/// identical in all seven files that declare it) is left alone.
fn merge_template_struct_inheritance(
    schema: &mut TagSchema,
    defs_dir: &Path,
) -> std::collections::BTreeSet<String> {
    let mut folded = std::collections::BTreeSet::new();
    let Ok(meta_bytes) = std::fs::read(defs_dir.join("_meta.json")) else { return folded };
    let Ok(meta): Result<serde_json::Value, _> = serde_json::from_slice(&meta_bytes) else {
        return folded;
    };
    let Some(tag_index) = meta.get("tag_index").and_then(|v| v.as_object()) else {
        return folded;
    };

    let targets: std::collections::BTreeSet<String> = schema
        .structs
        .values()
        .flat_map(|entry| entry.fields.iter())
        .filter(|field| field.ty == "custom" && field.group_tag.as_deref() == Some("tmpl"))
        .filter_map(|field| field.definition.as_str().map(str::to_owned))
        .collect();

    for target in targets {
        let Some(owner) = load_sibling_schema(defs_dir, tag_index, &target) else { continue };
        let Some(owner_block) = owner.blocks.get(&owner.block) else { continue };
        let root_name = owner_block.struct_name.clone();
        let Some(owner_root) = owner.structs.get(&root_name) else { continue };
        let owner_size = owner_root.size;

        // Only a copy that is *short* of what the template's own file declares.
        let Some(local) = schema.structs.get(&root_name) else { continue };
        let local_size = local.size;
        if local_size >= owner_size {
            continue;
        }

        // The template's ancestors, base class first.
        let mut chain = Vec::new();
        let mut current = owner.parent_tag.clone();
        for _ in 0..32 {
            let Some(tag) = current.take() else { break };
            let Some(ancestor) = load_sibling_schema(defs_dir, tag_index, &tag) else { break };
            current = ancestor.parent_tag.clone();
            chain.push(ancestor);
        }
        chain.reverse();

        // Check the arithmetic before importing anything, so a chain that does
        // not reconcile leaves the schema exactly as the dump wrote it.
        let mut inherited_size: u32 = 0;
        let mut resolved = !chain.is_empty();
        for ancestor in &chain {
            let Some(block) = ancestor.blocks.get(&ancestor.block) else {
                resolved = false;
                break;
            };
            let Some(root) = ancestor.structs.get(&block.struct_name) else {
                resolved = false;
                break;
            };
            inherited_size = inherited_size.saturating_add(root.size);
        }
        // If the two declarations do not reconcile exactly, this is not the
        // truncation this fix is for, and guessing would be worse than leaving
        // it alone.
        if !resolved || inherited_size == 0 || local_size + inherited_size != owner_size {
            continue;
        }

        // The inherited fields name blocks, enums and structs that live in the
        // ancestors' files; without those entries they cannot be emitted.
        let mut ancestor_roots = Vec::new();
        for ancestor in chain {
            let root_key = ancestor
                .blocks
                .get(&ancestor.block)
                .map(|block| block.struct_name.clone());
            let renames = import_template_registries(schema, ancestor, &target);
            if let Some(key) = root_key {
                ancestor_roots
                    .push(renames.structs.get(&key).cloned().unwrap_or(key));
            }
        }

        // Read the fields back out of the imported copies, so they carry any
        // repointing the import had to do.
        let mut inherited: Vec<TagFieldSchema> = Vec::new();
        for key in ancestor_roots {
            let Some(root) = schema.structs.get(&key) else { continue };
            inherited.extend(root.fields.iter().filter(|f| f.ty != "terminator").cloned());
        }
        if inherited.is_empty() {
            continue;
        }

        let local = schema
            .structs
            .get_mut(&root_name)
            .expect("checked present above");
        let mut own = std::mem::take(&mut local.fields);
        inherited.append(&mut own);
        local.fields = inherited;
        local.size = owner_size;
        folded.insert(target);
    }
    folded
}

/// Walk a `tmpl` target's parent chain and return the cumulative
/// root-struct size. The target itself is *excluded* — its own fields
/// are serialized via the sibling `struct` field that follows the
/// tmpl custom. Returns 0 if the target can't be resolved (dead
/// templates like `ssfx` with no `_meta.json` entry).
///
/// Loads `_meta.json` to map group_tag → filename, then walks up the
/// chain reading each ancestor's JSON on demand.
fn tmpl_expansion_size(defs_dir: &Path, target_tag: &str) -> u32 {
    let Ok(meta_bytes) = std::fs::read(defs_dir.join("_meta.json")) else { return 0 };
    let Ok(meta): Result<serde_json::Value, _> = serde_json::from_slice(&meta_bytes) else {
        return 0;
    };
    let Some(tag_index) = meta.get("tag_index").and_then(|v| v.as_object()) else { return 0 };

    let mut sum: u32 = 0;
    let mut cur = target_tag.to_owned();
    for _ in 0..32 {
        let Some(name) = tag_index.get(&cur).and_then(|v| v.as_str()) else { break };
        let Ok(bytes) = std::fs::read(defs_dir.join(format!("{name}.json"))) else { break };
        let Ok(schema): Result<TagSchema, _> = serde_json::from_slice(&bytes) else { break };
        // Skip the target itself — we only add parent chain sizes.
        if cur != target_tag {
            let Some(block) = schema.blocks.get(&schema.block) else { break };
            let Some(rs) = schema.structs.get(&block.struct_name) else { break };
            sum = sum.saturating_add(rs.size);
        }
        let Some(parent) = schema.parent_tag else { break };
        cur = parent;
    }
    sum
}

fn build_layout_from_schema(
    schema: TagSchema,
    defs_dir: &Path,
    folded_templates: &std::collections::BTreeSet<String>,
) -> Result<TagLayout, TagSchemaError> {
    let _ = parse_group_tag(&schema.tag)?; // validate early

    let mut strings = StringTable::new();

    // field_types registry — grown on-demand as fields are emitted.
    let mut field_types: Vec<TagFieldTypeLayout> = Vec::new();
    let mut field_type_index_by_name: std::collections::HashMap<&'static str, u32> = Default::default();
    let mut intern_field_type = |canonical: &'static str, size: u32, needs_sub: u32,
                                 strings: &mut StringTable|
     -> u32 {
        if let Some(&i) = field_type_index_by_name.get(canonical) {
            return i;
        }
        let name_offset = strings.intern(canonical);
        let i = field_types.len() as u32;
        field_types.push(TagFieldTypeLayout {
            name_offset,
            size,
            needs_sub_chunk: needs_sub,
        });
        field_type_index_by_name.insert(canonical, i);
        i
    };

    // Build custom_block_index_search_names_offsets — one entry per
    // *distinct* search-name string seen on custom_*_block_index
    // fields. Fields' `definition` becomes the index into here.
    // (Our JSON doesn't currently carry search names, so this stays
    // empty unless the dumper starts emitting them.)
    let custom_block_index_search_names_offsets: Vec<u32> = Vec::new();

    // Build data_definition_name_offsets from `datas` keys.
    let data_definition_name_offsets: Vec<u32> = schema
        .datas
        .keys()
        .map(|n| strings.intern(schema_display_name(n)))
        .collect();

    // Build string_lists (enums/flags). Each enum's options go into
    // string_offsets contiguously; string_lists[i] points at that
    // slice.
    let mut string_offsets: Vec<u32> = Vec::new();
    let mut string_lists: Vec<TagStringList> = Vec::new();
    for (name, enum_schema) in &schema.enums_flags {
        let list_name_offset = strings.intern(schema_display_name(name));
        let first = string_offsets.len() as u32;
        for opt in &enum_schema.options {
            let off = match opt {
                Some(s) => strings.intern(s),
                None => 0, // null option slot → empty string at offset 0
            };
            string_offsets.push(off);
        }
        string_lists.push(TagStringList {
            offset: list_name_offset,
            count: enum_schema.options.len() as u32,
            first,
        });
    }

    // Helper for the four lookups below — array/resource/interop/block
    // each name a struct, and we want a uniform "unknown struct" error.
    let resolve_struct_name = |name: &str| -> Result<u32, TagSchemaError> {
        schema
            .struct_index(name)
            .ok_or_else(|| TagSchemaError::UnknownReference { kind: "struct", name: name.to_owned() })
    };

    // Build array_layouts (resolve each array's struct by name).
    let mut array_layouts: Vec<TagArrayLayout> = Vec::with_capacity(schema.arrays.len());
    for (name, array) in &schema.arrays {
        array_layouts.push(TagArrayLayout {
            name_offset: strings.intern(schema_display_name(name)),
            count: array.count,
            struct_index: resolve_struct_name(&array.struct_name)?,
        });
    }

    // Build resource_layouts.
    let mut resource_layouts: Vec<TagResourceLayout> = Vec::with_capacity(schema.resources.len());
    for (name, resource) in &schema.resources {
        resource_layouts.push(TagResourceLayout {
            name_offset: strings.intern(schema_display_name(name)),
            unknown: resource.flags as u32,
            struct_index: resolve_struct_name(&resource.struct_name)?,
        });
    }

    // Build interop_layouts.
    let mut interop_layouts: Vec<TagInteropLayout> = Vec::with_capacity(schema.interops.len());
    for (name, interop) in &schema.interops {
        interop_layouts.push(TagInteropLayout {
            name_offset: strings.intern(schema_display_name(name)),
            struct_index: resolve_struct_name(&interop.struct_name)?,
            guid: parse_guid(&interop.guid)?,
        });
    }

    // Build block_layouts.
    let mut block_layouts: Vec<TagBlockLayout> = Vec::with_capacity(schema.blocks.len());
    for (i, (name, block)) in schema.blocks.iter().enumerate() {
        block_layouts.push(TagBlockLayout {
            index: i as u32,
            name_offset: strings.intern(schema_display_name(name)),
            max_count: block.max_count,
            struct_index: resolve_struct_name(&block.struct_name)?,
        });
    }

    // Build struct_layouts + the flat `fields` array. For each struct,
    // remember its `first_field_index` before pushing its fields.
    let mut struct_layouts: Vec<TagStructLayout> = Vec::with_capacity(schema.structs.len());
    let mut fields: Vec<TagFieldLayout> = Vec::new();
    for (i, (name, struct_schema)) in schema.structs.iter().enumerate() {
        let first = fields.len() as u32;

        for field in &struct_schema.fields {
            let info = field_type_info(&field.ty)
                .ok_or_else(|| TagSchemaError::UnknownFieldType(field.ty.clone()))?;

            // An `explanation` is the schema's documentation text, and a shipped
            // layout does not carry the text — but it *does* carry the field.
            //
            // This used to drop them entirely, on the belief that tags carry none
            // at all. Measured against HREK, that is wrong: the kits keep a
            // zero-width, unnamed `custom` field at each explanation's position,
            // so `cheap_particle_emitter`'s root has 43 fields where dropping
            // them gave 36 — the seven explanations in its schema. The bytes were
            // right either way, because these occupy none; what diverged was the
            // field *list*, and an editing kit walking its own field list against
            // a tag's is entitled to refuse the tag or fall over on it.
            //
            // So they are emitted the way the kits write them: `custom`, size 0,
            // no name. Keeping them as `explanation` would align the count and
            // still disagree on the type.
            let explanation = matches!(info.ty, TagFieldType::Explanation);
            let (canonical, ty, size) = if explanation {
                ("custom", TagFieldType::Custom, 0)
            } else {
                (info.canonical, info.ty, info.size)
            };

            let type_index =
                intern_field_type(canonical, size, info.needs_sub_chunk, &mut strings);

            // Clean the field name to its bare display form (drop `:units`,
            // `#help`, `{alias}`, and trailing `*`/`!` markers) so the embedded
            // layout matches shipped tags rather than the verbose JSON schema.
            // A shipped layout carries no name for a `custom` field, whatever the
            // JSON calls it. Measured across HREK: every `custom` in a kit tag's
            // layout is unnamed — the decorators, and the `tmpl` render-method
            // hole alike — while the dump names them `types`, `mapping`,
            // `shader`. An explanation is the same case with its heading text.
            //
            // The name is metadata on a field that carries no data, so dropping
            // it moves nothing; what it buys is a field list the editing kits
            // recognise as their own, which is what `cheap_particle_emitter`
            // needed. The JSON keeps the names, so nothing is lost that a reader
            // of the schema wanted.
            let field_name_offset = match &field.name {
                Some(n) if !explanation && !matches!(ty, TagFieldType::Custom) => {
                    strings.intern(&clean_blay_field_name(n))
                }
                _ => 0,
            };

            let definition = if explanation {
                0
            } else {
                resolve_field_definition(field, info.ty, &schema)?
            };

            fields.push(TagFieldLayout {
                name_offset: field_name_offset,
                type_index,
                definition,
                field_type: ty,
                offset: 0, // computed later by compute_struct_layout
            });
        }

        struct_layouts.push(TagStructLayout {
            index: i as u32,
            guid: parse_guid(&struct_schema.guid)?,
            name_offset: strings.intern(schema_display_name(name)),
            first_field_index: first,
            size: 0, // computed later
            version: 0,
        });
    }

    // Pull root-block index. Its struct's guid/size become the layout-
    // level guid/root_data_size (matching `TagLayout::read`).
    let root_block_index = schema.block_index(&schema.block).ok_or_else(|| {
        TagSchemaError::UnknownReference { kind: "block", name: schema.block.clone() }
    })?;
    let root_struct_index = block_layouts[root_block_index as usize].struct_index as usize;
    let root_struct = &struct_layouts[root_struct_index];
    let layout_guid = root_struct.guid;
    let schema_root_size = schema.structs.iter().nth(root_struct_index).map(|(_, s)| s.size).unwrap_or(0);

    let header = TagLayoutHeader {
        tag_group_block_index: root_block_index,
        string_data_size: 0, // filled in below
        string_offset_count: string_offsets.len() as u32,
        string_list_count: string_lists.len() as u32,
        custom_block_index_search_names_count: custom_block_index_search_names_offsets.len() as u32,
        data_definition_name_count: data_definition_name_offsets.len() as u32,
        array_layout_count: array_layouts.len() as u32,
        field_type_count: field_types.len() as u32,
        field_count: fields.len() as u32,
        aggregate_layout_count: 0,
        struct_layout_count: struct_layouts.len() as u32,
        block_layout_count: block_layouts.len() as u32,
        resource_layout_count: resource_layouts.len() as u32,
        interop_layout_count: interop_layouts.len() as u32,
    };

    // Classic Halo 2 inline-struct tags (0 = no on-disk header), parallel
    // to struct_layouts order.
    let struct_tags: Vec<u32> = schema
        .structs
        .values()
        .map(|s| match &s.tag {
            Some(t) => crate::fields::parse_group_tag(t).unwrap_or(0),
            None => 0,
        })
        .collect();

    // Classic Halo 2 per-version struct variant table, parallel to
    // struct_layouts order. `Some(v)` only for a base (multi-version)
    // struct: `v[n]` is the struct index of the variant for on-disk
    // version `n`, gaps padded with the base index. `None` everywhere
    // else (single-version structs + the variant entries themselves).
    let struct_version_table: Vec<Option<Vec<u32>>> = schema
        .structs
        .keys()
        .map(|name| {
            let vmap = schema.struct_versions.get(name)?;
            let base_idx = schema.struct_index(name)?;
            let max_ver = vmap
                .keys()
                .filter_map(|k| k.parse::<usize>().ok())
                .max()
                .unwrap_or(0);
            let mut v = vec![base_idx; max_ver + 1];
            for (ver_str, variant) in vmap {
                if let (Ok(ver), Some(idx)) = (ver_str.parse::<usize>(), schema.struct_index(variant)) {
                    if ver < v.len() {
                        v[ver] = idx;
                    }
                }
            }
            Some(v)
        })
        .collect();

    let mut result = TagLayout {
        root_data_size: schema_root_size,
        guid: layout_guid,
        version: 3, // H3 MCC — layout payload version 3
        header: TagLayoutHeader {
            string_data_size: strings.bytes.len() as u32,
            ..header
        },
        string_data: strings.bytes,
        string_offsets,
        string_lists,
        custom_block_index_search_names_offsets,
        data_definition_name_offsets,
        array_layouts,
        field_types,
        fields,
        block_layouts,
        resource_layouts,
        interop_layouts,
        struct_layouts,
        struct_tags,
        struct_version_table,
        tmpl_holes: Vec::new(),
    };

    // Compute struct sizes + field offsets. First pass with tmpl
    // customs stored at 0 (no expansion) — matches how H3 schemas lay
    // out (common shader fields are inlined directly in the struct
    // field that follows the tmpl).
    //
    // Every `tmpl` custom is recorded, including the ones that expand to
    // nothing: the point of `tmpl_holes` is *which template* a hole belongs
    // to, and a dead template like `ssfx` still answers that question. Only
    // the non-zero ones go on to have their size patched into the field's
    // `definition` slot below, because only those change the arithmetic.
    let mut tmpl_holes: Vec<TagTemplateHole> = Vec::new();
    let tmpl_expansions: Vec<(usize, u32)> = {
        let mut out = Vec::new();
        let mut global_field_idx = 0usize;
        for (_, struct_schema) in schema.structs.iter() {
            for field in &struct_schema.fields {
                if field.ty == "custom"
                    && field.group_tag.as_deref() == Some("tmpl")
                    && let Some(target) = field.definition.as_str() {
                        // A template whose inherited fields now live in the
                        // sibling struct must not also be reserved here, or the
                        // enclosing struct pays for the same bytes twice.
                        let exp = if folded_templates.contains(target) {
                            0
                        } else {
                            tmpl_expansion_size(defs_dir, target)
                        };
                        // A template whose tag is unparseable is not recorded
                        // rather than recorded as zero: an unknown identity and
                        // a known-empty one are different claims.
                        if let Ok(group_tag) = parse_group_tag(target) {
                            tmpl_holes.push(TagTemplateHole {
                                field_index: global_field_idx as u32,
                                group_tag,
                                size: exp,
                            });
                        }
                        if exp > 0 {
                            out.push((global_field_idx, exp));
                        }
                    }
                global_field_idx += 1;
            }
        }
        out
    };
    // `binary_search_by_key` in `template_hole` needs this ordering; the walk
    // above produces it, and sorting says so rather than relying on it.
    tmpl_holes.sort_by_key(|hole| hole.field_index);
    result.tmpl_holes = tmpl_holes;

    for i in 0..result.struct_layouts.len() {
        result.compute_struct_layout(i);
    }

    // Cross-check computed sizes against the schema's stated sizes.
    // If declared > computed and this struct has tmpl customs, apply
    // their expansion (Reach-style: parent-chain inlined here) and
    // recompute. If declared still doesn't match — or we're > declared
    // — it's a genuine mismatch.
    for (i, (name, struct_schema)) in schema.structs.iter().enumerate() {
        let computed = result.struct_layouts[i].size;
        let declared = struct_schema.size as usize;
        if computed == declared {
            continue;
        }
        if computed < declared {
            // Try tmpl expansion for this struct's fields.
            let first = result.struct_layouts[i].first_field_index as usize;
            let mut field_idx = first;
            let mut applied = 0usize;
            while result.fields[field_idx].field_type != TagFieldType::Terminator {
                if let Some(&(_, exp)) = tmpl_expansions.iter().find(|&&(fi, _)| fi == field_idx) {
                    result.fields[field_idx].definition = exp;
                    applied += exp as usize;
                }
                field_idx += 1;
            }
            if applied > 0 {
                // Reset the struct's size so compute_struct_layout runs again.
                result.struct_layouts[i].size = 0;
                result.compute_struct_layout(i);
            }
        }
        let computed = result.struct_layouts[i].size;
        if computed != declared {
            return Err(TagSchemaError::StructSizeMismatch {
                name: name.clone(),
                schema: struct_schema.size,
                computed,
            });
        }
    }

    // Update header size-counts that depend on final string_data size.
    result.header.string_data_size = result.string_data.len() as u32;

    Ok(result)
}

/// Translate a field schema's `definition` value into the `u32` that
/// goes into the corresponding `TagFieldLayout`. The interpretation
/// depends on the field type:
///
/// - named-registry types (struct/block/array/flags/enum/data/
///   resource/interop): string → index into the matching table.
/// - `pad`/`useless_pad`/`skip`: integer → byte count (stored in the
///   `definition` slot verbatim).
/// - `tag_reference`: object → would normally store flags+allowed,
///   but blay only stores flags here (just flags slot).
/// - `explanation`: string → stored as a string offset into
///   string_data.
/// - primitives / `terminator`: 0.
/// Reduce a schema field name to the bare display name shipped tags store in
/// their embedded layout: cut at the first `:` (units) or `#` (help/tooltip),
/// drop `{alias}` annotations (keeping the base name, as shipped tags do), and
/// strip trailing `*`/`!` markers. Keeps `[range]` hints, matching real tags.
fn clean_blay_field_name(name: &str) -> String {
    let cut = name.find([':', '#']).unwrap_or(name.len());
    let mut s = name[..cut].to_string();
    while let (Some(open), Some(close)) = (s.find('{'), s.find('}')) {
        if open < close {
            s.replace_range(open..=close, "");
        } else {
            break;
        }
    }
    s.trim_end_matches(['*', '!', '^', ' ']).trim().to_string()
}

fn resolve_field_definition(
    field: &TagFieldSchema,
    ty: TagFieldType,
    schema: &TagSchema,
) -> Result<u32, TagSchemaError> {
    let def = &field.definition;

    // `custom` fields contribute 0 bytes by default. `tmpl`-typed
    // customs inline their target group's parent-chain size only
    // when the containing struct's declared size is larger than the
    // sum of plain field sizes — that post-hoc patch happens in
    // `build_layout_from_schema`, not here.
    if matches!(ty, TagFieldType::Custom) {
        return Ok(0);
    }

    // Primitives & no-definition types: return 0.
    if matches!(
        ty,
        TagFieldType::Unknown
            | TagFieldType::String
            | TagFieldType::LongString
            | TagFieldType::StringId
            | TagFieldType::OldStringId
            | TagFieldType::CharInteger
            | TagFieldType::ShortInteger
            | TagFieldType::LongInteger
            | TagFieldType::Int64Integer
            | TagFieldType::ByteInteger
            | TagFieldType::WordInteger
            | TagFieldType::DwordInteger
            | TagFieldType::QwordInteger
            | TagFieldType::Angle
            | TagFieldType::Tag
            | TagFieldType::Point2d
            | TagFieldType::Rectangle2d
            | TagFieldType::RgbColor
            | TagFieldType::ArgbColor
            | TagFieldType::Real
            | TagFieldType::RealSlider
            | TagFieldType::RealFraction
            | TagFieldType::RealPoint2d
            | TagFieldType::RealPoint3d
            | TagFieldType::RealVector2d
            | TagFieldType::RealVector3d
            | TagFieldType::RealQuaternion
            | TagFieldType::RealEulerAngles2d
            | TagFieldType::RealEulerAngles3d
            | TagFieldType::RealPlane2d
            | TagFieldType::RealPlane3d
            | TagFieldType::RealRgbColor
            | TagFieldType::RealArgbColor
            | TagFieldType::RealHsvColor
            | TagFieldType::RealAhsvColor
            | TagFieldType::ShortIntegerBounds
            | TagFieldType::AngleBounds
            | TagFieldType::RealBounds
            | TagFieldType::FractionBounds
            | TagFieldType::VertexBuffer
            | TagFieldType::Pointer
            | TagFieldType::RealMatrix3x3
            | TagFieldType::CustomCharBlockIndex
            | TagFieldType::CustomShortBlockIndex
            | TagFieldType::CustomLongBlockIndex
            | TagFieldType::Terminator
            | TagFieldType::NonCacheRuntimeValue,
    ) {
        return Ok(0);
    }

    // Pad/skip/useless_pad: definition is a byte count integer.
    if matches!(ty, TagFieldType::Pad | TagFieldType::UselessPad | TagFieldType::Skip) {
        return def
            .as_u64()
            .map(|v| v as u32)
            .ok_or_else(|| TagSchemaError::BadFieldDefinition {
                field: field.name.clone().unwrap_or_default(),
                ty: field.ty.clone(),
            });
    }

    // Explanation: store as 0 in the layout (blay's `definition` slot
    // holds the string offset at runtime via a separate mechanism).
    // Preserving the text in string_data is out-of-scope for now.
    if matches!(ty, TagFieldType::Explanation) {
        return Ok(0);
    }

    // tag_reference: blay's `definition` holds flags. `allowed` list
    // isn't part of blay's field record.
    if matches!(ty, TagFieldType::TagReference) {
        let flags = def
            .as_object()
            .and_then(|m| m.get("flags"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        return Ok(flags as u32);
    }

    // Named-registry types: resolve by name.
    let name = def.as_str().ok_or_else(|| TagSchemaError::BadFieldDefinition {
        field: field.name.clone().unwrap_or_default(),
        ty: field.ty.clone(),
    })?;
    let lookup = match ty {
        TagFieldType::Struct => schema.struct_index(name),
        TagFieldType::Block
        | TagFieldType::LongBlockFlags
        | TagFieldType::WordBlockFlags
        | TagFieldType::ByteBlockFlags
        | TagFieldType::CharBlockIndex
        | TagFieldType::ShortBlockIndex
        | TagFieldType::LongBlockIndex => schema.block_index(name),
        TagFieldType::Array => schema.array_index(name),
        TagFieldType::CharEnum
        | TagFieldType::ShortEnum
        | TagFieldType::LongEnum
        | TagFieldType::LongFlags
        | TagFieldType::WordFlags
        | TagFieldType::ByteFlags => schema.enum_index(name),
        TagFieldType::Data => schema.data_index(name),
        TagFieldType::PageableResource => schema.resource_index(name),
        TagFieldType::ApiInterop => schema.interop_index(name),
        _ => None,
    };
    lookup.ok_or_else(|| TagSchemaError::UnknownReference {
        kind: match ty {
            TagFieldType::Struct => "struct",
            TagFieldType::Block
            | TagFieldType::LongBlockFlags
            | TagFieldType::WordBlockFlags
            | TagFieldType::ByteBlockFlags
            | TagFieldType::CharBlockIndex
            | TagFieldType::ShortBlockIndex
            | TagFieldType::LongBlockIndex => "block",
            TagFieldType::Array => "array",
            TagFieldType::CharEnum
            | TagFieldType::ShortEnum
            | TagFieldType::LongEnum
            | TagFieldType::LongFlags
            | TagFieldType::WordFlags
            | TagFieldType::ByteFlags => "enum_or_flags",
            TagFieldType::Data => "data",
            TagFieldType::PageableResource => "resource",
            TagFieldType::ApiInterop => "interop",
            _ => "?",
        },
        name: name.to_owned(),
    })
}
