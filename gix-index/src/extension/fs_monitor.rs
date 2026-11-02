use bstr::BString;

use crate::{
    extension::{FsMonitor, Signature},
    util::{read_u32, read_u64, split_at_byte_exclusive},
};

/// The token identifying the point in time at which the fsmonitor was last queried.
#[derive(Clone, Debug)]
pub enum Token {
    /// A timestamp, used by hook-based monitors.
    V1 {
        /// Nanoseconds since the UNIX epoch.
        nanos_since_1970: u64,
    },
    /// An opaque token, as used by `git fsmonitor--daemon`.
    V2 {
        /// The token itself.
        token: BString,
    },
}

impl FsMonitor {
    /// Create a new instance from `token` and a bitmap of entries that aren't known to be valid.
    pub fn new(token: Token, entry_dirty: gix_bitmap::ewah::Vec) -> Self {
        FsMonitor { token, entry_dirty }
    }

    /// Serialize this extension, including its header, to `out`.
    pub fn write_to(&self, mut out: impl std::io::Write) -> std::io::Result<()> {
        let mut body = Vec::new();
        match &self.token {
            Token::V1 { nanos_since_1970 } => {
                body.extend_from_slice(&1_u32.to_be_bytes());
                body.extend_from_slice(&nanos_since_1970.to_be_bytes());
            }
            Token::V2 { token } => {
                body.extend_from_slice(&2_u32.to_be_bytes());
                body.extend_from_slice(token);
                body.push(0);
            }
        }
        let mut bitmap = Vec::new();
        self.entry_dirty.write_to(&mut bitmap)?;
        body.extend_from_slice(&u32::try_from(bitmap.len()).map_err(std::io::Error::other)?.to_be_bytes());
        body.extend_from_slice(&bitmap);
        out.write_all(&SIGNATURE)?;
        out.write_all(&u32::try_from(body.len()).map_err(std::io::Error::other)?.to_be_bytes())?;
        out.write_all(&body)
    }

    /// The token of the last fsmonitor query that was persisted with the index.
    pub fn token(&self) -> &Token {
        &self.token
    }

    /// A bitmap with one bit per index entry: if set, the entry is NOT known to be valid.
    pub fn entry_dirty(&self) -> &gix_bitmap::ewah::Vec {
        &self.entry_dirty
    }
}

/// The signature of the fsmonitor extension.
pub const SIGNATURE: Signature = *b"FSMN";

/// Decode the fsmonitor extension from `data`.
pub fn decode(data: &[u8]) -> Option<FsMonitor> {
    let (version, data) = read_u32(data)?;
    let (token, data) = match version {
        1 => {
            let (nanos_since_1970, data) = read_u64(data)?;
            (Token::V1 { nanos_since_1970 }, data)
        }
        2 => {
            let (token, data) = split_at_byte_exclusive(data, 0)?;
            (Token::V2 { token: token.into() }, data)
        }
        _ => return None,
    };

    let (ewah_size, data) = read_u32(data)?;
    let ((entry_dirty, extra), data) = data
        .split_at_checked(ewah_size as usize)
        .and_then(|(entry_dirty, data)| {
            gix_bitmap::ewah::decode(entry_dirty)
                .ok()
                .map(|entry_dirty| (entry_dirty, data))
        })?;
    if !extra.is_empty() {
        return None;
    }

    if !data.is_empty() {
        return None;
    }

    FsMonitor { token, entry_dirty }.into()
}
