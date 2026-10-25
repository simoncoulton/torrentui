//! Tags for categorising torrents. Pure parsing and a small YAML store; the
//! app holds the map and the event loop decides when to save it.
//!
//! Tags are keyed by info-hash, not by the session's numeric torrent id (ids
//! are reassigned on restart). The store lives next to the sessions, not in
//! them, because the engine owns and rewrites the session files.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Result, WrapErr, eyre};

use crate::config;

/// info-hash (lowercase hex) -> sorted, de-duplicated tags.
pub type Tags = BTreeMap<String, Vec<String>>;

pub const MAX_TAG_LEN: usize = 32;
pub const MAX_TAGS: usize = 16;

const HEADER: &str = "\
# torrentui tags, keyed by info-hash. Edited from inside the app (T).
# Tags are lowercase letters, digits and - _ . /
";

/// `$XDG_DATA_HOME/torrentui/tags.yml` (`~/.local/share/torrentui/tags.yml`).
pub fn default_path() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .ok_or_else(|| eyre!("cannot determine your data directory"))?
        .join("torrentui")
        .join("tags.yml"))
}

fn normalize_tag(raw: &str) -> Result<String, String> {
    let tag = raw.strip_prefix('#').unwrap_or(raw).to_lowercase();
    if tag.is_empty() {
        return Err("a tag can't be just '#'".into());
    }
    if tag.chars().count() > MAX_TAG_LEN {
        return Err(format!(
            "tag '{tag}' is longer than {MAX_TAG_LEN} characters"
        ));
    }
    if let Some(bad) = tag
        .chars()
        .find(|c| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')))
    {
        return Err(format!(
            "tag '{tag}' contains '{bad}'; use letters, digits and - _ . /"
        ));
    }
    Ok(tag)
}

/// Tags from user text: separated by commas and/or whitespace, an optional
/// leading `#` is ignored. The result is lowercase, sorted and de-duplicated.
/// Empty input means "no tags".
pub fn parse_tags(text: &str) -> Result<Vec<String>, String> {
    let mut set = BTreeSet::new();
    for token in text
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
    {
        set.insert(normalize_tag(token)?);
    }
    if set.len() > MAX_TAGS {
        return Err(format!("at most {MAX_TAGS} tags per torrent"));
    }
    Ok(set.into_iter().collect())
}

/// Load the store. A missing file is an empty store; a malformed one is an
/// error (rather than silently starting empty and overwriting it later).
pub fn load(path: &Path) -> Result<Tags> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Tags::new()),
        Err(e) => return Err(e).wrap_err_with(|| format!("cannot read {}", path.display())),
    };
    let raw: Option<BTreeMap<String, Vec<String>>> = serde_yaml_ng::from_str(&text)
        .wrap_err_with(|| format!("invalid tags file {}", path.display()))?;
    let mut tags = Tags::new();
    for (hash, list) in raw.unwrap_or_default() {
        let list = parse_tags(&list.join(" "))
            .map_err(|e| eyre!("{e} (torrent {hash} in {})", path.display()))?;
        if !list.is_empty() {
            tags.insert(hash, list);
        }
    }
    Ok(tags)
}

pub fn save(path: &Path, tags: &Tags) -> Result<()> {
    let nonempty: BTreeMap<&String, &Vec<String>> =
        tags.iter().filter(|(_, v)| !v.is_empty()).collect();
    let body = if nonempty.is_empty() {
        String::new()
    } else {
        serde_yaml_ng::to_string(&nonempty)?
    };
    config::write_atomic(path, &format!("{HEADER}{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("torrentui-tags-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn parses_commas_spaces_hashes_and_case() {
        assert_eq!(
            parse_tags("Movies, #sci-fi  movies\t4k").unwrap(),
            ["4k", "movies", "sci-fi"]
        );
        assert_eq!(parse_tags("").unwrap(), Vec::<String>::new());
        assert_eq!(parse_tags(" , ,").unwrap(), Vec::<String>::new());
        assert_eq!(parse_tags("tv/shows a_b c.d").unwrap().len(), 3);
        assert_eq!(parse_tags("Ünï").unwrap(), ["ünï"]);
    }

    #[test]
    fn rejects_bad_tags_with_a_reason() {
        assert!(parse_tags("a!b").unwrap_err().contains("'!'"));
        assert!(parse_tags("#").unwrap_err().contains("just '#'"));
        assert!(
            parse_tags(&"x".repeat(33))
                .unwrap_err()
                .contains("longer than")
        );
        let many: Vec<String> = (0..17).map(|i| format!("t{i}")).collect();
        assert!(
            parse_tags(&many.join(" "))
                .unwrap_err()
                .contains("at most 16")
        );
        assert!(parse_tags(&"x".repeat(32)).is_ok());
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = scratch("missing");
        assert!(load(&dir.join("tags.yml")).unwrap().is_empty());
    }

    #[test]
    fn round_trips_including_numeric_looking_hashes() {
        let dir = scratch("roundtrip");
        let path = dir.join("nested").join("tags.yml");
        let mut tags = Tags::new();
        tags.insert(
            "1234567890123456789012345678901234567890".into(),
            vec!["a".into()],
        );
        tags.insert(
            "12345e6789012345678901234567890123456789".into(),
            vec!["b".into(), "c".into()],
        );
        tags.insert(
            "e67f5549f263c88053c57ae1f898e79258d52932".into(),
            vec!["movies".into()],
        );
        tags.insert("0000000000000000000000000000000000000000".into(), vec![]);
        save(&path, &tags).unwrap();

        let loaded = load(&path).unwrap();
        tags.remove("0000000000000000000000000000000000000000"); // empty lists aren't stored
        assert_eq!(loaded, tags);
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .starts_with("# torrentui tags")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_store_round_trips_and_empty_file_loads() {
        let dir = scratch("empty");
        let path = dir.join("tags.yml");
        save(&path, &Tags::new()).unwrap();
        assert!(load(&path).unwrap().is_empty());
        fs::write(&path, "").unwrap();
        assert!(load(&path).unwrap().is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn malformed_or_invalid_files_are_errors_that_name_the_file() {
        let dir = scratch("bad");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tags.yml");

        fs::write(&path, "abc: [unclosed").unwrap();
        assert!(format!("{:#}", load(&path).unwrap_err()).contains("tags.yml"));

        fs::write(&path, "abc:\n  - \"bad tag!\"\n").unwrap();
        let e = format!("{:#}", load(&path).unwrap_err());
        assert!(e.contains("'!'") && e.contains("abc"), "{e}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hand_edited_tags_are_normalised_on_load() {
        let dir = scratch("normalise");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tags.yml");
        fs::write(&path, "abc:\n  - Movies\n  - movies\n  - TV\n").unwrap();
        assert_eq!(load(&path).unwrap()["abc"], ["movies", "tv"]);
        let _ = fs::remove_dir_all(&dir);
    }
}
