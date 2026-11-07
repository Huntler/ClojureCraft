//! Example inputs taken from another function's output.
//!
//! A referenced input names an upstream function, a revision (the latest one when refreshed, or a
//! pinned one), and one of that revision's examples. The value used everywhere is the recorded
//! snapshot of the upstream output; it changes only when a new snapshot is recorded for exactly
//! the same reference, so upstream edits never alter a downstream example on their own.

use crate::function::{
    Edn, Example, ExampleInput, FunctionDocument, OutputReference, ReferencedValue, RevisionPin,
};
use crate::ids::ExampleId;

impl ExampleInput {
    /// The upstream reference, for an input taken from another function.
    pub fn reference(&self) -> Option<&OutputReference> {
        match self {
            Self::FunctionOutput { source, .. } => Some(source),
            Self::Direct { .. } => None,
        }
    }

    /// The recorded upstream output, for an input taken from another function.
    pub fn snapshot(&self) -> Option<&ReferencedValue> {
        match self {
            Self::FunctionOutput { snapshot, .. } => snapshot.as_ref(),
            Self::Direct { .. } => None,
        }
    }
}

impl RevisionPin {
    pub fn is_pinned(&self) -> bool {
        matches!(self, Self::Pinned(_))
    }
}

/// Whether a snapshot can stand for `source`: a pinned reference only accepts output of the
/// pinned revision.
fn snapshot_fits(source: &OutputReference, snapshot: &ReferencedValue) -> bool {
    match source.revision {
        RevisionPin::Latest => true,
        RevisionPin::Pinned(r) => r == snapshot.revision,
    }
}

impl Example {
    /// Makes the input refer to `source`. A recorded snapshot is kept only when it still answers
    /// the new reference (same function and upstream example, and from the pinned revision if
    /// one is pinned); otherwise the example has no input until it is refreshed. Returns whether
    /// anything changed.
    pub fn set_reference(&mut self, source: OutputReference) -> bool {
        let kept = match &self.input {
            ExampleInput::FunctionOutput {
                source: old,
                snapshot: Some(s),
            } if old.function == source.function
                && old.example == source.example
                && snapshot_fits(&source, s) =>
            {
                Some(s.clone())
            }
            _ => None,
        };
        let input = ExampleInput::FunctionOutput {
            source,
            snapshot: kept,
        };
        if self.input == input {
            return false;
        }
        self.input = input;
        true
    }

    /// Records an executed upstream output, but only if the input still refers to exactly
    /// `source` (it may have been edited while the upstream example ran) and the output comes
    /// from the pinned revision when one is pinned. Returns whether it was recorded.
    pub fn record_snapshot(&mut self, source: &OutputReference, snapshot: ReferencedValue) -> bool {
        if !snapshot_fits(source, &snapshot) {
            return false;
        }
        match &mut self.input {
            ExampleInput::FunctionOutput {
                source: current,
                snapshot: slot,
            } if current == source => {
                if slot.as_ref() == Some(&snapshot) {
                    return false;
                }
                *slot = Some(snapshot);
                true
            }
            _ => false,
        }
    }

    /// Turns a referenced input back into directly written EDN, keeping the recorded value as
    /// its text (empty when nothing was recorded). Returns whether anything changed.
    pub fn use_direct_input(&mut self) -> bool {
        let value = match &self.input {
            ExampleInput::FunctionOutput { snapshot, .. } => snapshot
                .as_ref()
                .map(|s| s.value.clone())
                .unwrap_or_else(Edn::default),
            ExampleInput::Direct { .. } => return false,
        };
        self.input = ExampleInput::Direct { value };
        true
    }
}

impl FunctionDocument {
    /// [`Example::set_reference`] for the example `example`.
    pub fn set_input_reference(&mut self, example: ExampleId, source: OutputReference) -> bool {
        self.example_mut(example).is_some_and(|e| e.set_reference(source))
    }

    /// [`Example::record_snapshot`] for the example `example`.
    pub fn record_reference_snapshot(
        &mut self,
        example: ExampleId,
        source: &OutputReference,
        snapshot: ReferencedValue,
    ) -> bool {
        self.example_mut(example)
            .is_some_and(|e| e.record_snapshot(source, snapshot))
    }

