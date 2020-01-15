/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use failure::Fail;

#[derive(Debug, Fail)]
pub enum ErrorKind {
    #[fail(
        display = "The `sync_status` column in DB has an illegal value: {}",
        _0
    )]
    BadSyncStatus(u8),

    #[fail(
        display = "Schema name {:?} does not match the collection name for this remerge database ({:?})",
        _0, _1
    )]
    SchemaNameMatchError(String, String),

    #[fail(display = "Invalid schema: {}", _0)]
    SchemaError(#[fail(cause)] crate::schema::error::SchemaError),

    #[fail(display = "Invalid record: {}", _0)]
    InvalidRecord(#[fail(cause)] InvalidRecord),

    #[fail(
        display = "No record with guid exists (when one was required): {:?}",
        _0
    )]
    NoSuchRecord(String),

    #[fail(
        display = "Failed to convert local record to native record (may indicate bad remerge schema): {}",
        _0
    )]
    LocalToNativeError(String),

    #[fail(display = "Error: {}", _0)]
    Unspecified(String),

    #[fail(display = "Error parsing JSON data: {}", _0)]
    JsonError(#[fail(cause)] serde_json::Error),

    #[fail(display = "Error executing SQL: {}", _0)]
    SqlError(#[fail(cause)] rusqlite::Error),

    #[fail(display = "Error parsing URL: {}", _0)]
    UrlParseError(#[fail(cause)] url::ParseError),

    /// Note: not an 'InvalidRecord' variant because it doesn't come from the user.
    #[fail(
        display = "UntypedMap has a key and tombstone with the same name when OnCollision::Error was requested"
    )]
    UntypedMapTombstoneCollision,

    #[fail(display = "Operation interrupted")]
    Interrupted,

    #[fail(display = "Not Yet Implemented: {}", _0)]
    NotYetImplemented(String),
}

error_support::define_error! {
    ErrorKind {
        (JsonError, serde_json::Error),
        (SchemaError, crate::schema::error::SchemaError),
        (UrlParseError, url::ParseError),
        (SqlError, rusqlite::Error),
        (InvalidRecord, InvalidRecord),
        (Unspecified, String),
    }
}

impl From<&str> for ErrorKind {
    fn from(e: &str) -> ErrorKind {
        log::error!("error: {}", e);
        ErrorKind::Unspecified(e.into())
    }
}

impl From<&str> for Error {
    fn from(e: &str) -> Self {
        Error::from(ErrorKind::from(e))
    }
}
#[derive(Debug, Fail)]
pub enum InvalidRecord {
    #[fail(display = "Cannot insert non-json object")]
    NotJsonObject,
    #[fail(display = "The field {:?} is required", _0)]
    MissingRequiredField(crate::Sym),
    #[fail(display = "The field {:?} must be of type \"{}\"", _0, _1)]
    WrongFieldType(crate::Sym, crate::schema::FieldKind),
    #[fail(display = "The field {:?} must parse as a valid url", _0)]
    NotUrl(crate::Sym),
    #[fail(display = "The field {:?} is out of the required bounds", _0)]
    OutOfBounds(crate::Sym),
    #[fail(display = "The field {:?} is not a valid record_set", _0)]
    InvalidRecordSet(crate::Sym),
    #[fail(display = "The field {:?} is not a valid guid", _0)]
    InvalidGuid(String),
    // TODO(issue 2232): Should be more specific.
    #[fail(display = "The field {:?} is invalid: {}", _0, _1)]
    InvalidField(crate::Sym, String),
    #[fail(display = "A record with the given guid already exists")]
    IdNotUnique,
    #[fail(display = "Record violates a `dedupe_on` constraint")]
    Duplicate,
}