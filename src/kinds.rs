// InventoryX — file kinds and colours.
// Ported from KindTable.swift and TreemapRenderer.swift of Disk Inventory X.
// GPL-3.0-or-later.

use std::collections::HashMap;
use std::sync::Mutex;

pub const FOLDER: u16 = 0;
pub const FREE: u16 = 1;
pub const OTHER: u16 = 2;
pub const SYMLINK: u16 = 3;
pub const DOCUMENT: u16 = 4;
pub const EXECUTABLE: u16 = 5;
pub const OTHER_FILE: u16 = 6;
const FIRST_DYNAMIC: usize = 7;

/// Friendly names for common extensions. Extensions sharing a name share a kind.
const NAMES: &[(&str, &str)] = &[
    ("mkv", "Matroska Video"), ("mp4", "MPEG-4 Movie"), ("m4v", "MPEG-4 Movie"),
    ("avi", "AVI Movie"), ("mov", "QuickTime Movie"), ("wmv", "Windows Media Video"),
    ("ts", "MPEG-2 Transport Stream"), ("m2ts", "MPEG-2 Transport Stream"),
    ("mpg", "MPEG Movie"), ("mpeg", "MPEG Movie"), ("webm", "WebM Video"), ("flv", "Flash Video"),
    ("mp3", "MP3 Audio"), ("flac", "FLAC Audio"), ("m4a", "MPEG-4 Audio"), ("aac", "AAC Audio"),
    ("wav", "WAV Audio"), ("ogg", "Ogg Audio"), ("opus", "Opus Audio"), ("wma", "Windows Media Audio"),
    ("jpg", "JPEG Image"), ("jpeg", "JPEG Image"), ("png", "PNG Image"), ("gif", "GIF Image"),
    ("heic", "HEIF Image"), ("webp", "WebP Image"), ("tif", "TIFF Image"), ("tiff", "TIFF Image"),
    ("bmp", "BMP Image"), ("svg", "SVG Image"), ("raw", "Raw Image"), ("cr2", "Canon Raw Image"),
    ("nef", "Nikon Raw Image"), ("arw", "Sony Raw Image"), ("dng", "Digital Negative"), ("psd", "Photoshop Document"),
    ("srt", "Subtitles"), ("sub", "Subtitles"), ("ass", "Subtitles"), ("vtt", "Subtitles"),
    ("nfo", "Info File"), ("txt", "Plain Text"), ("log", "Log File"), ("md", "Markdown Document"),
    ("pdf", "PDF Document"), ("epub", "EPUB Book"), ("mobi", "Mobipocket Book"), ("azw3", "Kindle Book"),
    ("doc", "Word Document"), ("docx", "Word Document"), ("xls", "Excel Spreadsheet"),
    ("xlsx", "Excel Spreadsheet"), ("ppt", "PowerPoint Presentation"), ("pptx", "PowerPoint Presentation"),
    ("csv", "CSV Document"), ("json", "JSON Document"), ("xml", "XML Document"), ("yml", "YAML Document"),
    ("yaml", "YAML Document"), ("html", "HTML Document"), ("htm", "HTML Document"), ("css", "CSS Stylesheet"),
    ("js", "JavaScript"), ("py", "Python Script"), ("sh", "Shell Script"), ("php", "PHP Script"),
    ("c", "C Source"), ("h", "C Header"), ("cpp", "C++ Source"), ("rs", "Rust Source"), ("go", "Go Source"),
    ("zip", "ZIP Archive"), ("rar", "RAR Archive"), ("7z", "7-Zip Archive"), ("tar", "Tar Archive"),
    ("gz", "Gzip Archive"), ("tgz", "Gzip Archive"), ("bz2", "Bzip2 Archive"), ("xz", "XZ Archive"),
    ("zst", "Zstandard Archive"), ("iso", "Disc Image"), ("img", "Disk Image"), ("dmg", "Apple Disk Image"),
    ("qcow2", "QEMU Disk Image"), ("vmdk", "VMware Disk Image"), ("vdi", "VirtualBox Disk Image"),
    ("vhd", "Virtual Hard Disk"), ("vhdx", "Virtual Hard Disk"),
    ("db", "Database"), ("sqlite", "SQLite Database"), ("sqlite3", "SQLite Database"),
    ("ibd", "InnoDB Table"), ("frm", "MySQL Table Definition"), ("wal", "Write-Ahead Log"),
    ("bak", "Backup File"), ("tmp", "Temporary File"), ("part", "Partial Download"),
    ("torrent", "Torrent File"), ("par2", "Parity Archive"), ("exe", "Windows Executable"),
    ("dll", "Windows Library"), ("so", "Shared Library"), ("jar", "Java Archive"), ("apk", "Android Package"),
    ("deb", "Debian Package"), ("rpm", "RPM Package"), ("txz", "Slackware Package"), ("plg", "Unraid Plugin"),
    ("ttf", "TrueType Font"), ("otf", "OpenType Font"), ("woff", "Web Font"), ("woff2", "Web Font"),
    ("bif", "Video Thumbnails Index"), ("pack", "Git Pack"), ("idx", "Index File"), ("bin", "Binary Data"),
    ("dat", "Data File"), ("cfg", "Configuration File"), ("conf", "Configuration File"), ("ini", "Configuration File"),
];

