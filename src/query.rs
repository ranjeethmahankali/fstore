use crate::{
    core::{get_relative_path, Error},
    filter::{Filter, TagMaker},
    load::{
        get_filename_str, get_store_path, implicit_tags_str, DirData, FileLoadingOptions,
        GlobMatches, Loader, LoaderOptions,
    },
    walk::WalkDirectories,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use itertools::Itertools;

/// Tries to set the flag at the given index. If the index is outside the bounds
/// of the vector, the vector is automatically resized. That is what makes it
/// 'safe'.
fn safe_set_flag(flags: &mut Vec<bool>, index: usize) {
    if index >= flags.len() {
        flags.resize(index + 1, false);
    }
    flags[index] = true;
}

/// Read the flag at the given index in the slice. If the index is outside the
/// bounds false is returned safely.
pub(crate) fn safe_get_flag(flags: &[bool], index: usize) -> bool {
    *flags.get(index).unwrap_or(&false)
}

/*
When a tags are specified for a folder, it's subfolders and all their subfolders
inherit those tags. This inheritance follows the directory tree. When
recursively loading all the data starting from a root directory, we have to keep
track of inherited tags. We do this by pushing all the tags into a vector, and
by storing the offsets that separate contiguous chunks of this vector across the
depth-first chain of directories currently being traversed.
 */
struct InheritedTags {
    /// Indices of currently loaded tags.
    tag_indices: Vec<usize>,
    /// Offsets that separate the tags across the depth-first chain of directories currently being traversed.
    offsets: Vec<usize>,
    /// Current depth of the traversal.
    depth: usize,
}

impl InheritedTags {
    /// Update the inherited tags for the specified `newdepth`. A new depth that
    /// is 1 more than the current depth implies traversing deeper into the
    /// directory tree. A new depth that is smaller than the current depth
    /// implies popping all the tags inherited from folders deeper than the new
    /// depth.
    fn update(&mut self, newdepth: usize) -> Result<(), Error> {
        if self.depth + 1 == newdepth {
            self.offsets.push(self.tag_indices.len());
        } else if self.depth >= newdepth {
            let mut marker = self.tag_indices.len();
            for _ in 0..(self.depth + 1 - newdepth) {
                marker = self.offsets.pop().ok_or(Error::DirectoryTraversalFailed)?;
            }
            self.tag_indices.truncate(marker);
            self.offsets.push(marker);
        } else {
            return Err(Error::DirectoryTraversalFailed);
        }
        self.depth = newdepth;
        Ok(())
    }
}

/// Table of all files and tags, that can be loaded recursively from any path.
pub(crate) struct TagTable {
    root: PathBuf,
    tag_index_map: HashMap<String, usize>,
    table: HashMap<PathBuf, Vec<bool>>,
}

impl TagTable {
    fn query(&self, filter: Filter<usize>) -> impl Iterator<Item = std::path::Display<'_>> {
        // Filter before sorting so we're doing a little less work
        let filtered = self.table.iter().filter(move |(_, flags)| {
            filter.eval(flags)
        });
        let sorted = Itertools::sorted(filtered);
        sorted.map(move |(path, _)| {
            path.display()
        })
    }

    /// Create a new tag table by recursively traversing directories
    /// from `dirpath`.
    pub(crate) fn from_dir(dirpath: PathBuf) -> Result<Self, Error> {
        // These structs are locally defined because their only
        // purpose is to use with serde_yaml to extract relevant
        // information from the YAML files.
        let mut table = TagTable {
            root: dirpath,
            tag_index_map: HashMap::new(),
            table: HashMap::new(),
        };
        let mut num_tags: usize = 0;
        let mut inherited = InheritedTags {
            tag_indices: Vec::new(),
            offsets: Vec::new(),
            depth: 0,
        };
        let rootdir = table.root.clone(); // We'll need this copy later.
        let mut walker = WalkDirectories::from(table.root.clone())?;
        let mut gmatcher = GlobMatches::new();
        let mut filetags: Vec<String> = Vec::new();
        let mut loader = Loader::new(LoaderOptions::new(
            true,
            false,
            FileLoadingOptions::Load {
                file_tags: true,
                file_desc: false,
            },
        ));
        while let Some((depth, curpath, children)) = walker.next() {
            inherited.update(depth)?;
            // Deserialize yaml without copy.
            let DirData {
                tags,
                files,
                desc: _,
            } = {
                match get_store_path::<true>(curpath) {
                    Some(path) => loader.load(&path)?,
                    None => continue,
                }
            };
            // Push directory tags.
            for tag in tags
                .iter()
                .map(|t| t.to_string())
                .chain(implicit_tags_str(get_filename_str(curpath)?))
            {
                inherited.tag_indices.push(Self::get_tag_index(
                    tag,
                    &mut table.tag_index_map,
                    &mut num_tags,
                ));
            }
            // Process all files in the directory.
            gmatcher.find_matches(children, &files, false);
            for (ci, cpath) in children
                .iter()
                .filter_map(|ch| get_relative_path(curpath, ch.name(), &rootdir))
                .enumerate()
            {
                filetags.clear();
                let mut found: bool = false;
                for fi in gmatcher.matched_globs(ci) {
                    found = true;
                    filetags.extend(files[fi].tags.iter().map(|t| t.to_string()));
                    filetags.extend(implicit_tags_str(get_filename_str(&cpath)?));
                }
                if found {
                    table.add_file(cpath, &mut filetags, &mut num_tags, &inherited.tag_indices);
                }
            }
        }
        Ok(table)
    }

    fn get_tag_index(tag: String, map: &mut HashMap<String, usize>, counter: &mut usize) -> usize {
        let size = map.len();
        let entry = *(map.entry(tag.to_string()).or_insert(size));
        *counter = map.len();
        entry
    }

    fn add_file(
        &mut self,
        path: PathBuf,
        tags: &mut Vec<String>,
        num_tags: &mut usize,
        inherited: &Vec<usize>,
    ) {
        let flags = self.table.entry(path).or_default();
        // Set the file's explicit tags.
        flags.reserve(flags.len() + tags.len());
        for tag in tags.drain(..) {
            safe_set_flag(
                flags,
                Self::get_tag_index(tag, &mut self.tag_index_map, num_tags),
            );
        }
        // Set inherited tags.
        for i in inherited {
            safe_set_flag(flags, *i);
        }
    }
}

