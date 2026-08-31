//! Every way a decode can fail. One flat enum, comparable with `assert_eq!`,
//! so tests pin the exact failure rather than "it errored somehow".

/// A decode or encode failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The buffer ended before the value did. `need` is the total length that
    /// would have been required from the start of the item, `had` what existed.
    Truncated { need: usize, had: usize },
    /// A LEB128 varint ran past ten bytes, so it cannot fit a u64.
    VarintOverflow,
    /// A length-prefixed string was not valid UTF-8.
    BadUtf8,
    /// A frame declared, or was asked to carry, more than `MAX_PAYLOAD` bytes.
    PayloadTooLarge { len: u64 },
    /// A frame header carried a message type this build has no decoder for.
    UnknownMsgType(u16),
    /// A field the spec marks required was absent (rule R4 was violated by the peer).
    MissingField { tag: u64 },
    /// A field was present but its contents are impossible.
    BadValue { tag: u64, why: &'static str },
    /// A body decoded successfully but had bytes left over.
    TrailingBytes { left: usize },
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Truncated { need, had } => write!(f, "truncated: needed {need} bytes, had {had}"),
            WireError::VarintOverflow => write!(f, "varint longer than 10 bytes"),
            WireError::BadUtf8 => write!(f, "string field was not valid UTF-8"),
            WireError::PayloadTooLarge { len } => write!(f, "payload of {len} bytes exceeds the 64 MiB cap"),
            WireError::UnknownMsgType(t) => write!(f, "unknown message type 0x{t:02x}"),
            WireError::MissingField { tag } => write!(f, "required field 0x{tag:02x} was absent"),
            WireError::BadValue { tag, why } => write!(f, "field 0x{tag:02x} is invalid: {why}"),
            WireError::TrailingBytes { left } => write!(f, "{left} trailing bytes after a complete body"),
        }
    }
}

impl std::error::Error for WireError {}

/// Every fallible operation in this crate returns this.
pub type Result<T> = std::result::Result<T, WireError>;
