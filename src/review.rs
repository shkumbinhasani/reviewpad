use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Old,
    New,
}

impl Side {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Old => "old",
            Self::New => "new",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewComment {
    pub path: String,
    pub side: Side,
    pub line: u32,
    pub body: String,
    pub context: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Review {
    pub comments: Vec<ReviewComment>,
}

impl Review {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let bytes = fs::read(path)
            .with_context(|| format!("failed to read review at {}", path.display()))?;
        serde_json::from_slice(&bytes)
            .with_context(|| format!("failed to parse review at {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let json = serde_json::to_vec_pretty(self)?;
        fs::write(path, json)
            .with_context(|| format!("failed to save review at {}", path.display()))
    }

    pub fn markdown(&self, repository: &Path) -> String {
        let mut output = format!(
            "# Code review\n\nRepository: `{}`\n\n",
            repository.display()
        );

        if self.comments.is_empty() {
            output.push_str("No review comments.\n");
            return output;
        }

        output.push_str("Please address every item below. Preserve unrelated changes and run the relevant tests when finished.\n\n");
        for (index, comment) in self.comments.iter().enumerate() {
            output.push_str(&format!(
                "## {}. `{}:{} ({})`\n\n{}\n\n",
                index + 1,
                comment.path,
                comment.line,
                comment.side.label(),
                comment.body.trim()
            ));
            if !comment.context.trim().is_empty() {
                output.push_str("```diff\n");
                output.push_str(comment.context.trim_end());
                output.push_str("\n```\n\n");
            }
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_agent_ready() {
        let review = Review {
            comments: vec![ReviewComment {
                path: "src/lib.rs".into(),
                side: Side::New,
                line: 12,
                body: "Handle the error instead of unwrapping.".into(),
                context: "+let value = thing.unwrap();".into(),
            }],
        };
        let text = review.markdown(Path::new("/tmp/project"));
        assert!(text.contains("`src/lib.rs:12 (new)`"));
        assert!(text.contains("Handle the error"));
        assert!(text.contains("```diff"));
    }
}
