//! Context accounting and the truncation policy.
//!
//! A prompt is built from labelled sections. Sections are never cut in the middle. Required
//! sections (the user's specification, examples, contract, instructions) are never left out: if
//! they do not fit together with the space reserved for the answer, the request is refused with
//! the token counts and a concrete remedy. Optional sections (retrieved documentation, earlier
//! attempts) are left out whole, in their stated order, until the request fits, and the plan
//! records what was left out so the caller can show it.

use crate::error::{GenerationError, Remedy};
use crate::protocol::Message;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Part of the system message (instructions).
    System,
    /// Part of a user message (the task and its data).
    User,
    /// An earlier answer of the model, for a request that continues a conversation.
    Assistant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Priority {
    Required,
    /// Left out when space runs short; higher values are left out first, and among equal
    /// values earlier sections first (older turns of a conversation before newer ones).
    Optional(u32),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    /// Shown to the user when the section is left out or too large, e.g. "Example 2".
    pub label: String,
    pub placement: Placement,
    pub priority: Priority,
    pub text: String,
}

impl Section {
    pub fn required(label: &str, placement: Placement, text: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            placement,
            priority: Priority::Required,
            text: text.into(),
        }
    }
    pub fn optional(label: &str, drop_order: u32, text: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            placement: Placement::User,
            priority: Priority::Optional(drop_order),
            text: text.into(),
        }
    }
    pub fn with_placement(mut self, placement: Placement) -> Self {
        self.placement = placement;
        self
    }
}

/// Joins sections into a system message (if any), then user and assistant messages in order:
/// consecutive sections of the same placement form one message.
pub fn messages(sections: &[&Section]) -> Vec<Message> {
    let texts = |p: Placement| {
        sections
            .iter()
            .filter(move |s| s.placement == p && !s.text.trim().is_empty())
            .map(|s| s.text.trim_end())
    };
    let mut out = Vec::new();
    let system = texts(Placement::System).collect::<Vec<_>>().join("\n\n");
    if !system.is_empty() {
        out.push(Message::system(system));
    }
    let mut turns: Vec<(Placement, Vec<&str>)> = Vec::new();
    for s in sections
        .iter()
        .filter(|s| s.placement != Placement::System && !s.text.trim().is_empty())
    {
        match turns.last_mut() {
            Some((p, parts)) if *p == s.placement => parts.push(s.text.trim_end()),
            _ => turns.push((s.placement, vec![s.text.trim_end()])),
        }
    }
    if turns.first().is_none_or(|(p, _)| *p != Placement::User) {
        turns.insert(0, (Placement::User, Vec::new()));
    }
    for (p, parts) in turns {
        let text = parts.join("\n\n");
        out.push(match p {
            Placement::Assistant => Message::assistant(text, Vec::new()),
            _ => Message::user(text),
        });
    }
    out
}

/// What will be sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptPlan {
    /// Indices into the original section list, in order.
    pub included: Vec<usize>,
    /// Labels and token counts of optional sections left out.
    pub omitted: Vec<(String, u32)>,
    pub prompt_tokens: u32,
    pub reserved_output: u32,
    pub context: u32,
    /// True when counts come from the model's tokenizer rather than an estimate.
    pub exact: bool,
}

/// Token counts for a request.
pub trait TokenCounter {
    /// Tokens of one section's text on its own.
    fn section(&mut self, text: &str) -> Result<u32, GenerationError>;
    /// Tokens of the complete rendered prompt (chat template included).
    fn prompt(&mut self, messages: &[Message]) -> Result<u32, GenerationError>;
    fn exact(&self) -> bool;
}

/// A counter for when no tokenizer is available: about 3 characters per token, rounded up.
/// Code and EDN tokenize densely, so this overestimates prose and roughly matches code.
pub struct Estimate;

