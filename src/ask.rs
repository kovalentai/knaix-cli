//! Questions a command asks the person running it.
//!
//! Behind a trait so a command's choices can be tested: the terminal in use,
//! scripted answers in tests.

use anyhow::Result;

/// How a command asks the person running it: the terminal in use, scripted
/// answers in tests.
pub(crate) trait Ask {
    /// One of `items`, or None when the person backs out. `fuzzy` lets them
    /// type to narrow a long list.
    fn pick(&self, prompt: &str, items: &[String], default: usize, fuzzy: bool) -> Option<usize>;
    /// A line of text, which may be empty only when `allow_empty`.
    fn text(&self, prompt: &str, allow_empty: bool) -> Result<String>;
    /// Yes or no: `default` on a bare Enter, and no when the prompt fails.
    fn confirm(&self, prompt: &str, default: bool) -> bool;
}

/// The terminal, through dialoguer.
pub(crate) struct Terminal;

impl Ask for Terminal {
    fn pick(&self, prompt: &str, items: &[String], default: usize, fuzzy: bool) -> Option<usize> {
        use crossterm::{cursor, execute};
        use dialoguer::{theme::ColorfulTheme, FuzzySelect, Select};
        let theme = ColorfulTheme::default();
        let _ = execute!(std::io::stderr(), cursor::Hide);
        let pick = if fuzzy {
            FuzzySelect::with_theme(&theme)
                .with_prompt(prompt)
                .items(items)
                .default(default)
                .interact_opt()
        } else {
            Select::with_theme(&theme)
                .with_prompt(prompt)
                .items(items)
                .default(default)
                .interact_opt()
        };
        let _ = execute!(std::io::stderr(), cursor::Show);
        pick.unwrap_or(None)
    }

    fn text(&self, prompt: &str, allow_empty: bool) -> Result<String> {
        use dialoguer::{theme::ColorfulTheme, Input};
        Ok(Input::<String>::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt)
            .allow_empty(allow_empty)
            .interact_text()?)
    }

    fn confirm(&self, prompt: &str, default: bool) -> bool {
        use dialoguer::{theme::ColorfulTheme, Confirm};
        Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt(prompt)
            .default(default)
            .interact()
            .unwrap_or(false)
    }
}

/// Scripted answers, for tests.
#[cfg(test)]
// Text and yes-or-no answers are asked for by Unix-only tests.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) mod scripted {
    use super::Ask;
    use anyhow::{anyhow, Result};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    #[derive(Debug)]
    pub(crate) enum Answer {
        Pick(Option<usize>),
        Text(String),
        /// The text prompt fails, as it does when the terminal goes away.
        Broken,
        Confirm(bool),
    }
    use Answer::{Broken, Confirm, Pick, Text};

    /// What was asked: the prompt, and for a pick its items, the one the
    /// cursor started on and whether it filters.
    #[derive(Debug, Default)]
    pub(crate) struct Asked {
        pub(crate) prompt: String,
        pub(crate) items: Vec<String>,
        pub(crate) default: usize,
        pub(crate) fuzzy: bool,
    }

    /// Answers in order, each checked against the kind of question.
    pub(crate) struct Scripted {
        answers: RefCell<VecDeque<Answer>>,
        pub(crate) asked: RefCell<Vec<Asked>>,
    }

    pub(crate) fn script(answers: Vec<Answer>) -> Scripted {
        Scripted {
            answers: RefCell::new(answers.into()),
            asked: RefCell::new(Vec::new()),
        }
    }

    impl Scripted {
        fn next(&self, asked: Asked) -> Answer {
            let answer = self.answers.borrow_mut().pop_front();
            let answer = answer.unwrap_or_else(|| panic!("unscripted: {asked:?}"));
            self.asked.borrow_mut().push(asked);
            answer
        }

        pub(crate) fn prompts(&self) -> Vec<String> {
            self.asked
                .borrow()
                .iter()
                .map(|a| a.prompt.clone())
                .collect()
        }
    }

    impl Ask for Scripted {
        fn pick(
            &self,
            prompt: &str,
            items: &[String],
            default: usize,
            fuzzy: bool,
        ) -> Option<usize> {
            let asked = Asked {
                prompt: prompt.into(),
                items: items.to_vec(),
                default,
                fuzzy,
            };
            match self.next(asked) {
                Pick(p) => p,
                other => panic!("{prompt:?} answered with {other:?}"),
            }
        }

        fn text(&self, prompt: &str, _allow_empty: bool) -> Result<String> {
            match self.next(Asked {
                prompt: prompt.into(),
                ..Asked::default()
            }) {
                Text(t) => Ok(t),
                Broken => Err(anyhow!("the terminal went away")),
                other => panic!("{prompt:?} answered with {other:?}"),
            }
        }

        fn confirm(&self, prompt: &str, _default: bool) -> bool {
            match self.next(Asked {
                prompt: prompt.into(),
                ..Asked::default()
            }) {
                Confirm(c) => c,
                other => panic!("{prompt:?} answered with {other:?}"),
            }
        }
    }
}
