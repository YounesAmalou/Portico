// The plan and transaction tasks use the scanner and remove this allowance.
#[allow(dead_code)]
mod diff;
#[allow(unused_imports)]
pub use diff::{FilePreview, Preview, PreviewLine};
// The plan and transaction tasks use the scanner and remove this allowance.
#[allow(dead_code)]
mod scan;
#[allow(unused_imports)]
pub(crate) use scan::*;