    /// [`Example::use_direct_input`] for the example `example`.
    pub fn use_direct_input(&mut self, example: ExampleId) -> bool {
        self.example_mut(example).is_some_and(Example::use_direct_input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FunctionId, RevisionId, Timestamp};

    fn reference(pin: RevisionPin) -> OutputReference {
        OutputReference {
            function: FunctionId::generate(),
            revision: pin,
            example: ExampleId::generate(),
        }
    }

    fn snapshot(value: &str, revision: RevisionId) -> ReferencedValue {
        ReferencedValue {
            value: Edn::new(value),
            revision,
            resolved_at: Timestamp(1),
        }
    }

    fn doc_with_example() -> (FunctionDocument, ExampleId) {
        let mut doc = FunctionDocument::new("down");
        doc.examples
            .push(Example::direct("e", Edn::new("[1]"), Edn::new("1")));
        let id = doc.examples[0].id;
        (doc, id)
    }

    #[test]
    fn a_snapshot_is_recorded_only_for_the_reference_it_was_resolved_for() {
        let (mut doc, id) = doc_with_example();
        let source = reference(RevisionPin::Latest);
        assert!(doc.set_input_reference(id, source.clone()));
        let r1 = RevisionId::generate();
        assert!(doc.record_reference_snapshot(id, &source, snapshot("[1 2]", r1)));

        // The user picked another upstream example while the old one was running.
        let other = OutputReference {
            example: ExampleId::generate(),
            ..source.clone()
        };
        assert!(doc.set_input_reference(id, other));
        assert_eq!(
            doc.examples[0].input.snapshot(),
            None,
            "a snapshot never answers another example"
        );
        assert!(!doc.record_reference_snapshot(id, &source, snapshot("[9]", r1)));
        assert_eq!(doc.examples[0].input.snapshot(), None);
    }

    #[test]
    fn pinning_keeps_a_snapshot_only_from_the_pinned_revision() {
        let (mut doc, id) = doc_with_example();
        let latest = reference(RevisionPin::Latest);
        doc.set_input_reference(id, latest.clone());
        let r1 = RevisionId::generate();
        doc.record_reference_snapshot(id, &latest, snapshot("[1 2]", r1));

        // Pinning the revision the value came from keeps it.
        let pinned = OutputReference {
            revision: RevisionPin::Pinned(r1),
            ..latest.clone()
        };
        doc.set_input_reference(id, pinned);
        assert_eq!(doc.examples[0].input.snapshot().map(|s| s.revision), Some(r1));

        // Pinning another revision does not pretend the old value came from it.
        let r2 = RevisionId::generate();
        let other_pin = OutputReference {
            revision: RevisionPin::Pinned(r2),
            ..latest
        };
        doc.set_input_reference(id, other_pin.clone());
        assert_eq!(doc.examples[0].input.snapshot(), None);
        assert!(!doc.record_reference_snapshot(id, &other_pin, snapshot("[3]", r1)));
        assert!(doc.record_reference_snapshot(id, &other_pin, snapshot("[3]", r2)));
    }

    #[test]
    fn refreshing_from_a_newer_revision_marks_results_stale_even_with_an_equal_value() {
        let (mut doc, id) = doc_with_example();
        let source = reference(RevisionPin::Latest);
        doc.set_input_reference(id, source.clone());
        doc.record_reference_snapshot(id, &source, snapshot("[1 2]", RevisionId::generate()));
        let before = doc.basis();
        assert!(doc.record_reference_snapshot(id, &source, snapshot("[1 2]", RevisionId::generate())));
        let freshness = before.freshness(&doc.basis());
        assert_eq!(
            freshness,
            crate::Freshness::Stale(vec![crate::Component::Specification])
        );
    }

    #[test]
    fn detaching_keeps_the_recorded_value_as_written_input() {
        let (mut doc, id) = doc_with_example();
        let source = reference(RevisionPin::Latest);
        doc.set_input_reference(id, source.clone());
        doc.record_reference_snapshot(id, &source, snapshot("{:a 1}", RevisionId::generate()));
        let fingerprint = doc.spec_fingerprint();
        assert!(doc.use_direct_input(id));
        assert_eq!(
            doc.examples[0].input,
            ExampleInput::Direct {
                value: Edn::new("{:a 1}")
            }
        );
        assert_ne!(
            fingerprint,
            doc.spec_fingerprint(),
            "the input's provenance changed"
        );
        assert!(!doc.use_direct_input(id));
    }
}
