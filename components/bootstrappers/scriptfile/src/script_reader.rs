// Copyright 2025 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Bootstrap script reader for processing JSONL script files
//!
//! This module provides functionality to read and parse bootstrap script files in JSONL format.
//! It supports multi-file reading, automatic sequencing, header validation, comment filtering,
//! and finish record handling.

use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::PathBuf,
};

use anyhow::anyhow;

use crate::script_types::{
    BootstrapFinishRecord, BootstrapHeaderRecord, BootstrapScriptRecord,
    SequencedBootstrapScriptRecord,
};

/// Reader for bootstrap script files
///
/// Reads JSONL files sequentially, validates header presence, filters comments,
/// and handles finish records. Implements Iterator for record iteration.
#[derive(Debug)]
pub struct BootstrapScriptReader {
    /// List of script files to read
    files: Vec<PathBuf>,
    /// Index of next file to open
    next_file_index: usize,
    /// Current file reader
    current_reader: Option<BufReader<File>>,
    /// Header record from the script
    header: BootstrapHeaderRecord,
    /// Footer/finish record (cached once encountered)
    footer: Option<SequencedBootstrapScriptRecord>,
    /// Current sequence number
    seq: u64,
    /// Whether the finish record has been returned by the iterator
    finish_returned: bool,
}

impl BootstrapScriptReader {
    /// Create a new bootstrap script reader
    ///
    /// # Arguments
    /// * `files` - List of JSONL file paths to read in order
    ///
    /// # Returns
    /// Result containing the reader or an error if validation fails or header is missing
    ///
    /// # Errors
    /// - Returns error if any file doesn't have .jsonl extension
    /// - Returns error if first record is not a Header
    pub fn new(files: Vec<PathBuf>) -> anyhow::Result<Self> {
        // Only supports JSONL files. Return error if any of the files are not JSONL files.
        for file in &files {
            if file.extension().map(|ext| ext != "jsonl").unwrap_or(true) {
                return Err(anyhow!(
                    "Invalid script file; only JSONL files supported: {}",
                    file.to_string_lossy()
                ));
            }
        }

        let mut reader = BootstrapScriptReader {
            files,
            next_file_index: 0,
            current_reader: None,
            header: BootstrapHeaderRecord::default(),
            footer: None,
            seq: 0,
            finish_returned: false,
        };

        // Read and validate first record is a Header
        let read_result = reader.get_next_record();

        if let Ok(seq_rec) = read_result {
            if let BootstrapScriptRecord::Header(header) = seq_rec.record {
                reader.header = header;
                Ok(reader)
            } else {
                Err(anyhow!(
                    "Script is missing Header record: {}",
                    reader.get_current_file_name()
                ))
            }
        } else {
            Err(anyhow!(
                "Script is missing Header record: {}",
                reader.get_current_file_name()
            ))
        }
    }

    /// Get the header record from the script
    pub fn get_header(&self) -> BootstrapHeaderRecord {
        self.header.clone()
    }