impl TokenCounter for Estimate {
    fn section(&mut self, text: &str) -> Result<u32, GenerationError> {
        Ok(u32::try_from(text.chars().count().div_ceil(3)).unwrap_or(u32::MAX))
    }
    fn prompt(&mut self, messages: &[Message]) -> Result<u32, GenerationError> {
        let mut n = 0u32;
        for m in messages {
            n = n.saturating_add(self.section(&m.content)?).saturating_add(8);
        }
        Ok(n)
    }
    fn exact(&self) -> bool {
        false
    }
}

/// Fits sections into `context` tokens, keeping `reserved_output` free for the answer.
/// `model_max` is the largest context the model supports, used to suggest a remedy.
pub fn plan(
    sections: &[Section],
    counter: &mut dyn TokenCounter,
    context: u32,
    reserved_output: u32,
    model_max: Option<u32>,
) -> Result<PromptPlan, GenerationError> {
    let all: Vec<&Section> = sections.iter().collect();
    let full = counter.prompt(&messages(&all))?;
    let available = context.saturating_sub(reserved_output);
    let mut included: Vec<usize> = (0..sections.len()).collect();
    let mut omitted = Vec::new();
    let mut tokens = full;
    if tokens > available {
        let mut droppable: Vec<usize> = included
            .iter()
            .copied()
            .filter(|i| sections[*i].priority != Priority::Required)
            .collect();
        // Highest drop order first; among equals, earlier sections first.
        droppable.sort_by_key(|i| (std::cmp::Reverse(sections[*i].priority), *i));
        for i in droppable {
            if tokens <= available {
                break;
            }
            if !included.contains(&i) {
                continue;
            }
            // An earlier answer leaves together with the message that replied to it.
            let mut unit = vec![i];
            if sections[i].placement == Placement::Assistant
                && sections
                    .get(i + 1)
                    .is_some_and(|next| next.priority == sections[i].priority)
            {
                unit.push(i + 1);
            }
            for k in unit {
                let n = counter.section(&sections[k].text)?;
                tokens = tokens.saturating_sub(n);
                included.retain(|j| *j != k);
                omitted.push((sections[k].label.clone(), n));
            }
        }
        if !omitted.is_empty() {
            // Recount what is actually sent; joining text changes counts slightly.
            let kept: Vec<&Section> = included.iter().map(|i| &sections[*i]).collect();
            tokens = counter.prompt(&messages(&kept))?;
        }
    }
    if tokens > available {
        let mut largest = Vec::new();
        for i in &included {
            let s = &sections[*i];
            if s.priority == Priority::Required && s.placement == Placement::User {
                largest.push((s.label.clone(), counter.section(&s.text)?));
            }
        }
        largest.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        largest.truncate(3);
        let needed = tokens.saturating_add(reserved_output);
        let mut remedies = Vec::new();
        let suggested = needed.div_ceil(1024).saturating_mul(1024);
        if model_max.is_none_or(|m| suggested <= m) {
            remedies.push(Remedy::LargerContext(suggested));
        }
        if !largest.is_empty() {
            remedies.push(Remedy::ShortenInput(
                largest.iter().map(|(l, _)| l.clone()).collect(),
            ));
        }
        return Err(GenerationError::ContextExceeded {
            needed,
            context,
            reserved_output,
            largest,
            remedies,
        });
    }
    Ok(PromptPlan {
        included,
        omitted,
        prompt_tokens: tokens,
        reserved_output,
        context,
        exact: counter.exact(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One token per character, plus 10 for the template.
    struct Chars;
    impl TokenCounter for Chars {
        fn section(&mut self, text: &str) -> Result<u32, GenerationError> {
            Ok(u32::try_from(text.len()).unwrap())
        }
        fn prompt(&mut self, messages: &[Message]) -> Result<u32, GenerationError> {
            Ok(10
                + messages
                    .iter()
                    .map(|m| u32::try_from(m.content.len()).unwrap())
                    .sum::<u32>())
        }
        fn exact(&self) -> bool {
            true
        }
    }

    fn sections() -> Vec<Section> {
        vec![
            Section::required("Instructions", Placement::System, "s".repeat(10)),
            Section::required("Description", Placement::User, "d".repeat(30)),
            Section::optional("Doc: map", 1, "m".repeat(20)),
            Section::optional("Doc: reduce", 2, "r".repeat(20)),
            Section::required("Example 1", Placement::User, "e".repeat(40)),
        ]
    }

    #[test]
    fn everything_fits_when_there_is_room() {
        let p = plan(&sections(), &mut Chars, 400, 100, None).unwrap();
        assert_eq!(p.included, vec![0, 1, 2, 3, 4]);
        assert!(p.omitted.is_empty());
        assert!(p.prompt_tokens <= 300);
    }

    #[test]
    fn optional_sections_are_left_out_whole_in_drop_order() {
        // Full prompt: 10 + 10 + (30+20+20+40 + 3 separators * 2) = 136.
        let p = plan(&sections(), &mut Chars, 220, 100, None).unwrap();
        assert_eq!(p.omitted, vec![("Doc: reduce".to_owned(), 20)]);
        assert_eq!(p.included, vec![0, 1, 2, 4]);
        assert!(p.prompt_tokens <= 120);
    }

    #[test]
    fn required_content_is_never_dropped_and_overflow_names_remedies() {
        let e = plan(&sections(), &mut Chars, 150, 100, Some(4096)).unwrap_err();
        let GenerationError::ContextExceeded {
            needed,
            context,
            largest,
            remedies,
            ..
        } = e
        else {
            panic!("{e:?}")
        };
        assert_eq!(context, 150);
        assert!(needed > 150);
        assert_eq!(largest[0], ("Example 1".to_owned(), 40));
        assert_eq!(remedies[0], Remedy::LargerContext(1024));
        assert!(matches!(&remedies[1], Remedy::ShortenInput(l) if l[0] == "Example 1"));
    }

    #[test]
    fn larger_context_is_only_suggested_within_the_model_limit() {
        let e = plan(&sections(), &mut Chars, 150, 100, Some(512)).unwrap_err();
        let GenerationError::ContextExceeded { remedies, .. } = e else {
            panic!()
        };
        assert!(!remedies.iter().any(|r| matches!(r, Remedy::LargerContext(_))));
    }

    #[test]
    fn equal_drop_orders_leave_out_earlier_sections_first() {
        let s = vec![
            Section::required("Task", Placement::User, "t".repeat(10)),
            Section::optional("Turn 1", 2, "a".repeat(20)).with_placement(Placement::Assistant),
            Section::optional("Feedback 1", 2, "f".repeat(20)),
            Section::required("Turn 2", Placement::Assistant, "b".repeat(20)),
            Section::required("Feedback 2", Placement::User, "g".repeat(20)),
        ];
        let p = plan(&s, &mut Chars, 150, 60, None).unwrap();
        assert_eq!(
            p.omitted,
            vec![("Turn 1".to_owned(), 20), ("Feedback 1".to_owned(), 20)]
        );
    }

    #[test]
    fn a_conversation_alternates_user_and_assistant_messages() {
        let s = [
            Section::required("Instructions", Placement::System, "sys"),
            Section::required("Task", Placement::User, "task"),
            Section::required("Spec", Placement::User, "spec"),
            Section::required("Answer", Placement::Assistant, "code"),
            Section::required("Feedback", Placement::User, "failed"),
        ];
        let refs: Vec<&Section> = s.iter().collect();
        let m = messages(&refs);
        let roles: Vec<&str> = m.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["system", "user", "assistant", "user"]);
        assert_eq!(m[1].content, "task\n\nspec");
        assert_eq!(m[2].content, "code");
        assert_eq!(m[3].content, "failed");
    }

    #[test]
    fn messages_join_sections_by_placement() {
        let s = sections();
        let refs: Vec<&Section> = s.iter().collect();
        let m = messages(&refs);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].role, "system");
        assert!(m[1].content.starts_with("ddd") && m[1].content.ends_with("eee"));
    }
}
