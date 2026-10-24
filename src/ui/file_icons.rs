//! File-type icons for the Files panel, drawn from the vendored Material pack.
//!
//! The pack's own rules, nothing more: an exact file name first, then the
//! longest extension the name ends in, then the pack's default. Folders are
//! matched by name, and carry a second icon for the expanded state. Nothing
//! here reads a file or asks a language server, so a remote workspace's tree
//! gets the same icons as a local one.
//!
//! Names are lowercased once, where a listing is built, not per row: a tree can
//! be thousands of rows and every frame draws all of them.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

use crate::ui::assets;

const MAPPING_JSON: &str = include_str!("../../assets/icons/material/material-icons.json");

/// The closed icon, the expanded one, and their light-theme variants.
///
/// Four indices rather than two paths: which of the pair to draw depends on
/// whether the theme is light, and that can change while a listing is cached.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileIcon {
    normal: u16,
    open: u16,
    light_normal: u16,
    light_open: u16,
}

impl Default for FileIcon {
    /// What a row that is not a file — a note standing in for an empty
    /// directory — carries. It draws no icon; this is only a value to hold.
    fn default() -> Self {
        let mapping = mapping();
        FileIcon::pair(mapping, mapping.default_file, mapping.default_file)
    }
}

impl FileIcon {
    /// The asset path to draw, as the theme and the row's state choose it.
    pub(crate) fn path(&self, light: bool, expanded: bool) -> &'static str {
        let index = match (light, expanded) {
            (false, false) => self.normal,
            (false, true) => self.open,
            (true, false) => self.light_normal,
            (true, true) => self.light_open,
        };
        assets::material_icon_path(index)
    }

    fn pair(mapping: &Mapping, normal: u16, open: u16) -> FileIcon {
        FileIcon {
            normal,
            open,
            light_normal: mapping.light_of(normal),
            light_open: mapping.light_of(open),
        }
    }
}

/// The icon for a row named `name`.
///
/// A tree's root row takes the ordinary folder rather than the pack's
/// `rootFolder` art: that one is a grey ring, which at 16px beside the chevron
/// reads as an unselected radio button, not as the folder the row is.
pub(crate) fn for_entry(name: &str, is_dir: bool) -> FileIcon {
    let mapping = mapping();
    // Most names are already lower case; only the ones that are not pay for a
    // second string.
    let lower = match name.chars().any(char::is_uppercase) {
        true => std::borrow::Cow::Owned(name.to_lowercase()),
        false => std::borrow::Cow::Borrowed(name),
    };
    if is_dir {
        let closed = mapping
            .folder_names
            .get(&*lower)
            .copied()
            .unwrap_or(mapping.default_folder);
        let open = mapping
            .folder_names_expanded
            .get(&*lower)
            .copied()
            .unwrap_or(mapping.default_folder_open);
        FileIcon::pair(mapping, closed, open)
    } else {
        let index = file_index(mapping, &lower).unwrap_or(mapping.default_file);
        FileIcon::pair(mapping, index, index)
    }
}

/// The exact name, then the longest extension it ends in: `foo.d.ts.map` tries
/// `d.ts.map`, then `ts.map`, then `map` — the pack files the middle one.
fn file_index(mapping: &Mapping, lower: &str) -> Option<u16> {
    if let Some(&index) = mapping.file_names.get(lower) {
        return Some(index);
    }
    lower
        .match_indices('.')
        .find_map(|(dot, _)| mapping.file_extensions.get(&lower[dot + 1..]).copied())
}

fn mapping() -> &'static Mapping {
    static MAPPING: OnceLock<Mapping> = OnceLock::new();
    MAPPING.get_or_init(|| {
        parse().unwrap_or_else(|| {
            log::warn!("the Material icon mapping could not be read; every row gets the default");
            Mapping::defaults()
        })
    })
}

struct Mapping {
    file_names: HashMap<Box<str>, u16>,
    file_extensions: HashMap<Box<str>, u16>,
    folder_names: HashMap<Box<str>, u16>,
    folder_names_expanded: HashMap<Box<str>, u16>,
    default_file: u16,
    default_folder: u16,
    default_folder_open: u16,
    /// The light-theme definition for each icon, itself when there is none.
    light: Vec<u16>,
}

