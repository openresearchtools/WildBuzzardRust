/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Fail-closed coverage for incomplete Wild Buzzard Stylo embedding boundaries.

use std::panic::{catch_unwind, AssertUnwindSafe};
use style::stylesheets::UrlExtraData;
use to_shmem::{SharedMemoryBuilder, ToShmem};
use url::Url;

#[test]
fn stylesheet_url_shared_memory_transfer_returns_structured_error_without_writing() {
    let url_data = UrlExtraData::from(Url::parse("https://example.invalid/style.css").unwrap());
    let mut storage = [0_u8; 64];
    // SAFETY: `storage` is a live, uniquely borrowed allocation for the entire builder lifetime;
    // its pointer is valid for exactly the bounded capacity passed here.
    let mut builder = unsafe { SharedMemoryBuilder::new(storage.as_mut_ptr(), storage.len()) };

    let outcome = catch_unwind(AssertUnwindSafe(|| url_data.to_shmem(&mut builder)));
    let error = outcome
        .expect("unsupported stylesheet URL transfer must not panic")
        .expect_err("unsupported stylesheet URL transfer must fail closed");

    assert_eq!(
        error,
        "Wild Buzzard has not implemented cross-process stylesheet URL transfer"
    );
    assert_eq!(
        builder.len(),
        0,
        "the failing boundary must not consume space"
    );
}
