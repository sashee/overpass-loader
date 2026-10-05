//! Why an import fails: the input is refused (exit status 1), or reading or
//! writing files failed (exit status 2).

use std::fmt;
use std::io;
use std::path::Path;

use crate::blocks::BlockError;
use crate::map::IdTooLarge;
use crate::pbf::PbfError;

#[derive(Debug)]
pub enum ImportError {
    Input(PbfError),
    Block {
        file: &'static str,
        error: BlockError,
    },
    Map(IdTooLarge),
    /// Merging the area files of sharded builds; see areas.rs.
    Areas(crate::areas::AreaError),
    Io(io::Error),
    /// A part of the import stopped because another failed; that one's
    /// error is reported instead.
    Cancelled,
}

impl ImportError {
    /// Whether the input is at fault, rather than the system.
    pub fn is_refusal(&self) -> bool {
        !matches!(self, ImportError::Io(_) | ImportError::Cancelled)
    }
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::Input(e) => write!(f, "{e}"),
            ImportError::Block { file, error } => write!(f, "{file}: {error}"),
            ImportError::Map(e) => write!(f, "{e}"),
            ImportError::Areas(e) => write!(f, "{e}"),
            ImportError::Io(e) => write!(f, "{e}"),
            ImportError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<io::Error> for ImportError {
    fn from(e: io::Error) -> ImportError {
        ImportError::Io(e)
    }
}

impl From<PbfError> for ImportError {
    fn from(e: PbfError) -> ImportError {
        ImportError::Input(e)
    }
}

impl From<IdTooLarge> for ImportError {
    fn from(e: IdTooLarge) -> ImportError {
        ImportError::Map(e)
    }
}

/// An I/O error with the path it happened on.
pub fn at(path: &Path) -> impl Fn(io::Error) -> io::Error + '_ {
    move |e| io::Error::new(e.kind(), format!("{}: {e}", path.display()))
}
