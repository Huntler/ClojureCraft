//! A sampling grammar for generated code that makes unbalanced delimiters impossible.
//!
//! Generation answers are plain text, not JSON, because small models write noticeably worse code
//! when every line break and quote has to be escaped inside a JSON string:
//!
//! ````text
//! PLAN: how example 1's input becomes its output, then the functions to use
//! ```clojure
//! (ns …)
//! (defn …)
//! ```
//! STEPS:
//! STEP 1: (the code that carries out step 1)
//! ASSUMPTIONS:
//! - a decision the model made
//! ````
//!
//! The `STEPS:` section exists only when the specification has steps, with at most one line per
//! step. The grammar describes the code as Clojure reads it: lists, vectors, and maps must close
//! with the matching delimiter and string literals must end. The code starts with `(ns …)`, and
//! every later top-level form is a `defn`, `defn-`, `def`, or `declare` list, so a definition can
//! never lose its opening parenthesis and no example calls are left at the top level. Comments are
//! not allowed: small models tend to ramble in them until the answer is cut off.
//! Everything else (symbols, keywords, numbers, reader macros such as `#`, `'`, `@`, `^`) is an
//! atom; `#{…}`, `#(…)`, and `#"…"` read as the atom `#` before a collection or a string. So the
//! grammar does not check that code is valid Clojure, only that it can be read into balanced
//! forms, which was the most common failure of small models. Backticks are not allowed outside
//! string literals, so the closing fence is unambiguous.
//!
//! Two atoms can never follow each other without whitespace in between, so llama.cpp's grammar
//! sampler never has to track more than one way of splitting a run of characters into atoms.
//!
//! The answer is turned into the generation answer's JSON shape by
//! [`crate::answers::generation_from_text`] and validated as before.

/// Grammar rules shared by every generation answer; `root` is added by [`generation`].
const RULES: &str = r#"plan ::= [^`\x00]{0,1000}
lchar ::= [^`\n\x00]
assumptions ::= ( "\n- " lchar{1,200} ){0,4}

top ::= sp "(ns " seq ")" sp ( "(" ( "defn-" | "defn" | "def" | "declare" ) sp1 seq ")" sp )+
seq ::= sp rest
rest ::= ( coll seq | cstr seq | atom after )?
after ::= ( sp1 rest | coll seq | cstr seq )?
coll ::= "(" seq ")" | "[" seq "]" | "{" seq "}"
sp ::= wsu*
sp1 ::= wsu+
wsu ::= [ ,\t\n\r]
cstr ::= "\"" ( [^"\\\x00] | "\\" [^\x00] )* "\""
atom ::= tok | chr
tok ::= tchar+
tchar ::= [^()\[\]{}"\\;,` \t\n\r\x00]
chr ::= "\\" ( [a-zA-Z0-9]+ | [^a-zA-Z0-9 \t\n\r\x00] )
"#;

/// The generation grammar for a specification with `steps` transformation steps: at most one
/// step line per step, and no step section without steps, so the model does not spend its answer
/// budget quoting its code a second time.
pub fn generation(steps: usize) -> String {
    let step_section = match steps {
        0 => String::new(),
        n => format!(
            " \"\\nSTEPS:\" ( \"\\nSTEP \" [1-9] [0-9]? \": \" lchar{{1,200}} ){{0,{}}}",
            n.min(20)
        ),
    };
    format!(
        "root ::= \"PLAN: \" plan \"```clojure\\n\" top \"```\"{step_section} \"\\nASSUMPTIONS:\" assumptions\n{RULES}"
    )
}

#[cfg(test)]
mod tests {
    //! A small GBNF interpreter, enough for this grammar, so the grammar is tested on real
    //! answers without a model. It parses the rules into alternatives of sequences and matches
    //! with a backtracking search over all ways each rule can end.

    use std::collections::HashMap;

    use super::generation;

    #[derive(Clone, Debug)]
    enum Item {
        Lit(Vec<char>),
        Class {
            negated: bool,
            ranges: Vec<(char, char)>,
        },
        Rule(String),
        Group(Vec<Vec<Rep>>),
    }

    #[derive(Clone, Debug)]
    struct Rep {
        item: Item,
        min: usize,
        max: Option<usize>,
    }

