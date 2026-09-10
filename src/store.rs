use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub const MAX_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_UPLOAD_BYTES: u64 = 10 * 1024 * 1024 * 1024;
/// 文字文件用内容开头命名，列表里一眼能认出是哪一段。
const TEXT_NAME_CHARS: usize = 28;

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub modified: u64,
    pub is_text: bool,
}

impl Store {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)
            .context("Could not create the data folder; move the app to a writable directory")?;
        if fs::symlink_metadata(&root)?.file_type().is_symlink() {
            bail!("The data folder must not be a symlink");
        }
        let root = root.canonicalize()?;
        // Fail at startup instead of accepting uploads into an unwritable directory.
        tempfile::Builder::new()
            .prefix(".lan-drop-")
            .tempfile_in(&root)
            .context("The data folder is not writable; move the app to a writable directory")?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn list(&self) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for item in fs::read_dir(&self.root)? {
            let item = item?;
            let name = item.file_name().to_string_lossy().into_owned();
            if validate_name(&name).is_err() || !item.file_type()?.is_file() {
                continue;
            }
            let Ok(meta) = item.metadata() else { continue };
            entries.push(Entry {
                is_text: name.to_ascii_lowercase().ends_with(".txt"),
                name,
                size: meta.len(),
                modified: meta
                    .modified()
                    .unwrap_or(UNIX_EPOCH)
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            });
        }
        entries.sort_by(|a, b| b.modified.cmp(&a.modified).then(a.name.cmp(&b.name)));
        Ok(entries)
    }

    pub fn temporary(&self) -> Result<tempfile::NamedTempFile> {
        Ok(tempfile::Builder::new()
            .prefix(".lan-drop-")
            .tempfile_in(&self.root)?)
    }

    pub fn commit(&self, mut temp: tempfile::NamedTempFile, name: &str) -> Result<String> {
        validate_name(name)?;
        let path = Path::new(name);
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        let extension = path
            .extension()
            .map(|x| format!(".{}", x.to_string_lossy()))
            .unwrap_or_default();
        for index in 0..10_000 {
            let candidate = if index == 0 {
                name.to_owned()
            } else {
                format!("{stem} ({index}){extension}")
            };
            match temp.persist_noclobber(self.root.join(&candidate)) {
                Ok(_) => return Ok(candidate),
                Err(err) if err.error.kind() == std::io::ErrorKind::AlreadyExists => {
                    temp = err.file
                }
                Err(err) => return Err(err.error.into()),
            }
        }
        bail!("Too many files with this name; rename it and try again")
    }

    pub fn import(&self, source: &Path) -> Result<String> {
        let name = source
            .file_name()
            .context("Invalid file name")?
            .to_str()
            .context("File names must be valid UTF-8")?;
        validate_name(name)?;
        let mut input = fs::File::open(source)?;
        if !input.metadata()?.is_file() {
            bail!("Please choose a file; compress folders first");
        }
        let mut temp = self.temporary()?;
        let size = std::io::copy(
            &mut Read::by_ref(&mut input).take(MAX_UPLOAD_BYTES + 1),
            &mut temp,
        )?;
        if size > MAX_UPLOAD_BYTES {
            bail!("A single file cannot exceed 10 GiB");
        }
        temp.as_file().sync_all()?;
        self.commit(temp, name)
    }

    pub fn save_text(&self, text: &str) -> Result<String> {
        if text.trim().is_empty() {
            bail!("Please enter some text");
        }
        if text.len() > MAX_TEXT_BYTES {
            bail!("Text cannot exceed 1 MiB");
        }
        let name = text_file_name(text);
        let mut temp = self.temporary()?;
        temp.write_all(text.as_bytes())?;
        temp.as_file().sync_all()?;
        self.commit(temp, &name)
    }

    pub fn open(&self, name: &str) -> Result<fs::File> {
        validate_name(name)?;
        let path = self.root.join(name);
        if !fs::symlink_metadata(&path)?.is_file() {
            bail!("Not a regular file");
        }
        let mut options = fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
        }
        let file = options.open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            bail!("Not a regular file");
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if meta.file_attributes() & 0x400 != 0 {
                bail!("Reparse points are not allowed");
            }
        }
        Ok(file)
    }

    pub fn read_text(&self, name: &str) -> Result<String> {
        let file = self.open(name)?;
        if file.metadata()?.len() > MAX_TEXT_BYTES as u64 {
            bail!("This text is over 1 MiB; download it to read");
        }
        let mut text = String::new();
        file.take((MAX_TEXT_BYTES + 1) as u64)
            .read_to_string(&mut text)?;
        if text.len() > MAX_TEXT_BYTES {
            bail!("This text is over 1 MiB; download it to read");
        }
        Ok(text)
    }
}