    /// Get the next record from the script
    ///
    /// Reads records sequentially from all files, filtering out comments.
    /// Once a Finish record is encountered, it is cached and always returned.
    /// If no Finish record exists, one is auto-generated at end of files.
    fn get_next_record(&mut self) -> anyhow::Result<SequencedBootstrapScriptRecord> {
        // Once we have reached the end of the script, always return the Finish record.
        if let Some(ref footer) = self.footer {
            return Ok(footer.clone());
        }

        if self.current_reader.is_none() {
            self.open_next_file()?;
        }

        if let Some(reader) = &mut self.current_reader {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    // End of current file, try next file
                    self.current_reader = None;
                    self.get_next_record()
                }
                Ok(_) => {
                    let record: BootstrapScriptRecord = match serde_json::from_str(&line) {
                        Ok(r) => r,
                        Err(e) => {
                            return Err(anyhow!(
                                "Bad record format in file {}: Error - {}; Record - {}",
                                self.get_current_file_name(),
                                e,
                                line
                            ));
                        }
                    };

                    let seq_rec = match &record {
                        BootstrapScriptRecord::Comment(_) => {
                            // The BootstrapScriptReader should never return a Comment record.
                            // Return the next record, but need to increment sequence counter
                            self.seq += 1;
                            return self.get_next_record();
                        }
                        BootstrapScriptRecord::Header(_) => {
                            let seq_rec = SequencedBootstrapScriptRecord {
                                record: record.clone(),
                                seq: self.seq,
                            };

                            // Warn if there is a Header record in the middle of the script.
                            if seq_rec.seq > 0 {
                                log::warn!(
                                    "Header record found not at start of the script: {:?}",
                                    seq_rec
                                );
                            }

                            seq_rec
                        }
                        _ => SequencedBootstrapScriptRecord {
                            record: record.clone(),
                            seq: self.seq,
                        },
                    };
                    self.seq += 1;

                    // If the record is a Finish record, set the footer and return it so it is always returned in the future.
                    if let BootstrapScriptRecord::Finish(_) = seq_rec.record {
                        self.footer = Some(seq_rec.clone());
                    }
                    Ok(seq_rec)
                }
                Err(e) => Err(anyhow!("Error reading file: {}", e)),
            }
        } else {
            // Generate a synthetic Finish record to mark the end of the script.
            let footer = SequencedBootstrapScriptRecord {
                record: BootstrapScriptRecord::Finish(BootstrapFinishRecord {
                    description: "Auto generated at end of script.".to_string(),
                }),
                seq: self.seq,
            };
            self.footer = Some(footer.clone());
            Ok(footer)
        }
    }

    /// Get the current file name (for error reporting)
    fn get_current_file_name(&self) -> String {
        if self.current_reader.is_some() {
            let path = self.files[self.next_file_index - 1].clone();
            path.to_string_lossy().into_owned()
        } else {
            "None".to_string()
        }
    }

    /// Open the next file in the sequence
    fn open_next_file(&mut self) -> anyhow::Result<()> {
        if self.next_file_index < self.files.len() {
            let file_path = &self.files[self.next_file_index];
            let file = File::open(file_path).map_err(|e| {
                anyhow!(
                    "Can't open script file: {} - {}",
                    file_path.to_string_lossy(),
                    e
                )
            })?;
            self.current_reader = Some(BufReader::new(file));
            self.next_file_index += 1;
        } else {
            self.current_reader = None;
        }
        Ok(())
    }
}

impl Iterator for BootstrapScriptReader {
    type Item = anyhow::Result<SequencedBootstrapScriptRecord>;