impl TagMaker<usize> for TagTable {
    /// Return the index of the given string tag from the list of parsed tags.
    fn create_tag(&self, tagstr: &str) -> Filter<usize> {
        match self.tag_index_map.get(tagstr) {
            Some(i) => Filter::Tag(*i),
            None => Filter::FalseTag,
        }
    }
}

/// Returns the number of files and the number of tags.
pub fn count_files_tags(path: PathBuf) -> Result<(usize, usize), Error> {
    let tt = TagTable::from_dir(path)?;
    Ok((tt.table.len(), tt.tag_index_map.len()))
}

pub fn run_query(dirpath: PathBuf, filter: &str) -> Result<(), Error> {
    let table = TagTable::from_dir(dirpath)?;
    let filter = Filter::<usize>::parse(filter, &table).map_err(Error::InvalidFilter)?;
    for path in table.query(filter) {
        println!("{}", path);
    }
    Ok(())
}

/// 2d array of bools.
pub(crate) struct BoolTable {
    data: Box<[bool]>, // Cannot be resized by accident.
    ncols: usize,
}

impl BoolTable {
    pub fn new(nrows: usize, ncols: usize) -> Self {
        BoolTable {
            data: vec![false; nrows * ncols].into_boxed_slice(),
            ncols,
        }
    }

    pub fn row(&self, r: usize) -> &[bool] {
        let start = r * self.ncols;
        &self.data[start..(start + self.ncols)]
    }

    pub fn row_mut(&mut self, r: usize) -> &mut [bool] {
        let start = r * self.ncols;
        &mut self.data[start..(start + self.ncols)]
    }
}

/// This is similar to a `TagTable`, but the flags indicating in which
/// file has which tags are stored in a dense 2d array rather than a
/// sparse hash-map of vectors.
pub struct DenseTagTable {
    root: PathBuf,
    flags: BoolTable,
    files: Box<[String]>,
    tags: Box<[String]>,
    tag_indices: HashMap<String, usize>,
}

impl DenseTagTable {
    pub fn from_dir(dirpath: PathBuf) -> Result<DenseTagTable, Error> {
        let TagTable {
            root,
            tag_index_map: tag_indices,
            table: sparse,
        } = TagTable::from_dir(dirpath)?;
        let tags: Box<[String]> = {
            let mut pairs: Vec<_> = tag_indices.iter().collect();
            pairs.sort_by(|(_t1, i1), (_t2, i2)| i1.cmp(i2));
            pairs.into_iter().map(|(t, _i)| t.clone()).collect()
        };
        let (files, flags) = {
            let mut pairs: Vec<_> = sparse
                .into_iter()
                .map(|(p1, f1)| (format!("{}", p1.display()), f1))
                .collect();
            pairs.sort_by(|(path1, _flags1), (path2, _flags2)| path1.cmp(path2));
            let (files, flags): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
            let mut dense = BoolTable::new(files.len(), tags.len());
            for (i, src) in flags.iter().enumerate() {
                debug_assert!(tags.len() >= src.len());
                let dst = dense.row_mut(i);
                let dst = &mut dst[..src.len()];
                dst.copy_from_slice(src); // Requires the src and dst to be of same length.
            }
            (files.into_boxed_slice(), dense)
        };
        Ok(DenseTagTable {
            root,
            flags,
            files,
            tags,
            tag_indices,
        })
    }

    pub fn path(&self) -> &Path {
        &self.root
    }

    pub fn flags(&self, row: usize) -> &[bool] {
        self.flags.row(row)
    }

    pub fn tags(&self) -> &[String] {
        &self.tags
    }

    pub fn files(&self) -> &[String] {
        &self.files
    }
}

impl TagMaker<usize> for DenseTagTable {
    /// Return the index of the given string tag from the list of parsed tags.
    fn create_tag(&self, input: &str) -> Filter<usize> {
        match self.tag_indices.get(input) {
            Some(i) => Filter::Tag(*i),
            None => Filter::FalseTag,
        }
    }
}