impl Mapping {
    /// Only reached if the embedded JSON stops parsing, which the build would
    /// have to have broken: the defaults are named by the pack itself, so they
    /// are still served.
    fn defaults() -> Mapping {
        let icon = |name: &str| {
            assets::material_icon_index_by_name(name)
                .unwrap_or_else(|| panic!("the Material pack ships no `{name}.svg`"))
        };
        Mapping {
            file_names: HashMap::new(),
            file_extensions: HashMap::new(),
            folder_names: HashMap::new(),
            folder_names_expanded: HashMap::new(),
            default_file: icon("file"),
            default_folder: icon("folder"),
            default_folder_open: icon("folder-open"),
            light: (0..assets::material_icon_count() as u16).collect(),
        }
    }

    fn light_of(&self, index: u16) -> u16 {
        self.light.get(index as usize).copied().unwrap_or(index)
    }
}

fn parse() -> Option<Mapping> {
    let raw: RawMapping<'static> = serde_json::from_str(MAPPING_JSON).ok()?;
    let index_of = |definition: &str| definition_index(&raw, definition);
    let resolve = |definition: &str, fallback: &str| {
        index_of(definition)
            .or_else(|| assets::material_icon_index_by_name(fallback))
            .unwrap_or_else(|| panic!("the Material pack ships no `{fallback}.svg`"))
    };

    let mut mapping = Mapping {
        file_names: resolve_map(&raw.file_names, &index_of),
        file_extensions: resolve_map(&raw.file_extensions, &index_of),
        folder_names: resolve_map(&raw.folder_names, &index_of),
        folder_names_expanded: resolve_map(&raw.folder_names_expanded, &index_of),
        default_file: resolve(raw.file, "file"),
        default_folder: resolve(raw.folder, "folder"),
        default_folder_open: resolve(raw.folder_expanded, "folder-open"),
        light: (0..assets::material_icon_count() as u16).collect(),
    };

    // The pack states its light variants per key, not per definition: the same
    // key that picks `toml` in a dark theme picks `toml_light` in a light one.
    // Each table is overlaid on the icon its dark key already resolved to.
    let Mapping {
        light,
        file_names,
        file_extensions,
        folder_names,
        folder_names_expanded,
        ..
    } = &mut mapping;
    for (dark, raw_light) in [
        (&*file_names, &raw.light.file_names),
        (&*file_extensions, &raw.light.file_extensions),
        (&*folder_names, &raw.light.folder_names),
        (&*folder_names_expanded, &raw.light.folder_names_expanded),
    ] {
        for (key, definition) in raw_light {
            let Some((&dark_index, light_index)) = dark
                .get(key.to_lowercase().as_str())
                .zip(index_of(definition))
            else {
                continue;
            };
            light[dark_index as usize] = light_index;
        }
    }
    Some(mapping)
}

/// The pack's keys are mostly lowercase but not all (`APKBUILD`, `META-INF`),
/// and a case-insensitive filesystem means both spellings name the same file —
/// so both sides are folded here, once.
fn resolve_map(
    raw: &HashMap<&'static str, &'static str>,
    index_of: &impl Fn(&str) -> Option<u16>,
) -> HashMap<Box<str>, u16> {
    raw.iter()
        .filter_map(|(key, definition)| Some((key.to_lowercase().into(), index_of(definition)?)))
        .collect()
}

/// The pack index of a definition, through the icon file it points at.
fn definition_index(raw: &RawMapping<'static>, definition: &str) -> Option<u16> {
    let path = raw.icon_definitions.get(definition)?.icon_path;
    let name = path.rsplit('/').next()?.strip_suffix(".svg")?;
    assets::material_icon_index_by_name(name)
}

#[derive(Deserialize)]
struct RawMapping<'a> {
    #[serde(rename = "iconDefinitions")]
    icon_definitions: HashMap<&'a str, RawDefinition<'a>>,
    #[serde(rename = "fileExtensions")]
    file_extensions: HashMap<&'a str, &'a str>,
    #[serde(rename = "fileNames")]
    file_names: HashMap<&'a str, &'a str>,
    #[serde(rename = "folderNames")]
    folder_names: HashMap<&'a str, &'a str>,
    #[serde(rename = "folderNamesExpanded")]
    folder_names_expanded: HashMap<&'a str, &'a str>,
    file: &'a str,
    folder: &'a str,
    #[serde(rename = "folderExpanded")]
    folder_expanded: &'a str,
    #[serde(default)]
    light: RawLight<'a>,
}

#[derive(Deserialize)]
struct RawDefinition<'a> {
    #[serde(rename = "iconPath")]
    icon_path: &'a str,
}

