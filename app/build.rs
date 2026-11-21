//! Embeds the application icon and manifest in the Windows executable.
//!
//! The resources are written directly as a COFF object with a `.rsrc`
//! section, which the linker merges into the executable's resource directory.
//! This needs no resource compiler (rc.exe or windres), so the GNU toolchain
//! builds the executable on its own.

use std::env;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;

use image::codecs::png::PngEncoder;
use image::{ExtendedColorType, ImageEncoder};

#[path = "src/icon.rs"]
mod icon;
const MANIFEST: &str = "assets/lemon-mix.manifest";

/// Icon sizes in the executable, largest first, as the previous build had.
const ICON_SIZES: [u32; 5] = [256, 64, 48, 32, 16];

const RT_ICON: u32 = 3;
const RT_GROUP_ICON: u32 = 14;
const RT_MANIFEST: u32 = 24;
const RT_VERSION: u32 = 16;
const LANG_NEUTRAL: u32 = 0;
const LANG_EN_US: u32 = 0x0409;

/// The icon group the GUI loads with LoadImageW(instance, MAKEINTRESOURCE(1)).
const ICON_GROUP_ID: u32 = 1;
const MANIFEST_ID: u32 = 1; // CREATEPROCESS_MANIFEST_RESOURCE_ID
const VERSION_ID: u32 = 1; // VS_VERSION_INFO

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed={MANIFEST}");
    // The version is the workspace's, which a release sets.
    println!("cargo:rerun-if-changed=../Cargo.toml");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let mut resources = Vec::new();
    let icons = icon_images();
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes()); // reserved
    group.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    group.extend_from_slice(&(icons.len() as u16).to_le_bytes());
    for (index, (size, png)) in icons.into_iter().enumerate() {
        let id = index as u32 + 1;
        let dimension = if size >= 256 { 0 } else { size as u8 }; // 0 means 256
        group.extend_from_slice(&[dimension, dimension, 0, 0]);
        group.extend_from_slice(&1u16.to_le_bytes()); // planes
        group.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        group.extend_from_slice(&(png.len() as u32).to_le_bytes());
        group.extend_from_slice(&(id as u16).to_le_bytes());
        resources.push(Resource { kind: RT_ICON, id, lang: LANG_NEUTRAL, data: png });
    }
    resources.push(Resource { kind: RT_GROUP_ICON, id: ICON_GROUP_ID, lang: LANG_NEUTRAL, data: group });
    let manifest = fs::read(MANIFEST).unwrap_or_else(|err| panic!("read {MANIFEST}: {err}"));
    resources.push(Resource { kind: RT_MANIFEST, id: MANIFEST_ID, lang: LANG_EN_US, data: manifest });
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo sets the version");
    resources.push(Resource { kind: RT_VERSION, id: VERSION_ID, lang: LANG_EN_US, data: version_info(&version) });

    let object = coff_object(&resources);
    let path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("resources.o");
    fs::write(&path, object).expect("write resource object");
    println!("cargo:rustc-link-arg-bins={}", path.display());
}

/// Draws the icon at each size and encodes it as PNG, which Windows accepts
/// for every icon size since Vista.
fn icon_images() -> Vec<(u32, Vec<u8>)> {
    ICON_SIZES
        .iter()
        .map(|&size| {
            let pixels = icon::lemon_rgba(size);
            let mut png = Vec::new();
            PngEncoder::new(Cursor::new(&mut png))
                .write_image(&pixels, size, size, ExtendedColorType::Rgba8)
                .expect("encode icon PNG");
            (size, png)
        })
        .collect()
}

struct Resource {
    kind: u32,
    id: u32,
    lang: u32,
    data: Vec<u8>,
}

