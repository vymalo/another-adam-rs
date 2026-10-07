//! The `Memory` implementations pass the conformance suites they define; and the suites fail what
//! they are meant to fail.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use adam_operator_ports::memory::{MemoryDirectory, MemoryRuntime, MemoryStore};

mod runtime {
    use super::*;

    async fn make() -> Option<MemoryRuntime> {
        Some(MemoryRuntime::new())
    }
    adam_operator_ports::runtime_provider_conformance!(make);
}

mod runtime_without_suspend {
    use super::*;

    async fn make() -> Option<MemoryRuntime> {
        Some(MemoryRuntime::new().without_suspend())
    }
    adam_operator_ports::runtime_provider_conformance!(make);
}

mod store {
    use super::*;

    async fn make() -> Option<MemoryStore> {
        Some(MemoryStore::new())
    }
    adam_operator_ports::store_provisioner_conformance!(make);
}

mod store_without_cnpg {
    use super::*;

    async fn make() -> Option<MemoryStore> {
        Some(MemoryStore::new().without_cnpg())
    }
    adam_operator_ports::store_provisioner_conformance!(make);
}

mod directory {
    use super::*;

    async fn make() -> Option<MemoryDirectory> {
        Some(MemoryDirectory::new())
    }
    adam_operator_ports::agent_directory_conformance!(make);
}