struct State {
    by_ext: HashMap<Vec<u8>, u16>,
    by_name: HashMap<String, u16>,
    names: Vec<String>,
}

pub struct KindTable {
    state: Mutex<State>,
    known: HashMap<&'static str, &'static str>,
}

impl Default for KindTable {
    fn default() -> Self { Self::new() }
}

impl KindTable {
    pub fn new() -> Self {
        let names = ["Folder", "Free Space", "Other Space", "Symbolic Link", "Document",
                     "Unix Executable File", "Other"].iter().map(|s| s.to_string()).collect::<Vec<_>>();
        debug_assert_eq!(names.len(), FIRST_DYNAMIC);
        KindTable {
            state: Mutex::new(State { by_ext: HashMap::new(), by_name: HashMap::new(), names }),
            known: NAMES.iter().copied().collect(),
        }
    }

    /// A table for an index loaded from disk: the names in id order.
    pub fn with_names(names: Vec<String>) -> Self {
        let t = Self::new();
        {
            let mut st = t.state.lock().unwrap();
            if names.len() >= FIRST_DYNAMIC {
                for (i, n) in names.iter().enumerate().skip(FIRST_DYNAMIC) { st.by_name.insert(n.clone(), i as u16); }
                st.names = names;
            }
        }
        t
    }

    /// Lower-cased extension of `name`, if it has a plausible one.
    pub fn extension(name: &[u8]) -> Option<Vec<u8>> {
        let dot = name.iter().rposition(|&b| b == b'.')?;
        if dot == 0 { return None; }
        let ext = &name[dot + 1..];
        if ext.is_empty() || ext.len() > 16 || ext.contains(&b' ') { return None; }
        if ext.iter().all(u8::is_ascii_digit) { return None; }
        Some(ext.to_ascii_lowercase())
    }

    /// Kind of a regular file named `name`.
    #[cfg(test)]
    pub fn kind_for(&self, name: &[u8], is_exec: bool) -> u16 {
        match Self::extension(name) {
            None => if is_exec { EXECUTABLE } else { DOCUMENT },
            Some(ext) => self.kind_for_ext(&ext),
        }
    }

