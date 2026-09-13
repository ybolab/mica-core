//! Static asset hosting: path resolution, content classification, and the
//! router that applies both.
//!
//! `docs/design/api.md` §4, split so that the decisions are pure functions and
//! only [`serve`] touches axum: [`path::resolve`] decides *which file*,
//! [`mime::content_type`] and [`mime::cache_class`] decide *what headers it is
//! served with*, and [`serve::fallback`] is the asset router §4.1 rule 4
//! mounts as the router's fallback — which is what makes §4.1's precedence
//! structural rather than checked.
//!
//! [`serve`] applies §4.2's fallback for the one rejection that permits it
//! ([`path::Rejection::eligible_for_fallback`]); §4.3's `/api/` row is
//! [`mime::CacheClass::NoStore`], applied by the reservation in
//! `crate::routes`.

pub mod builtin;
pub mod mime;
pub mod path;
pub mod serve;
