use crate::config::Snippet;

pub const MAX_TRIGGER_LEN: usize = 64;
const BUFFER_LEN: usize = MAX_TRIGGER_LEN * 2;

/// Rolling record of the characters typed since the last reset.
#[derive(Default)]
pub struct Buffer {
    chars: Vec<char>,
}

impl Buffer {
    pub fn push(&mut self, c: char) {
        if self.chars.len() == BUFFER_LEN {
            self.chars.drain(..MAX_TRIGGER_LEN);
        }
        self.chars.push(c);
    }

    pub fn pop(&mut self) {
        self.chars.pop();
    }

    pub fn clear(&mut self) {
        self.chars.clear();
    }

    /// Returns the first snippet (callers pass them longest-trigger first) whose
    /// trigger ends the buffer. Triggers starting with a letter or digit must also
    /// start a word, so `rg` doesn't fire inside `forget`.
    pub fn find<'a>(&self, snippets: &'a [Snippet]) -> Option<&'a Snippet> {
        snippets.iter().find(|s| {
            let trigger: Vec<char> = s.trigger.chars().collect();
            if !self.chars.ends_with(&trigger) {
                return false;
            }
            let start = self.chars.len() - trigger.len();
            !trigger[0].is_alphanumeric() || start == 0 || !self.chars[start - 1].is_alphanumeric()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snippets() -> Vec<Snippet> {
        let mut v: Vec<Snippet> = [("rg", "ripgrep"), ("rgx", "regex"), (";sig", "Best")]
            .iter()
            .map(|(t, s)| Snippet { trigger: t.to_string(), snippet: s.to_string() })
            .collect();
        v.sort_by_key(|s| std::cmp::Reverse(s.trigger.len()));
        v
    }

    fn typed(text: &str) -> Buffer {
        let mut b = Buffer::default();
        text.chars().for_each(|c| b.push(c));
        b
    }

    fn hit(text: &str) -> Option<String> {
        typed(text).find(&snippets()).map(|s| s.snippet.clone())
    }

    #[test]
    fn fires_at_word_start() {
        assert_eq!(hit("rg").as_deref(), Some("ripgrep"));
        assert_eq!(hit("use rg").as_deref(), Some("ripgrep"));
        assert_eq!(hit("(rg").as_deref(), Some("ripgrep"));
    }

    #[test]
    fn ignores_mid_word() {
        assert_eq!(hit("forg"), None);
        assert_eq!(hit("r"), None);
    }

    #[test]
    fn punctuation_triggers_fire_anywhere() {
        assert_eq!(hit("hi;sig").as_deref(), Some("Best"));
    }

    #[test]
    fn longest_trigger_wins() {
        assert_eq!(hit("rgx").as_deref(), Some("regex"));
    }

    #[test]
    fn backspace_and_clear() {
        let mut b = typed("fr");
        b.pop();
        b.pop();
        b.push('r');
        b.push('g');
        assert!(b.find(&snippets()).is_some());
        b.clear();
        b.push('g');
        assert!(b.find(&snippets()).is_none());
    }

    #[test]
    fn long_input_keeps_working() {
        let mut b = typed(&"x".repeat(1000));
        b.push(' ');
        b.push('r');
        b.push('g');
        assert!(b.find(&snippets()).is_some());
    }
}
