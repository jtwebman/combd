//! Storage, behind the little the cores need: get, put (always, if absent,
//! or if the etag still matches), delete, list. A bucket (S3, R2, Silo) is
//! the first backend; Postgres, or whatever an operator runs at scale, can be
//! another. The catalog uses this from the start; the ownership core's
//! bucket calls move behind it later.

use crate::s3::{Cond, S3};

pub trait Store: Send + Sync {
    /// The value and its etag, or `None`.
    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, String)>, String>;
    /// `Ok(Ok(etag))`, or `Ok(Err(()))` when the condition failed.
    fn put(&self, key: &str, value: &[u8], cond: &Cond) -> Result<Result<String, ()>, String>;
    fn delete(&self, key: &str) -> Result<(), String>;
    fn list(&self, prefix: &str) -> Result<Vec<String>, String>;
}

impl Store for S3 {
    fn get(&self, key: &str) -> Result<Option<(Vec<u8>, String)>, String> {
        S3::get(self, key).map_err(|e| e.to_string())
    }
    fn put(&self, key: &str, value: &[u8], cond: &Cond) -> Result<Result<String, ()>, String> {
        S3::put(self, key, value, cond).map_err(|e| e.to_string())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        S3::delete(self, key).map_err(|e| e.to_string())
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, String> {
        S3::list(self, prefix).map_err(|e| e.to_string())
    }
}