    fn next(&mut self) -> Option<Self::Item> {
        // If we've already returned the finish record, we're done
        if self.finish_returned {
            return None;
        }

        // If footer is set but not yet returned, return it now
        if let Some(footer) = &self.footer {
            self.finish_returned = true;
            return Some(Ok(footer.clone()));
        }

        // Get the next record
        match self.get_next_record() {
            Ok(record) => {
                // Check if a finish record was just cached
                if self.footer.is_some() {
                    // The record we got back IS the finish record
                    self.finish_returned = true;
                }
                Some(Ok(record))
            }
            Err(e) => Some(Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn create_temp_jsonl_file(content: &str) -> std::path::PathBuf {
        let temp_dir = std::env::temp_dir();
        let file_name = format!("test_{}.jsonl", uuid::Uuid::new_v4());
        let file_path = temp_dir.join(file_name);

        std::fs::write(&file_path, content).unwrap();
        file_path
    }

    fn cleanup_temp_file(path: &Path) {
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_invalid_file_extension() {
        let temp_dir = std::env::temp_dir();
        let file_path = temp_dir.join("test.txt");
        std::fs::write(&file_path, "test").unwrap();

        let result = BootstrapScriptReader::new(vec![file_path.clone()]);
        cleanup_temp_file(&file_path);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("only JSONL files supported"));
    }

    #[test]
    fn test_missing_header() {
        let content = r#"{"kind":"Node","id":"n1","labels":["Test"],"properties":{}}"#;
        let file_path = create_temp_jsonl_file(content);

        let result = BootstrapScriptReader::new(vec![file_path.clone()]);
        cleanup_temp_file(&file_path);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("missing Header record"));
    }

    #[test]
    fn test_valid_script_with_header() {
        let content = r#"{"kind":"Header","start_time":"2024-01-01T00:00:00+00:00","description":"Test"}
{"kind":"Node","id":"n1","labels":["Test"],"properties":{}}
{"kind":"Finish","description":"Done"}"#;
        let file_path = create_temp_jsonl_file(content);

        let reader = BootstrapScriptReader::new(vec![file_path.clone()]);
        assert!(reader.is_ok());

        let reader = reader.unwrap();
        assert_eq!(reader.get_header().description, "Test");

        cleanup_temp_file(&file_path);
    }

    #[test]
    fn test_comment_filtering() {
        let content = r#"{"kind":"Header","start_time":"2024-01-01T00:00:00+00:00","description":"Test"}
{"kind":"Comment","comment":"This should be filtered"}
{"kind":"Node","id":"n1","labels":["Test"],"properties":{}}"#;
        let file_path = create_temp_jsonl_file(content);

        let mut reader = BootstrapScriptReader::new(vec![file_path.clone()]).unwrap();

        // First record after header should be Node (comment filtered)
        let record = reader.next().unwrap().unwrap();
        match record.record {
            BootstrapScriptRecord::Node(n) => assert_eq!(n.id, "n1"),
            _ => panic!("Expected Node record, got {:?}", record.record),
        }

        cleanup_temp_file(&file_path);
    }

    #[test]
    fn test_auto_generated_finish() {
        let content = r#"{"kind":"Header","start_time":"2024-01-01T00:00:00+00:00","description":"Test"}
{"kind":"Node","id":"n1","labels":["Test"],"properties":{}}"#;
        let file_path = create_temp_jsonl_file(content);

        let mut reader = BootstrapScriptReader::new(vec![file_path.clone()]).unwrap();

        // Read the node
        let rec1 = reader.next().unwrap().unwrap();
        match rec1.record {
            BootstrapScriptRecord::Node(_) => {}
            _ => panic!("Expected Node"),
        }

        // Auto-generated Finish record is returned
        let rec2 = reader.next().unwrap().unwrap();
        match rec2.record {
            BootstrapScriptRecord::Finish(f) => {
                assert!(f.description.contains("Auto generated"));
            }
            _ => panic!("Expected Finish"),
        }

        // After Finish, iteration stops
        assert!(reader.next().is_none());

        cleanup_temp_file(&file_path);
    }

    #[test]
    fn test_sequence_numbering() {
        let content = r#"{"kind":"Header","start_time":"2024-01-01T00:00:00+00:00","description":"Test"}
{"kind":"Node","id":"n1","labels":["Test"],"properties":{}}
{"kind":"Node","id":"n2","labels":["Test"],"properties":{}}"#;
        let file_path = create_temp_jsonl_file(content);

        let mut reader = BootstrapScriptReader::new(vec![file_path.clone()]).unwrap();

        let rec1 = reader.next().unwrap().unwrap();
        assert_eq!(rec1.seq, 1); // Header is seq 0

        let rec2 = reader.next().unwrap().unwrap();
        assert_eq!(rec2.seq, 2);

        cleanup_temp_file(&file_path);
    }

    #[test]
    fn test_malformed_json() {
        let content = r#"{"kind":"Header","start_time":"2024-01-01T00:00:00+00:00","description":"Test"}
not valid json"#;
        let file_path = create_temp_jsonl_file(content);

        let mut reader = BootstrapScriptReader::new(vec![file_path.clone()]).unwrap();

        let result = reader.next();
        assert!(result.is_some());
        assert!(result.unwrap().is_err());

        cleanup_temp_file(&file_path);
    }

    #[test]
    fn test_multi_file_reading() {
        let content1 = r#"{"kind":"Header","start_time":"2024-01-01T00:00:00+00:00","description":"Test"}
{"kind":"Node","id":"n1","labels":["Test"],"properties":{}}"#;
        let content2 = r#"{"kind":"Node","id":"n2","labels":["Test"],"properties":{}}
{"kind":"Finish","description":"Done"}"#;

        let file1 = create_temp_jsonl_file(content1);
        let file2 = create_temp_jsonl_file(content2);

        let mut reader = BootstrapScriptReader::new(vec![file1.clone(), file2.clone()]).unwrap();

        // Read both nodes
        let rec1 = reader.next().unwrap().unwrap();
        match rec1.record {
            BootstrapScriptRecord::Node(n) => assert_eq!(n.id, "n1"),
            _ => panic!("Expected Node n1"),
        }

        let rec2 = reader.next().unwrap().unwrap();
        match rec2.record {
            BootstrapScriptRecord::Node(n) => assert_eq!(n.id, "n2"),
            _ => panic!("Expected Node n2"),
        }

        // Finish record from file2 is returned
        let rec3 = reader.next().unwrap().unwrap();
        match rec3.record {
            BootstrapScriptRecord::Finish(_) => {}
            _ => panic!("Expected Finish"),
        }

        // Iterator stops after returning Finish
        assert!(reader.next().is_none());

        cleanup_temp_file(&file1);
        cleanup_temp_file(&file2);
    }
}
