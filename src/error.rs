//! The app-wide error: a Riot failure, a database failure, or anything else.

use std::fmt;

use crate::riot::RiotError;

#[derive(Debug)]
pub enum Error {
    Riot(RiotError),
    Db(rusqlite::Error),
    Other(String),
}

impl Error {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Riot(e) => e.kind(),
            Self::Db(_) => "DatabaseError",
            Self::Other(_) => "Error",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Riot(e) => e.fmt(f),
            Self::Db(e) => e.fmt(f),
            Self::Other(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for Error {}

impl From<RiotError> for Error {
    fn from(e: RiotError) -> Self {
        Self::Riot(e)
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Db(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
