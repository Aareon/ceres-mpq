use std::cmp::min;
use std::io::Error as IoError;
use std::io::{Seek, SeekFrom, Write};

use byteorder::{WriteBytesExt, LE};
use indexmap::IndexMap;

use super::archive::MpqReader;
use super::consts::*;
use super::crypto::*;
use super::header::*;
use super::tables::*;
use super::util::*;

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
struct FileKey {
    hash_a: u32,
    hash_b: u32,
    index: u32,
}

impl FileKey {
    fn new(name: &str) -> FileKey {
        let hash_a = hash_string_noslash(name.as_bytes(), MPQ_HASH_NAME_A);
        let hash_b = hash_string_noslash(name.as_bytes(), MPQ_HASH_NAME_B);
        let index = hash_string_noslash(name.as_bytes(), MPQ_HASH_TABLE_INDEX);

        FileKey {
            hash_a,
            hash_b,
            index,
        }
    }
}

#[derive(Debug)]
struct FileRecord {
    file_name: String,
    contents: Vec<u8>,
    offset: Option<u64>,
    options: FileOptions,
}

impl FileRecord {
    fn new<S: Into<String>, C: Into<Vec<u8>>>(
        name: S,
        contents: C,
        options: FileOptions,
    ) -> FileRecord {
        FileRecord {
            file_name: name.into(),
            contents: contents.into(),
            offset: None,
            options,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FileOptions {
    pub encrypt: bool,
    pub compress: bool,
    pub adjust_key: bool,
}

#[derive(Debug)]
// TODO: Add documentation
// TODO: Add support for multiple compression types
pub struct MpqBuilder {
    added_files: IndexMap<FileKey, FileRecord>,

    sector_size: u64,
}

impl Default for MpqBuilder {
    fn default() -> MpqBuilder {
        MpqBuilder {
            added_files: IndexMap::new(),
            sector_size: 0x10000,
        }
    }
}

impl MpqBuilder {
    pub fn add_file<C>(&mut self, file_name: &str, contents: C, options: FileOptions)
    where
        C: Into<Vec<u8>>,
    {
        let key = FileKey::new(file_name);

        self.added_files
            .entry(key)
            .or_insert_with(|| FileRecord::new(file_name, contents, options));
    }

    /// Writes out the entire archive to the specified writer.
    ///
    /// The archive start position is calculated as follows:
    /// `((current_pos + (HEADER_BOUNDARY - 1)) / HEADER_BOUNDARY) * HEADER_BOUNDARY`
    /// Where `current_pos` is the writer's current seek pos, and `HEADER_BOUNDARY` is 512.
    ///
    /// Will write the following:
    /// - MPQ Header
    /// - All files with their sector offset table
    /// - MPQ hash table
    /// - MPQ block table
    pub fn write<W>(self, mut writer: W) -> Result<(), IoError>
    where
        W: Write + Seek,
    {
        let (mut added_files, sector_size) = match self {
            MpqBuilder {
                added_files,
                sector_size,
            } => (added_files, sector_size),
        };

        let current_pos = writer.seek(SeekFrom::Current(0))?;
        // starting from the current pos, this will find the closest valid header position
        let archive_start =
            ((current_pos + (HEADER_BOUNDARY - 1)) / HEADER_BOUNDARY) * HEADER_BOUNDARY;
        writer.seek(SeekFrom::Start(archive_start))?;

        // skip writing the header for now
        writer.seek(SeekFrom::Current(HEADER_MPQ_SIZE as i64))?;

        // create a listfile
        let mut listfile = String::new();
        for file in added_files.values() {
            listfile += &file.file_name.replace("\\", "/");
            listfile += "\r\n";
        }

        // add it to the file list
        {
            let key = FileKey::new("(listfile)");
            added_files.insert(
                key,
                FileRecord::new(
                    "(listfile)",
                    listfile,
                    FileOptions {
                        compress: true,
                        encrypt: true,
                        adjust_key: true,
                    },
                ),
            );
        }

        // write out all the files back-to-back, and store their offsets and sizes
        let mut file_offsets = Vec::with_capacity(added_files.len());
        let mut file_sizes = Vec::with_capacity(added_files.len());
        for file in added_files.values() {
            let (offset, compressed_size) =
                write_file(sector_size, archive_start, &mut writer, &file)?;
            file_offsets.push(offset);
            file_sizes.push(compressed_size);
        }

        let mut hashtable_size = MIN_HASH_TABLE_SIZE;
        while hashtable_size < added_files.len() {
            hashtable_size *= 2;
        }

        // write hash table and remember its position
        let hashtable_pos = {
            let hashtable_pos = writer.seek(SeekFrom::Current(0))?;
            let mut hashtable = vec![HashTableEntry::blank(); hashtable_size];
            let hash_index_mask = hashtable_size - 1;

            for (block_index, (key, _)) in added_files.iter().enumerate() {
                let hash_index = (key.index as usize) & hash_index_mask;
                let hash_entry = HashTableEntry::new(key.hash_a, key.hash_b, block_index as u32);

                hashtable[hash_index] = hash_entry;
            }

            let mut buf = vec![0u8; hashtable_size * HASH_TABLE_ENTRY_SIZE as usize];

            let mut cursor = buf.as_mut_slice();
            for entry in hashtable {
                entry.write(&mut cursor)?;
            }
            encrypt_mpq_block(&mut buf, HASH_TABLE_KEY);

            writer.write_all(&buf)?;

            hashtable_pos
        };

        // write block table and remember its position
        let blocktable_pos = {
            let blocktable_pos = writer.seek(SeekFrom::Current(0))?;

            let mut buf = vec![0u8; added_files.len() * BLOCK_TABLE_ENTRY_SIZE as usize];

            let mut cursor = buf.as_mut_slice();
            for (block_index, (_, file)) in added_files.iter().enumerate() {
                let mut flags = MPQ_FILE_EXISTS;

                if file.options.encrypt {
                    flags |= MPQ_FILE_ENCRYPTED;
                }

                if file.options.adjust_key {
                    flags |= MPQ_FILE_ADJUST_KEY;
                }

                if file.options.compress {
                    flags |= MPQ_FILE_COMPRESS;
                }

                let block_entry = BlockTableEntry::new(
                    file_offsets[block_index] - archive_start,
                    file_sizes[block_index],
                    file.contents.len() as u64,
                    flags,
                );

                block_entry.write(&mut cursor)?;
            }

            encrypt_mpq_block(&mut buf, BLOCK_TABLE_KEY);
            writer.write_all(&buf)?;

            blocktable_pos
        };

        let archive_end = writer.seek(SeekFrom::Current(0))?;

        {
            let header = MpqFileHeader::new_v1(
                (archive_end - archive_start) as u32,
                sector_size as u32,
                (hashtable_pos - archive_start) as u32,
                (blocktable_pos - archive_start) as u32,
                hashtable_size as u32,
                added_files.len() as u32,
            );

            writer.seek(SeekFrom::Start(archive_start))?;
            header.write(&mut writer)?;
        }

        Ok(())
    }
}

/// Writes out the specified file starting at the writer's current position.
/// If the file is marked for compression, a Sector Offset Table (SOT) will be written, and all sectors will attempt compression.
/// If the file is not marked for compression, no SOT will be written, and sectoring will not be done.
/// If the file is marked for encryption, it will also be encrypted after compression.
fn write_file<W>(
    sector_size: u64,
    archive_start: u64,
    mut writer: W,
    file: &FileRecord,
) -> Result<(u64, u64), IoError>
where
    W: Write + Seek,
{
    let options = file.options;

    if options.compress {
        let sector_count = sector_count_from_size(file.contents.len() as u64, sector_size);
        let sector_table_pos = writer.seek(SeekFrom::Current(0))?;

        let encryption_key = if options.encrypt {
            Some(calculate_file_key(
                &file.file_name,
                (sector_table_pos - archive_start) as u32,
                file.contents.len() as u32,
                options.adjust_key,
            ))
        } else {
            None
        };

        let mut offsets: Vec<u32> = Vec::new();

        // store the start of the first sector
        let first_sector_start = ((sector_count + 1) * 4) as u32;
        writer.seek(SeekFrom::Current(i64::from(first_sector_start)))?;
        offsets.push(first_sector_start);
        // write each sector and its end
        for i in 0..sector_count {
            let sector_start = i * sector_size;
            let sector_end = min((i + 1) * sector_size, file.contents.len() as u64);
            let data = &file.contents[sector_start as usize..sector_end as usize];

            let mut compressed = compress_mpq_block(data);

            // encrypt the block if encryption was requested
            if let Some(key) = encryption_key.map(|k| k + i as u32) {
                encrypt_mpq_block(compressed.to_mut(), key);
            }

            writer.write_all(&compressed)?;

            // store the end of the current sector
            // which is also the start of the next sector if there is one

            let current_offset = writer.seek(SeekFrom::Current(0))?;
            offsets.push((current_offset - sector_table_pos) as u32);
        }

        let file_end = writer.seek(SeekFrom::Current(0))?;

        // write the sector offset table
        {
            let mut buf = vec![0u8; offsets.len() * 4];
            let mut cursor = buf.as_mut_slice();
            for offset in &offsets {
                cursor.write_u32::<LE>(*offset)?;
            }

            // encrypt the SOT if requested
            if let Some(key) = encryption_key.map(|k| k - 1) {
                encrypt_mpq_block(&mut buf, key);
            }

            writer.seek(SeekFrom::Start(sector_table_pos))?;
            writer.write_all(&buf)?;
        }

        // put the writer at the file end, so that we don't overwrite this file with subsequent writes
        writer.seek(SeekFrom::Start(file_end))?;

        Ok((sector_table_pos, u64::from(*offsets.last().unwrap())))
    } else {
        unimplemented!("uncompressed files not yet supported")
    }
}

pub fn test_builder() {
    use std::fs::File;

    let mut builder = MpqBuilder::default();
    let mut out_file = File::create("out.w3x").unwrap();

    let options = FileOptions {
        compress: true,
        encrypt: true,
        adjust_key: false,
    };
    builder.add_file("hello.txt", "hello world!", options);
    builder.add_file("this file is written in rust!.txt", "hello world!", options);
    builder.write(&mut out_file).unwrap();

    out_file.flush().unwrap();
}