/// Builds an AMD64 COFF object whose `.rsrc` section holds a three-level
/// resource directory (type, name, language). Each data entry's RVA is
/// relocated against the section, as resource compilers emit it.
fn coff_object(resources: &[Resource]) -> Vec<u8> {
    let mut resources: Vec<&Resource> = resources.iter().collect();
    resources.sort_by_key(|r| (r.kind, r.id, r.lang));
    let mut kinds: Vec<u32> = resources.iter().map(|r| r.kind).collect();
    kinds.dedup();

    // Directory tables: the root, one per type, and one per (type, name).
    let dir_size = |entries: usize| 16 + 8 * entries;
    let mut offset = dir_size(kinds.len());
    let mut type_dirs = Vec::new(); // (kind, offset, resources)
    for &kind in &kinds {
        let of_kind: Vec<&Resource> = resources.iter().copied().filter(|r| r.kind == kind).collect();
        type_dirs.push((kind, offset, of_kind.clone()));
        offset += dir_size(of_kind.len());
    }
    let mut name_dirs = Vec::new(); // (offset, resource), one language each
    for (_, _, of_kind) in &type_dirs {
        for &resource in of_kind {
            name_dirs.push((offset, resource));
            offset += dir_size(1);
        }
    }
    let entries_offset = offset;
    offset += 16 * resources.len();
    let mut data_offsets = Vec::new();
    for resource in &resources {
        offset = offset.next_multiple_of(8);
        data_offsets.push(offset);
        offset += resource.data.len();
    }
    let section_size = offset.next_multiple_of(8);

    let mut section = vec![0u8; section_size];
    let put32 = |buf: &mut Vec<u8>, at: usize, value: u32| buf[at..at + 4].copy_from_slice(&value.to_le_bytes());
    let put_dir = |buf: &mut Vec<u8>, at: usize, entries: &[(u32, u32)]| {
        buf[at + 14..at + 16].copy_from_slice(&(entries.len() as u16).to_le_bytes()); // ID entries
        for (i, &(id, target)) in entries.iter().enumerate() {
            put32(buf, at + 16 + 8 * i, id);
            put32(buf, at + 20 + 8 * i, target);
        }
    };

    const SUBDIRECTORY: u32 = 0x8000_0000;
    let root: Vec<(u32, u32)> = type_dirs.iter().map(|(kind, at, _)| (*kind, *at as u32 | SUBDIRECTORY)).collect();
    put_dir(&mut section, 0, &root);
    let mut name_index = 0;
    for (_, at, of_kind) in &type_dirs {
        let mut entries = Vec::new();
        for resource in of_kind {
            entries.push((resource.id, name_dirs[name_index].0 as u32 | SUBDIRECTORY));
            name_index += 1;
        }
        put_dir(&mut section, *at, &entries);
    }
    let mut relocations = Vec::new();
    for (i, (at, resource)) in name_dirs.iter().enumerate() {
        // name_dirs follows the sorted resource order, as do the data entries.
        let entry = entries_offset + 16 * i;
        put_dir(&mut section, *at, &[(resource.lang, entry as u32)]);
        put32(&mut section, entry, data_offsets[i] as u32); // RVA, relocated below
        put32(&mut section, entry + 4, resource.data.len() as u32);
        relocations.push(entry as u32);
        section[data_offsets[i]..data_offsets[i] + resource.data.len()].copy_from_slice(&resource.data);
    }

    const HEADER_SIZE: usize = 20;
    const SECTION_HEADER_SIZE: usize = 40;
    const IMAGE_REL_AMD64_ADDR32NB: u16 = 3;
    let raw_offset = HEADER_SIZE + SECTION_HEADER_SIZE;
    let relocation_offset = raw_offset + section.len();
    let symbol_offset = relocation_offset + 10 * relocations.len();

    let mut object = Vec::new();
    object.extend_from_slice(&0x8664u16.to_le_bytes()); // IMAGE_FILE_MACHINE_AMD64
    object.extend_from_slice(&1u16.to_le_bytes()); // sections
    object.extend_from_slice(&0u32.to_le_bytes()); // timestamp
    object.extend_from_slice(&(symbol_offset as u32).to_le_bytes());
    object.extend_from_slice(&1u32.to_le_bytes()); // symbols
    object.extend_from_slice(&0u16.to_le_bytes()); // optional header size
    object.extend_from_slice(&0u16.to_le_bytes()); // characteristics

    object.extend_from_slice(b".rsrc\0\0\0");
    object.extend_from_slice(&0u32.to_le_bytes()); // virtual size
    object.extend_from_slice(&0u32.to_le_bytes()); // virtual address
    object.extend_from_slice(&(section.len() as u32).to_le_bytes());
    object.extend_from_slice(&(raw_offset as u32).to_le_bytes());
    object.extend_from_slice(&(relocation_offset as u32).to_le_bytes());
    object.extend_from_slice(&0u32.to_le_bytes()); // line numbers
    object.extend_from_slice(&(relocations.len() as u16).to_le_bytes());
    object.extend_from_slice(&0u16.to_le_bytes());
    object.extend_from_slice(&0x4000_0040u32.to_le_bytes()); // initialized data, readable

    object.extend_from_slice(&section);
    for at in relocations {
        object.extend_from_slice(&at.to_le_bytes());
        object.extend_from_slice(&0u32.to_le_bytes()); // symbol 0: the section
        object.extend_from_slice(&IMAGE_REL_AMD64_ADDR32NB.to_le_bytes());
    }

    object.extend_from_slice(b".rsrc\0\0\0");
    object.extend_from_slice(&0u32.to_le_bytes()); // value
    object.extend_from_slice(&1i16.to_le_bytes()); // section number
    object.extend_from_slice(&0u16.to_le_bytes()); // type
    object.push(3); // IMAGE_SYM_CLASS_STATIC
    object.push(0); // auxiliary symbols
    object.extend_from_slice(&4u32.to_le_bytes()); // empty string table
    object
}