#[derive(Default, Deserialize)]
#[serde(bound(deserialize = "'de: 'a"))]
struct RawLight<'a> {
    #[serde(rename = "fileExtensions", default)]
    file_extensions: HashMap<&'a str, &'a str>,
    #[serde(rename = "fileNames", default)]
    file_names: HashMap<&'a str, &'a str>,
    #[serde(rename = "folderNames", default)]
    folder_names: HashMap<&'a str, &'a str>,
    #[serde(rename = "folderNamesExpanded", default)]
    folder_names_expanded: HashMap<&'a str, &'a str>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(icon: FileIcon, light: bool, expanded: bool) -> &'static str {
        icon.path(light, expanded)
    }

    #[test]
    fn names_and_extensions_pick_the_packs_icons() {
        assert!(name(for_entry("package.json", false), false, false).ends_with("/nodejs.svg"));
        assert!(name(for_entry("main.rs", false), false, false).ends_with("/rust.svg"));
        // The extension is the longest dotted suffix the pack knows, not the
        // last one: it files `ts.map` under JSON, and `d.ts` under
        // `typescript-def`.
        assert!(name(for_entry("App.d.ts.map", false), false, false).ends_with("/json.svg"));
        assert!(name(for_entry("App.d.ts", false), false, false).ends_with("/typescript-def.svg"));
        // Names are matched without regard to case, on both sides.
        assert_eq!(
            name(for_entry("Package.JSON", false), false, false),
            name(for_entry("package.json", false), false, false)
        );
        assert_eq!(
            name(for_entry("APKBUILD", false), false, false),
            name(for_entry("apkbuild", false), false, false)
        );
    }

    #[test]
    fn an_unknown_name_falls_back_to_the_default_file() {
        let icon = for_entry("notes.unknownextension", false);
        assert_eq!(
            name(icon, false, false),
            assets::material_icon_path(mapping().default_file)
        );
    }

    #[test]
    fn folders_carry_both_of_their_states() {
        let rust = for_entry("Rust", true);
        assert!(name(rust, false, false).ends_with("/folder-rust.svg"));
        assert!(name(rust, false, true).ends_with("/folder-rust-open.svg"));

        let named = for_entry("src", true);
        assert!(name(named, false, false).ends_with("/folder-src.svg"));

        let plain = for_entry("nothing-names-this-folder", true);
        assert!(name(plain, false, false).ends_with("/folder.svg"));
        assert!(name(plain, false, true).ends_with("/folder-open.svg"));

        // A tree's root row is a folder like any other here, not the pack's
        // `rootFolder` ring.
        let root = for_entry("tty7", true);
        assert!(name(root, false, true).ends_with("/folder-open.svg"));
    }

    #[test]
    fn the_light_theme_swaps_the_icons_the_pack_has_a_variant_for() {
        // `toml` is one of the definitions the pack repaints for light themes.
        let toml = for_entry("Cargo.toml", false);
        assert!(name(toml, false, false).ends_with("/toml.svg"));
        assert!(name(toml, true, false).ends_with("/toml_light.svg"));
        // A definition with no light variant keeps its dark drawing.
        let rust = for_entry("main.rs", false);
        assert_eq!(name(rust, false, false), name(rust, true, false));
    }

    /// The pack names an icon in every definition and every mapping entry; one
    /// the tarball did not ship would draw nothing at all, which no one notices
    /// until they happen to look at that file type.
    #[test]
    fn every_icon_the_mapping_names_is_shipped() {
        let raw: RawMapping<'static> =
            serde_json::from_str(MAPPING_JSON).expect("the vendored mapping is valid JSON");
        for (definition, entry) in &raw.icon_definitions {
            assert!(
                definition_index(&raw, definition).is_some(),
                "{definition} points at {}, which the pack does not ship",
                entry.icon_path
            );
        }
        for (what, map) in [
            ("fileNames", &raw.file_names),
            ("fileExtensions", &raw.file_extensions),
            ("folderNames", &raw.folder_names),
            ("folderNamesExpanded", &raw.folder_names_expanded),
            // The light tables are a second set of references over the same
            // pack, and a dangling one only shows in a light theme.
            ("light.fileNames", &raw.light.file_names),
            ("light.fileExtensions", &raw.light.file_extensions),
            ("light.folderNames", &raw.light.folder_names),
            (
                "light.folderNamesExpanded",
                &raw.light.folder_names_expanded,
            ),
        ] {
            for (key, definition) in map {
                assert!(
                    definition_index(&raw, definition).is_some(),
                    "{what}[{key}] names {definition}, which the pack does not ship"
                );
            }
        }
    }
}