    pub fn kind_for_ext(&self, ext: &[u8]) -> u16 {
        let mut st = self.state.lock().unwrap();
        if let Some(&k) = st.by_ext.get(ext) { return k; }
        let text = String::from_utf8_lossy(ext).into_owned();
        let name = match self.known.get(text.as_str()) {
            Some(n) => n.to_string(),
            None => format!("{} File", text.to_uppercase()),
        };
        let id = match st.by_name.get(&name) {
            Some(&k) => k,
            None => {
                // u16 ids: anything past the limit collapses into "Document".
                if st.names.len() >= u16::MAX as usize - 1 { return DOCUMENT; }
                let k = st.names.len() as u16;
                st.names.push(name.clone());
                st.by_name.insert(name, k);
                k
            }
        };
        st.by_ext.insert(ext.to_vec(), id);
        id
    }

    pub fn name(&self, kind: u16) -> String {
        self.state.lock().unwrap().names.get(kind as usize).cloned().unwrap_or_else(|| "Other".into())
    }

    pub fn names(&self) -> Vec<String> { self.state.lock().unwrap().names.clone() }

    pub fn count(&self) -> usize { self.state.lock().unwrap().names.len() }
}

const DISTINCT: [[f32; 3]; 16] = [
    [0.20, 0.40, 1.00], [1.00, 0.25, 0.25], [0.25, 0.85, 0.30], [0.10, 0.85, 0.90],
    [0.90, 0.30, 0.90], [1.00, 0.85, 0.15], [0.55, 0.60, 1.00], [1.00, 0.60, 0.55],
    [0.60, 1.00, 0.60], [0.60, 1.00, 1.00], [1.00, 0.60, 1.00], [1.00, 1.00, 0.60],
    [1.00, 0.55, 0.10], [0.55, 0.35, 0.95], [0.40, 0.75, 0.55], [0.85, 0.65, 0.40],
];

/// The kinds using the most space get the distinct colours; the rest share muted greys.
/// `kinds_by_size` lists kind ids largest first.
pub fn palette(kinds_by_size: &[u16], count: usize) -> Vec<[f32; 3]> {
    let mut colors = vec![[0.6f32, 0.6, 0.6]; count.max(FIRST_DYNAMIC)];
    colors[FOLDER as usize] = [0.65, 0.65, 0.62];
    colors[FREE as usize] = [0.92, 0.92, 0.92];
    colors[OTHER as usize] = [0.45, 0.45, 0.45];
    let mut next = 0;
    let mut grey = 0.75f32;
    for &k in kinds_by_size {
        if k == FREE || k == OTHER || k == FOLDER || k as usize >= colors.len() { continue; }
        if next < DISTINCT.len() {
            colors[k as usize] = DISTINCT[next];
            next += 1;
        } else {
            colors[k as usize] = [grey, grey, grey];
            grey = (grey - 0.02).max(0.4);
        }
    }
    colors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_and_fixed_kinds() {
        let t = KindTable::new();
        assert_eq!(t.kind_for(b"README", false), DOCUMENT);
        assert_eq!(t.kind_for(b"run", true), EXECUTABLE);
        assert_eq!(t.kind_for(b".bashrc", false), DOCUMENT);
        // Rotated logs (syslog.1) and version suffixes are not file types.
        assert_eq!(t.kind_for(b"syslog.1", false), DOCUMENT);
        assert_eq!(t.kind_for(b"libfoo.so.12", false), DOCUMENT);
        assert_ne!(t.kind_for(b"archive.7z", false), DOCUMENT);
        let jpg = t.kind_for(b"a.JPG", false);
        assert_eq!(jpg, t.kind_for(b"b.jpeg", false));
        assert_eq!(t.name(jpg), "JPEG Image");
        let odd = t.kind_for(b"x.qqq", false);
        assert_eq!(t.name(odd), "QQQ File");
        assert_ne!(odd, jpg);
    }

    #[test]
    fn palette_gives_largest_kind_first_colour() {
        let p = palette(&[9, 8], 10);
        assert_eq!(p[9], DISTINCT[0]);
        assert_eq!(p[8], DISTINCT[1]);
        assert_eq!(p[FREE as usize], [0.92, 0.92, 0.92]);
    }
}