/// The numbers of a version such as `1.2.3` or `1.2.3-dev`, for the fixed
/// part of the version resource, which holds four numbers.
fn version_numbers(version: &str) -> [u16; 4] {
    let release = version.split(['-', '+']).next().unwrap_or_default();
    let mut numbers = [0u16; 4];
    for (slot, part) in numbers.iter_mut().zip(release.split('.')) {
        *slot = part.parse().unwrap_or(0);
    }
    numbers
}

/// The version resource (VS_VERSIONINFO) that Windows shows in a file's
/// properties: the version as numbers and as text, and the product's name.
fn version_info(version: &str) -> Vec<u8> {
    let [major, minor, patch, build] = version_numbers(version).map(u32::from);
    let prerelease = version.contains('-');
    let mut fixed = Vec::new();
    for value in [
        0xFEEF_04BD,                      // signature
        0x0001_0000,                      // structure version
        (major << 16) | minor,            // file version
        (patch << 16) | build,            //
        (major << 16) | minor,            // product version
        (patch << 16) | build,            //
        0x3F,                             // flags that are valid
        if prerelease { 0x2 } else { 0 }, // VS_FF_PRERELEASE
        0x0004_0004,                      // VOS_NT_WINDOWS32
        0x1,                              // VFT_APP
        0,                                // subtype
        0,                                // date
        0,                                //
    ] {
        fixed.extend_from_slice(&u32::to_le_bytes(value));
    }
    let strings: Vec<Vec<u8>> = [
        ("CompanyName", "Tom Albans"),
        ("FileDescription", "Lemon Mix"),
        ("FileVersion", version),
        ("LegalCopyright", "Copyright (c) 2026 Tom Albans. MIT licence."),
        ("ProductName", "Lemon Mix"),
        ("ProductVersion", version),
    ]
    .iter()
    .map(|(key, value)| {
        let text = utf16z(value);
        version_block(key, 1, (text.len() / 2) as u16, &text, &[])
    })
    .collect();
    // US English, Unicode.
    let table = version_block("040904B0", 1, 0, &[], &strings);
    let string_info = version_block("StringFileInfo", 1, 0, &[], &[table]);
    let translation = [0x0409u16, 1200].iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>();
    let var = version_block("Translation", 0, translation.len() as u16, &translation, &[]);
    let var_info = version_block("VarFileInfo", 1, 0, &[], &[var]);
    version_block("VS_VERSION_INFO", 0, fixed.len() as u16, &fixed, &[string_info, var_info])
}

/// One block of a version resource: its length, its value's length, its
/// type (1 for text), its key, its value and its children, each starting on
/// a 32-bit boundary.
fn version_block(key: &str, kind: u16, value_length: u16, value: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    let mut block = vec![0u8; 6];
    block[2..4].copy_from_slice(&value_length.to_le_bytes());
    block[4..6].copy_from_slice(&kind.to_le_bytes());
    block.extend_from_slice(&utf16z(key));
    let align = |block: &mut Vec<u8>| block.resize(block.len().next_multiple_of(4), 0);
    align(&mut block);
    block.extend_from_slice(value);
    for child in children {
        align(&mut block);
        block.extend_from_slice(child);
    }
    let length = block.len() as u16;
    block[0..2].copy_from_slice(&length.to_le_bytes());
    block
}

/// Text as UTF-16, with its terminating zero.
fn utf16z(text: &str) -> Vec<u8> {
    text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}
