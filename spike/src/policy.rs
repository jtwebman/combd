//! Policy hooks: where a service built on combd puts its own rules, such as
//! which org may claim which domain (and how it proves it owns it: a TXT
//! record), or how many objects, bytes, or awake cells a tenant may have.
//! combd itself has none: it runs your own apps, and every rule here would
//! only be in your way. Running combd for others? Fork it and write yours
//! in these functions. Each returns `Err(why)` to refuse.

use crate::catalog::{Entry, State};

/// A change about to be written to the catalog: a deploy (`entry.app` is
/// `org/app`, its value the app's state: version, domains), its vars
/// (`org/app#vars`), a secret (`org/app#secret/<name>`), or the app's
/// removal (an empty value), with the state it would apply to. Refused, the
/// write fails with `why`. Runs after combd's own checks (two apps can't
/// hold one domain), on the node that takes the write.
pub fn catalog_write(_state: &State, _entry: &Entry) -> Result<(), String> {
    Ok(())
}

/// A new object about to start on this node: nothing of it exists yet (no
/// file, nothing in the bucket to restore). `name` is
/// `org/app/process/id` for an app's object. Refused, its request fails
/// with `why` (403 alone; in a fleet, the client is told to retry).
pub fn new_object(_name: &str) -> Result<(), String> {
    Ok(())
}
