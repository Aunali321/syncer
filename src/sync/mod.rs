//! Keeping folders in step with peers: following their indexes, fetching
//! content and Git objects, and serving ours.

pub mod fetch;
pub mod pull;
pub mod serve;
pub mod worker;