/// 取内容开头的 TEXT_NAME_CHARS 个字符做文件名；换行和文件名里不能出现的字符换成空格。
/// 内容开头没有可用字符（纯符号、纯空白）或撞上保留名称时，退回时间戳命名。
fn text_file_name(text: &str) -> String {
    let stem: String = text
        .trim_start()
        .chars()
        .take(TEXT_NAME_CHARS)
        .map(|c| {
            if c.is_control() || "/\\:*?\"<>|".contains(c) {
                ' '
            } else {
                c
            }
        })
        .collect();
    let name = format!(
        "{}.txt",
        stem.trim_matches(|c: char| c == '.' || c.is_whitespace())
    );
    if validate_name(&name).is_ok() {
        return name;
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("text-{stamp}.txt")
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 200
        || name.starts_with('.')
        || name.ends_with(['.', ' '])
        || name
            .chars()
            .any(|c| c.is_control() || "/\\:*?\"<>|".contains(c))
    {
        bail!("Invalid file name: hidden files, paths and special characters are not supported");
    }
    let base = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    if ["CON", "PRN", "AUX", "NUL"].contains(&base.as_str())
        || (base.len() == 4
            && (base.starts_with("COM") || base.starts_with("LPT"))
            && matches!(base.as_bytes()[3], b'1'..=b'9'))
    {
        bail!("This name is reserved on Windows");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn names_are_portable_and_cannot_escape() {
        for name in [
            "",
            "..",
            "../secret",
            "a/b",
            "a\\b",
            "x:y",
            ".lan-drop-x",
            "a\n",
            "a.",
            "a ",
            "CON.txt",
            "LPT1",
        ] {
            assert!(validate_name(name).is_err(), "{name}");
        }
        for name in ["你好.txt", "photo 1.png", "report.tar.gz"] {
            assert!(validate_name(name).is_ok());
        }
    }
    #[test]
    fn concurrent_saves_never_overwrite_and_temps_are_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned()).unwrap();
        let mut pending = store.temporary().unwrap();
        pending.write_all(b"partial").unwrap();
        assert!(store.list().unwrap().is_empty());
        std::thread::scope(|scope| {
            for i in 0..12 {
                let store = &store;
                scope.spawn(move || {
                    let mut temp = store.temporary().unwrap();
                    write!(temp, "{i}").unwrap();
                    store.commit(temp, "same.txt").unwrap();
                });
            }
        });
        let entries = store.list().unwrap();
        assert_eq!(entries.len(), 12);
        let texts: std::collections::HashSet<_> = entries
            .iter()
            .map(|e| store.read_text(&e.name).unwrap())
            .collect();
        assert_eq!(texts.len(), 12);
    }
    #[test]
    fn text_roundtrip_and_limits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned()).unwrap();
        let content = "你好 🌿\n<script>alert(1)</script>";
        let name = store.save_text(content).unwrap();
        assert_eq!(store.read_text(&name).unwrap(), content);
        assert!(store.save_text("  \n").is_err());
        assert!(store.save_text(&"x".repeat(MAX_TEXT_BYTES + 1)).is_err());
    }
    #[test]
    fn text_is_named_after_its_first_characters() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().to_owned()).unwrap();
        assert_eq!(
            store.save_text("Meeting notes").unwrap(),
            "Meeting notes.txt"
        );
        // 同样的开头不会互相覆盖。
        assert_eq!(
            store.save_text("Meeting notes").unwrap(),
            "Meeting notes (1).txt"
        );
        // 文件名跟着内容走，可以是任何语言。
        assert_eq!(store.save_text("会议纪要").unwrap(), "会议纪要.txt");
        // 超过 28 个字符只留开头，换行与非法字符换成空格。
        assert_eq!(
            store
                .save_text("  First line\nhttps://example.com/a/b and a lot more")
                .unwrap(),
            "First line https   example.c.txt"
        );
        // 开头没有可用字符时退回时间戳。
        let fallback = store.save_text("...\n").unwrap();
        assert!(
            fallback.starts_with("text-") && fallback.ends_with(".txt"),
            "{fallback}"
        );
        assert!(store.save_text("CON").unwrap().starts_with("text-"));
        for entry in store.list().unwrap() {
            assert!(validate_name(&entry.name).is_ok(), "{}", entry.name);
        }
    }
    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_shared() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path().join("data")).unwrap();
        fs::write(dir.path().join("secret"), "secret").unwrap();
        std::os::unix::fs::symlink(dir.path().join("secret"), store.root().join("leak.txt"))
            .unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(store.open("leak.txt").is_err());
    }
}