    struct P<'a> {
        s: &'a [char],
        i: usize,
    }

    impl P<'_> {
        fn peek(&self) -> Option<char> {
            self.s.get(self.i).copied()
        }
        fn skip_ws(&mut self) {
            while matches!(self.peek(), Some(' ' | '\t')) {
                self.i += 1;
            }
        }
        fn escaped(&mut self) -> char {
            let c = self.s[self.i];
            self.i += 1;
            if c != '\\' {
                return c;
            }
            let e = self.s[self.i];
            self.i += 1;
            match e {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                'x' => {
                    let h: String = self.s[self.i..self.i + 2].iter().collect();
                    self.i += 2;
                    char::from_u32(u32::from_str_radix(&h, 16).unwrap()).unwrap()
                }
                other => other,
            }
        }
        fn alternatives(&mut self) -> Vec<Vec<Rep>> {
            let mut alts = vec![self.sequence()];
            while self.peek() == Some('|') {
                self.i += 1;
                alts.push(self.sequence());
            }
            alts
        }
        fn sequence(&mut self) -> Vec<Rep> {
            let mut seq = Vec::new();
            loop {
                self.skip_ws();
                let item = match self.peek() {
                    Some('"') => {
                        self.i += 1;
                        let mut lit = Vec::new();
                        while self.peek() != Some('"') {
                            lit.push(self.escaped());
                        }
                        self.i += 1;
                        Item::Lit(lit)
                    }
                    Some('[') => {
                        self.i += 1;
                        let negated = self.peek() == Some('^');
                        if negated {
                            self.i += 1;
                        }
                        let mut ranges = Vec::new();
                        while self.peek() != Some(']') {
                            let a = self.escaped();
                            if self.peek() == Some('-') && self.s.get(self.i + 1) != Some(&']') {
                                self.i += 1;
                                let b = self.escaped();
                                ranges.push((a, b));
                            } else {
                                ranges.push((a, a));
                            }
                        }
                        self.i += 1;
                        Item::Class { negated, ranges }
                    }
                    Some('(') => {
                        self.i += 1;
                        let alts = self.alternatives();
                        self.skip_ws();
                        assert_eq!(self.peek(), Some(')'));
                        self.i += 1;
                        Item::Group(alts)
                    }
                    Some(c) if c.is_ascii_alphanumeric() || c == '-' => {
                        let start = self.i;
                        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric() || c == '-') {
                            self.i += 1;
                        }
                        Item::Rule(self.s[start..self.i].iter().collect())
                    }
                    _ => return seq,
                };
                let (min, max) = match self.peek() {
                    Some('*') => (0, None),
                    Some('+') => (1, None),
                    Some('?') => (0, Some(1)),
                    Some('{') => {
                        self.i += 1;
                        let close = self.s[self.i..].iter().position(|c| *c == '}').unwrap();
                        let body: String = self.s[self.i..self.i + close].iter().collect();
                        self.i += close;
                        let (a, b) = body.split_once(',').unwrap_or((&body, &body));
                        (a.parse().unwrap(), b.parse().ok())
                    }
                    _ => {
                        seq.push(Rep {
                            item,
                            min: 1,
                            max: Some(1),
                        });
                        continue;
                    }
                };
                self.i += 1;
                seq.push(Rep { item, min, max });
            }
        }
    }

    fn rules(text: &str) -> HashMap<String, Vec<Vec<Rep>>> {
        let mut out = HashMap::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let (name, body) = line.split_once("::=").unwrap();
            let chars: Vec<char> = body.chars().collect();
            let mut p = P { s: &chars, i: 0 };
            let alts = p.alternatives();
            p.skip_ws();
            assert_eq!(p.i, chars.len(), "rule {name} was not read to its end");
            out.insert(name.trim().to_owned(), alts);
        }
        out
    }

    /// Every position where matching `item` from `at` can end.
    fn item_ends(g: &HashMap<String, Vec<Vec<Rep>>>, item: &Item, s: &[char], at: usize) -> Vec<usize> {
        match item {
            Item::Lit(l) => {
                if s[at..].starts_with(l) {
                    vec![at + l.len()]
                } else {
                    vec![]
                }
            }
            Item::Class { negated, ranges } => match s.get(at) {
                Some(c) if ranges.iter().any(|(a, b)| a <= c && c <= b) != *negated => vec![at + 1],
                _ => vec![],
            },
            Item::Rule(r) => alts_ends(g, &g[r], s, at),
            Item::Group(alts) => alts_ends(g, alts, s, at),
        }
    }

    fn alts_ends(g: &HashMap<String, Vec<Vec<Rep>>>, alts: &[Vec<Rep>], s: &[char], at: usize) -> Vec<usize> {
        let mut out: Vec<usize> = alts.iter().flat_map(|seq| seq_ends(g, seq, s, at)).collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    fn seq_ends(g: &HashMap<String, Vec<Vec<Rep>>>, seq: &[Rep], s: &[char], at: usize) -> Vec<usize> {
        let mut here = vec![at];
        for rep in seq {
            let mut next = Vec::new();
            for &start in &here {
                let mut frontier = vec![start];
                let mut count = 0;
                if rep.min == 0 {
                    next.push(start);
                }
                while !frontier.is_empty() && rep.max.is_none_or(|m| count < m) {
                    let mut grown: Vec<usize> = frontier
                        .iter()
                        .flat_map(|&p| item_ends(g, &rep.item, s, p))
                        .filter(|&e| e > start || rep.max == Some(1))
                        .collect();
                    grown.sort_unstable();
                    grown.dedup();
                    count += 1;
                    if count >= rep.min {
                        next.extend(grown.iter().copied());
                    }
                    frontier = grown;
                }
            }
            next.sort_unstable();
            next.dedup();
            here = next;
        }
        here
    }

    fn matches(grammar: &str, answer: &str) -> bool {
        let g = rules(grammar);
        let chars: Vec<char> = answer.chars().collect();
        alts_ends(&g, &g["root"], &chars, 0).contains(&chars.len())
    }

    fn accepts(code: &str) -> bool {
        let answer = format!(
            "PLAN: sum the values\n```clojure\n{code}\n```\nSTEPS:\nSTEP 1: (inc x)\nASSUMPTIONS:\n- the input is a vector"
        );
        matches(&generation(2), &answer)
    }

    #[test]
    fn balanced_code_with_strings_chars_regexes_and_comments_is_accepted() {
        for code in [
            "(ns totals.core\n  (:require [clojure.string :as str]))\n\n(defn totals [xs] (reduce + 0 xs))\n",
            "(ns a)\n(defn- helper [m] {:a 1, :b #{2 3}})\n(defn f [x] #(inc %))",
            "(ns a)\n(defn f [s] (str/split s #\"\\s*,\\s*\"))",
            "(ns a)\n(defn f [s] (str \"say \\\"hi\\\" (\" \\( \\newline \\\" \\\\ \"\\n\"))",
            "(ns a)\n(defn f [x] (let [{:keys [a b]} x] ^long (+ a b)))\n(def ^:private n 'sym)\n(defn g [x] @x)",
            "(ns a)\n(defn f [x]\n\t(case x \"a\" 1 2))",
            "(ns a)\n(defn f [s] (str \"a line\nand a ``` fence inside a string\"))",
        ] {
            assert!(accepts(code), "rejected: {code}");
        }
    }

    #[test]
    fn unbalanced_or_unterminated_code_is_rejected() {
        for code in [
            "(ns a)\n(defn f [x] (inc x)",
            "(ns a)\n(defn f [x] x))",
            "(ns a)\n(defn f [x) x)",
            "(ns a)\n(defn f [x] {:a 1)",
            "(ns a)\n(defn f [x] (str \"open x))",
            "(ns a)\n\ndefn- g [x]\n  (inc x)\n(defn f [x] (g x))",
            "(ns a)\n(defn f [x] `(inc ~x))",
            "(ns a)\n;; a comment\n(defn f [x] x)",
            "(ns a)\n(defn f [x] x)\n(f 1)",
            "(defn f [x] x)",
            "(ns a)",
        ] {
            assert!(!accepts(code), "accepted: {code}");
        }
    }

    #[test]
    fn the_sections_around_the_code_are_fixed_in_shape() {
        let ok = "PLAN: p\n```clojure\n(ns a)\n(def x 1)\n```\nASSUMPTIONS:";
        assert!(matches(&generation(0), ok));
        assert!(matches(
            &generation(0),
            "PLAN: p\n```clojure\n(ns a)\n(def x 1)\n```\nASSUMPTIONS:\n- one\n- two"
        ));
        assert!(!matches(&generation(0), "PLAN: p\n```clojure\n(ns a)\n```"));
        assert!(!matches(&generation(0), "PLAN: p\n```clojure\n```\nASSUMPTIONS:"));
        assert!(!matches(
            &generation(0),
            "PLAN: uses `x`\n```clojure\n(ns a)\n(def x 1)\n```\nASSUMPTIONS:"
        ));
        assert!(!matches(
            &generation(0),
            "PLAN: p\n```clojure\n(ns a)\n(def x 1)\n```\nSTEPS:\nSTEP 1: x\nASSUMPTIONS:"
        ));
        let one_step = "PLAN: p\n```clojure\n(ns a)\n(def x 1)\n```\nSTEPS:\nSTEP 1: (inc x)\nASSUMPTIONS:";
        assert!(matches(&generation(1), one_step));
        assert!(!matches(
            &generation(1),
            "PLAN: p\n```clojure\n(ns a)\n(def x 1)\n```\nSTEPS:\nSTEP 1: a\nSTEP 2: b\nASSUMPTIONS:"
        ));
    }
}
