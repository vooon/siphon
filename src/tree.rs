//! Mapping between S3 object keys and paths inside the pxar archive.
//!
//! A key `a/b/c` becomes the file `c` in directory `a/b`. Keys that cannot be
//! represented that way (empty, `.` or `..` components, components longer than
//! a file name may be, a key that is also a "directory" of other keys, keys
//! under our own reserved directory) are stored flat under
//! `.siphon/keys/<sha256 of key>`, with the real key in the `user.s3.key` xattr.

use std::collections::BTreeMap;
use std::time::SystemTime;

/// Top-level directory reserved for siphon's own entries.
pub const RESERVED_DIR: &str = ".siphon";
/// Sub-directory of [`RESERVED_DIR`] holding escaped keys.
pub const ESCAPED_DIR: &str = "keys";

const MAX_NAME_LEN: usize = 255;

/// One object from the bucket listing.
#[derive(Clone, Debug, PartialEq)]
pub struct Object {
    pub key: String,
    pub size: u64,
    pub last_modified: SystemTime,
}

/// A file in the archive tree.
#[derive(Clone, Debug, PartialEq)]
pub struct FileNode {
    pub object: Object,
    /// Stored under [`ESCAPED_DIR`]; restore must use the `user.s3.key` xattr.
    pub escaped: bool,
}

#[derive(Debug, PartialEq)]
pub enum Node {
    File(FileNode),
    Dir(Dir),
}

#[derive(Debug, Default, PartialEq)]
pub struct Dir {
    pub children: BTreeMap<String, Node>,
}

impl Dir {
    /// Latest modification time of anything below this directory.
    pub fn mtime(&self) -> Option<SystemTime> {
        self.children
            .values()
            .filter_map(|node| match node {
                Node::File(f) => Some(f.object.last_modified),
                Node::Dir(d) => d.mtime(),
            })
            .max()
    }

    /// Number of files below this directory.
    #[cfg(test)]
    pub fn file_count(&self) -> usize {
        self.children
            .values()
            .map(|node| match node {
                Node::File(_) => 1,
                Node::Dir(d) => d.file_count(),
            })
            .sum()
    }
}

/// Split a key into path components, or `None` if it has to be escaped.
fn components(key: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = key.split('/').collect();
    let valid = parts
        .iter()
        .all(|p| !p.is_empty() && *p != "." && *p != ".." && p.len() <= MAX_NAME_LEN);
    if !valid || parts[0] == RESERVED_DIR {
        return None;
    }
    Some(parts)
}

/// File name for an escaped key.
pub fn escaped_name(key: &str) -> String {
    hex::encode(openssl::sha::sha256(key.as_bytes()))
}

/// Build the archive tree from a bucket listing.
pub fn build(objects: impl IntoIterator<Item = Object>) -> Dir {
    let mut root = Dir::default();
    let mut escaped = Vec::new();

    for object in objects {
        let key = object.key.clone();
        match components(&key) {
            Some(parts) => {
                if let Some(displaced) = insert(&mut root, &parts, object) {
                    escaped.push(displaced);
                }
            }
            None => escaped.push(object),
        }
    }

    if !escaped.is_empty() {
        let mut keys = Dir::default();
        for object in escaped {
            keys.children.insert(
                escaped_name(&object.key),
                Node::File(FileNode {
                    object,
                    escaped: true,
                }),
            );
        }
        let mut reserved = Dir::default();
        reserved
            .children
            .insert(ESCAPED_DIR.to_string(), Node::Dir(keys));
        root.children
            .insert(RESERVED_DIR.to_string(), Node::Dir(reserved));
    }

    root
}

/// Insert `object` at `parts`. Returns an object that could not stay in the
/// tree because a file and a directory want the same name; the file loses.
fn insert(dir: &mut Dir, parts: &[&str], object: Object) -> Option<Object> {
    let (name, rest) = parts.split_first().expect("at least one component");

    if rest.is_empty() {
        return match dir.children.get(*name) {
            Some(Node::Dir(_)) => Some(object),
            Some(Node::File(_)) => unreachable!("duplicate key {}", object.key),
            None => {
                dir.children.insert(
                    name.to_string(),
                    Node::File(FileNode {
                        object,
                        escaped: false,
                    }),
                );
                None
            }
        };
    }

    let mut displaced = None;
    if let Some(Node::File(_)) = dir.children.get(*name)
        && let Some(Node::File(file)) = dir.children.remove(*name)
    {
        displaced = Some(file.object);
    }
    let child = dir
        .children
        .entry(name.to_string())
        .or_insert_with(|| Node::Dir(Dir::default()));
    let Node::Dir(child) = child else {
        unreachable!()
    };
    let nested = insert(child, rest, object);
    // At most one of them can be set: a file is displaced only on the way down.
    displaced.or(nested)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn obj(key: &str, secs: u64) -> Object {
        Object {
            key: key.to_string(),
            size: 1,
            last_modified: SystemTime::UNIX_EPOCH + Duration::from_secs(secs),
        }
    }

    fn file<'a>(dir: &'a Dir, path: &str) -> &'a FileNode {
        let mut dir = dir;
        let parts: Vec<&str> = path.split('/').collect();
        for p in &parts[..parts.len() - 1] {
            match dir.children.get(*p) {
                Some(Node::Dir(d)) => dir = d,
                other => panic!("{p} is not a dir: {other:?}"),
            }
        }
        match dir.children.get(*parts.last().unwrap()) {
            Some(Node::File(f)) => f,
            other => panic!("{path} is not a file: {other:?}"),
        }
    }

    fn escaped<'a>(root: &'a Dir, key: &str) -> &'a FileNode {
        let f = file(
            root,
            &format!("{RESERVED_DIR}/{ESCAPED_DIR}/{}", escaped_name(key)),
        );
        assert!(f.escaped);
        assert_eq!(f.object.key, key);
        f
    }

    #[test]
    fn plain_keys_become_paths() {
        let root = build([obj("a/b/c", 1), obj("a/d", 2), obj("e", 3)]);
        assert_eq!(file(&root, "a/b/c").object.key, "a/b/c");
        assert!(!file(&root, "a/d").escaped);
        assert_eq!(file(&root, "e").object.key, "e");
        assert!(!root.children.contains_key(RESERVED_DIR));
        assert_eq!(root.file_count(), 3);
    }

    #[test]
    fn unrepresentable_keys_are_escaped() {
        let long = "x".repeat(MAX_NAME_LEN + 1);
        let keys = [
            "dir/",
            "/leading",
            "a//b",
            "./dot",
            "a/../b",
            ".siphon/keys/x",
            long.as_str(),
        ];
        let root = build(keys.iter().map(|k| obj(k, 1)));
        for key in keys {
            escaped(&root, key);
        }
        assert_eq!(root.file_count(), keys.len());
    }

    #[test]
    fn file_and_dir_with_same_name() {
        // listing order: "a" sorts before "a/b"
        let root = build([obj("a", 1), obj("a/b", 2)]);
        assert_eq!(file(&root, "a/b").object.key, "a/b");
        escaped(&root, "a");

        // and the other way round
        let root = build([obj("a/b", 2), obj("a", 1)]);
        assert_eq!(file(&root, "a/b").object.key, "a/b");
        escaped(&root, "a");
    }

    #[test]
    fn dir_mtime_is_latest_child() {
        let root = build([obj("a/b", 5), obj("a/c/d", 9), obj("e", 1)]);
        let Some(Node::Dir(a)) = root.children.get("a") else {
            panic!()
        };
        assert_eq!(
            a.mtime(),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(9))
        );
    }
}
