//! The run-certificate conformance suite on the fake issuer ([ADR 0086]): the macro every adapter
//! uses, run here so it is exercised and so a broken fake is seen red.
//!
//! [ADR 0086]: https://github.com/vymalo/another-adam-rs/blob/main/docs/decisions/0086-a-run-signs-its-model-requests-with-a-certificate.md

use std::sync::Arc;

use adam_core::turn::run_cert::DynRunCertIssuer;
use adam_core::turn::run_cert::testkit::FakeRunCertIssuer;

fn make() -> DynRunCertIssuer {
    Arc::new(FakeRunCertIssuer::new())
}

adam_core::run_cert_issuer_conformance!(make);
