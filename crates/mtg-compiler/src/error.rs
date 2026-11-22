use thiserror::Error;

/// Errors produced during card input validation, override resolution, or IR compilation.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CompileError {
    #[error("invalid card input: {0}")]
    InvalidInput(String),

    #[error("invalid override for oracle_id {oracle_id}: {message}")]
    InvalidOverride { oracle_id: String, message: String },

    #[error("{kind} intern table overflow ({len} entries exceeds u16::MAX)")]
    InternTableOverflow { kind: &'static str, len: usize },

    #[error("invalid {kind} id {id} (table length {table_len})")]
    InvalidInternId {
        kind: &'static str,
        id: u16,
        table_len: usize,
    },

    #[error("internal compiler error: {0}")]
    Internal(String),
}
