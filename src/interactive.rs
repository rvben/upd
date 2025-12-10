use colored::Colorize;
use std::io::{self, BufRead, Write};

/// User's decision for an update
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Apply this update
    Yes,
    /// Skip this update
    No,
    /// Apply all remaining updates
    All,
    /// Skip all remaining updates and finish
    Quit,
}

/// Response from prompting user about updates
#[derive(Debug)]
pub enum PromptResult {
    /// Individual decisions for each update
    Decisions(Vec<Decision>),
    /// Apply all updates (user selected 'a' early)
    ApplyAll,
    /// Quit without applying remaining (user selected 'q')
    Quit,
}

/// Prompt the user for a single update decision
pub fn prompt_single(
    file: &str,
    line_num: Option<usize>,
    package: &str,
    old_version: &str,
    new_version: &str,
    is_major: bool,
) -> io::Result<Decision> {
    // Format location
    let location = match line_num {
        Some(n) => format!("{}:{}:", file, n),
        None => format!("{}:", file),
    };

    // Build the prompt line
    let type_indicator = if is_major {
        " (MAJOR)".yellow().bold().to_string()
    } else {
        String::new()
    };

    print!(
        "{} {} {} → {}{}\n  Apply? [{}]es / [{}]o / [{}]ll / [{}]uit: ",
        location.blue().underline(),
        package.bold(),
        old_version.dimmed(),
        new_version.green(),
        type_indicator,
        "y".green().bold(),
        "n".red().bold(),
        "a".cyan().bold(),
        "q".yellow().bold(),
    );
    io::stdout().flush()?;

    // Read user input
    let stdin = io::stdin();
    let mut input = String::new();
    stdin.lock().read_line(&mut input)?;

    let input = input.trim().to_lowercase();

    match input.as_str() {
        "y" | "yes" | "" => Ok(Decision::Yes), // default to yes on empty input
        "n" | "no" => Ok(Decision::No),
        "a" | "all" => Ok(Decision::All),
        "q" | "quit" => Ok(Decision::Quit),
        _ => {
            // Invalid input, default to no
            println!("{}", "Invalid input, skipping...".yellow());
            Ok(Decision::No)
        }
    }
}

/// Represents a pending update that can be approved or rejected
#[derive(Debug, Clone)]
pub struct PendingUpdate {
    pub file: String,
    pub line_num: Option<usize>,
    pub package: String,
    pub old_version: String,
    pub new_version: String,
    pub is_major: bool,
    pub approved: bool,
}

impl PendingUpdate {
    pub fn new(
        file: String,
        line_num: Option<usize>,
        package: String,
        old_version: String,
        new_version: String,
        is_major: bool,
    ) -> Self {
        Self {
            file,
            line_num,
            package,
            old_version,
            new_version,
            is_major,
            approved: false,
        }
    }
}

/// Run interactive prompts for all pending updates
/// Returns the updates with their approval status set
pub fn prompt_all(mut updates: Vec<PendingUpdate>) -> io::Result<Vec<PendingUpdate>> {
    if updates.is_empty() {
        return Ok(updates);
    }

    let total = updates.len();
    println!("\n{} {} update(s) available\n", "?".cyan().bold(), total);

    for (i, update) in updates.iter_mut().enumerate() {
        // Show progress
        print!("[{}/{}] ", i + 1, total);

        let decision = prompt_single(
            &update.file,
            update.line_num,
            &update.package,
            &update.old_version,
            &update.new_version,
            update.is_major,
        )?;

        match decision {
            Decision::Yes => {
                update.approved = true;
            }
            Decision::No => {
                update.approved = false;
            }
            Decision::All => {
                // Approve this and all remaining updates
                update.approved = true;
                for remaining in updates.iter_mut().skip(i + 1) {
                    remaining.approved = true;
                }
                println!("{}", "Applying all remaining updates...".cyan());
                break;
            }
            Decision::Quit => {
                // Keep current update as not approved, stop prompting
                update.approved = false;
                println!("{}", "Skipping remaining updates...".yellow());
                break;
            }
        }
    }

    Ok(updates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pending_update_new() {
        let update = PendingUpdate::new(
            "test.txt".to_string(),
            Some(10),
            "flask".to_string(),
            "2.0.0".to_string(),
            "3.0.0".to_string(),
            true,
        );

        assert_eq!(update.file, "test.txt");
        assert_eq!(update.line_num, Some(10));
        assert_eq!(update.package, "flask");
        assert_eq!(update.old_version, "2.0.0");
        assert_eq!(update.new_version, "3.0.0");
        assert!(update.is_major);
        assert!(!update.approved); // default not approved
    }

    #[test]
    fn test_pending_update_no_line_num() {
        let update = PendingUpdate::new(
            "Cargo.toml".to_string(),
            None,
            "serde".to_string(),
            "1.0.0".to_string(),
            "1.1.0".to_string(),
            false,
        );

        assert_eq!(update.file, "Cargo.toml");
        assert_eq!(update.line_num, None);
        assert!(!update.is_major);
    }

    #[test]
    fn test_prompt_all_empty() {
        let updates: Vec<PendingUpdate> = vec![];
        let result = prompt_all(updates).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_decision_enum() {
        assert_eq!(Decision::Yes, Decision::Yes);
        assert_ne!(Decision::Yes, Decision::No);
        assert_ne!(Decision::All, Decision::Quit);
    }
}
