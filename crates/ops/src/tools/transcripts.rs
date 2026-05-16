//! Transcripts stub — ops does not run the transcript stack.
use serde_json::Value;

pub fn transcript_len() -> usize {
    0
}

pub fn transcript_recent(_limit: usize) -> Vec<Value> {
    vec![]
}

pub fn transcript_since(_offset: usize) -> Vec<Value> {
    vec![]
}
