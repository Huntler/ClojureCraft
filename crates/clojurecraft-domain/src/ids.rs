//! Stable identifiers. Every identifier is a random UUID so that records can be created
//! independently (for example in a duplicated function) without coordination.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            /// A new, globally unique identifier.
            pub fn generate() -> Self {
                Self(Uuid::new_v4())
            }

            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(s).map(Self)
            }
        }
    };
}

id_type!(
    /// Identity of a function across renames, edits, and revisions.
    FunctionId
);
id_type!(
    /// A transformation step; stays the same when steps are reordered or reworded.
    StepId
);
id_type!(
    /// An example input/expected-output pair within one function.
    ExampleId
);
id_type!(
    /// An immutable snapshot of a function.
    RevisionId
);
id_type!(
    /// An edge-case test within one function.
    TestCaseId
);
id_type!(
    /// One validation run.
    ValidationId
);
id_type!(
    /// A saved benchmark configuration within one function.
    BenchmarkConfigId
);
id_type!(
    /// One representative input of a benchmark configuration.
    BenchmarkInputId
);
id_type!(
    /// One benchmark run.
    BenchmarkResultId
);
