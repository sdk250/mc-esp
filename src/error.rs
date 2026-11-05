use thiserror::Error;
use std::{
    sync::Arc,
    io::Error as IOError
};


pub type Result<T, E = ESPError> = std::result::Result<T, E>;


#[derive(Debug, Error)]
pub enum ESPError {
    #[error("IO Error: {0}")]
    IO(#[from] IOError),

    #[error("{0}: 0 bytes read.")]
    ZeroRead(&'static str),

    #[error("{0}: Not fully read: dst={1}, cur={2}")]
    NotFullyRead(&'static str, usize, usize),

    #[error("{0}")]
    Overlay(#[from] android_overlay::Error),

    #[error("An error occurred in {0}: {1}")]
    Custom(&'static str, Arc<str>)
}
