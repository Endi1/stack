use std::collections::HashMap;
use std::env;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::str;

// --- Custom Error Type ---
#[derive(Debug)]
struct StackError(String);

impl fmt::Display for StackError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for StackError {}

fn err(msg: &str) -> Box<dyn Error> {
    Box::new(StackError(msg.to_string()))
}

type StackResult<T> = Result<T, Box<dyn Error>>;

fn prompt(message: &str) -> StackResult<String> {
    print!("{}", message);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn prompt_multiline(message: &str) -> StackResult<String> {
    println!("{} (enter empty line to finish):", message);
    let mut lines = Vec::new();
    loop {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        let trimmed = line.trim_end_matches('\n').to_string();
        if trimmed.is_empty() {
            break;
        }
        lines.push(trimmed);
    }
    Ok(lines.join("\n"))
}

// --- Git Helpers ---

fn run_command(cmd: &str, args: &[&str]) -> StackResult<String> {
    // println!("> {} {}", cmd, args.join(" ")); // Uncomment for debug
    let output = Command::new(cmd)
        .args(args)
        .stdin(Stdio::inherit())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("{}", stderr);
        return Err(err(&format!("Command failed: {} {}", cmd, args.join(" "))));
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn git(args: &[&str]) -> StackResult<String> {
    run_command("git", args)
}

fn git_passthrough(args: &[&str]) -> StackResult<()> {
    let status = Command::new("git")
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if !status.success() {
        return Err(err("Git command failed"));
    }
    Ok(())
}

fn get_current_branch() -> StackResult<String> {
    git(&["branch", "--show-current"])
}

// --- Logic ---

fn get_child_map() -> StackResult<HashMap<String, Vec<String>>> {
    let raw = match git(&["config", "--get-regexp", "branch\\..*\\.stack-parent"]) {
        Ok(out) => out,
        Err(_) => return Ok(HashMap::new()),
    };

    let mut map: HashMap<String, Vec<String>> = HashMap::new();

    for line in raw.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 2 {
            continue;
        }

        let key = parts[0];
        let parent = parts[1];

        if let Some(without_prefix) = key.strip_prefix("branch.") {
            if let Some(child) = without_prefix.strip_suffix(".stack-parent") {
                map.entry(parent.to_string())
                    .or_default()
                    .push(child.to_string());
            }
        }
    }
    Ok(map)
}

fn recursive_rebase(current: &str, child_map: &HashMap<String, Vec<String>>) -> StackResult<()> {
    let children = match child_map.get(current) {
        Some(c) => c,
        None => return Ok(()),
    };

    for child in children {
        println!("   -> Rebase {} onto {}", child, current);
        git(&["checkout", child])?;
        git(&["rebase", current])?;
        recursive_rebase(child, child_map)?;
    }
    Ok(())
}

// --- Commands ---

fn cmd_new(args: &[String]) -> StackResult<()> {
    if args.is_empty() {
        return Err(err("Usage: stack new <branch-name>"));
    }
    let name = &args[0];

    let parent = get_current_branch()?;
    println!("Creating branch '{}' tracking parent '{}'", name, parent);

    git(&["checkout", "-b", name])?;
    git(&["config", &format!("branch.{}.stack-parent", name), &parent])?;

    Ok(())
}

fn cmd_switch(args: &[String]) -> StackResult<()> {
    if args.is_empty() {
        return Err(err("Usage: stack switch <branch-name>"));
    }
    let name = &args[0];

    // We use passthrough so users see the nice git output (colors, info)
    git_passthrough(&["checkout", name])
}

fn cmd_submit() -> StackResult<()> {
    let current = get_current_branch()?;
    let parent = git(&["config", &format!("branch.{}.stack-parent", current)])
        .unwrap_or_else(|_| "main".to_string());

    println!("Pushing {}...", current);
    git(&["push", "origin", &current, "--force-with-lease"])?;

    // Check if PR already exists
    let pr_exists = run_command("gh", &["pr", "view", &current]).is_ok();

    if pr_exists {
        run_command("gh", &["pr", "edit", &current, "--base", &parent])?;
        println!("Updated existing PR base to {}", parent);
    } else {
        println!("Creating PR against {}...", parent);

        let title = prompt("PR Title: ")?;
        let body = prompt_multiline("PR Description")?;

        let mut gh_args = vec![
            "pr", "create", "--base", &parent, "--head", &current, "--title", &title,
        ];

        if body.is_empty() {
            gh_args.extend_from_slice(&["--body", ""]);
        } else {
            gh_args.extend_from_slice(&["--body", &body])
        }

        run_command("gh", &gh_args)?;
        println!("PR created!");
    }

    Ok(())
}

fn cmd_restack() -> StackResult<()> {
    let start_branch = get_current_branch()?;
    let child_map = get_child_map()?;

    println!("Restacking children of {}...", start_branch);
    recursive_rebase(&start_branch, &child_map)?;

    println!("Done. Returning to {}", start_branch);
    git(&["checkout", &start_branch])?;
    Ok(())
}

fn cmd_amend() -> StackResult<()> {
    println!("Amending...");
    git_passthrough(&["commit", "--amend", "--no-edit"])?;
    cmd_restack()
}

fn cmd_log() -> StackResult<()> {
    let current = get_current_branch()?;
    let child_map = get_child_map()?;

    // Find the root of the stack (walk up parents)
    let mut root = current.clone();
    loop {
        match git(&["config", &format!("branch.{}.stack-parent", root)]) {
            Ok(parent) => root = parent,
            Err(_) => break,
        }
    }

    // Print the tree starting from root
    println!();
    print_tree(&root, &current, &child_map, "", true)?;
    println!();

    Ok(())
}

fn print_tree(
    branch: &str,
    current: &str,
    child_map: &HashMap<String, Vec<String>>,
    prefix: &str,
    is_last: bool,
) -> StackResult<()> {
    let connector = if prefix.is_empty() {
        ""
    } else if is_last {
        "└── "
    } else {
        "├── "
    };
    let marker = if branch == current { " ◀" } else { "" };

    // Get short commit info
    let commit_info = git(&["log", "-1", "--format=%h %s", branch]).unwrap_or_default();

    println!("{}{}{}{}", prefix, connector, branch, marker);
    println!("{}    {}", prefix, commit_info);

    if let Some(children) = child_map.get(branch) {
        let new_prefix = if prefix.is_empty() {
            "".to_string()
        } else if is_last {
            format!("{}    ", prefix)
        } else {
            format!("{}│   ", prefix)
        };

        for (i, child) in children.iter().enumerate() {
            let child_is_last = i == children.len() - 1;
            print_tree(child, current, child_map, &new_prefix, child_is_last)?;
        }
    }

    Ok(())
}

fn cmd_land() -> StackResult<()> {
    let current = get_current_branch()?;

    // Build the stack from current back to main
    let mut stack = vec![current.clone()];
    let mut branch = current.clone();

    loop {
        match git(&["config", &format!("branch.{}.stack-parent", branch)]) {
            Ok(parent) => {
                if parent == "main" {
                    break;
                }
                // Only add if branch exists AND hasn't been merged into main yet
                if branch_exists(&parent)? && !is_merged_into_main(&parent)? {
                    stack.push(parent.clone());
                }
                branch = parent;
            }
            Err(_) => break,
        }
    }

    // Reverse so we merge bottom-up (closest to main first)
    stack.reverse();

    if stack.is_empty() {
        return Err(err("Nothing to land"));
    }

    println!("Will land the following branches into main:");
    for b in &stack {
        println!("  - {}", b);
    }

    let confirm = prompt("Proceed? [y/N] ")?;
    if confirm.to_lowercase() != "y" {
        println!("Aborted.");
        return Ok(());
    }

    // Switch to main and pull latest
    git(&["checkout", "main"])?;
    git(&["pull", "origin", "main"])?;

    for branch in &stack {
        println!("Merging {}...", branch);

        // Merge with squash or regular merge - using squash for clean history
        git(&["merge", "--squash", branch])?;

        // Get the original commit message
        let msg = git(&["log", "-1", "--format=%B", branch])?;
        git(&["commit", "-m", &msg])?;

        // Delete the branch locally and remotely
        git(&["branch", "-D", branch])?;
        let _ = git(&["push", "origin", "--delete", branch]); // Ignore if remote doesn't exist

        // Clean up the stack-parent config
        let _ = git(&[
            "config",
            "--unset",
            &format!("branch.{}.stack-parent", branch),
        ]);
    }

    println!("Pushing main...");
    git(&["push", "origin", "main"])?;

    println!("Done! Landed {} branch(es).", stack.len());
    Ok(())
}

fn branch_exists(name: &str) -> StackResult<bool> {
    Ok(git(&["rev-parse", "--verify", name]).is_ok())
}

fn is_merged_into_main(branch: &str) -> StackResult<bool> {
    // Fetch latest main first to be accurate
    let _ = git(&["fetch", "origin", "main"]);

    // Check if branch is an ancestor of main (i.e., already merged)
    Ok(git(&["merge-base", "--is-ancestor", branch, "origin/main"]).is_ok())
}

// --- Main ---

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: stack <new|switch|submit|restack|amend|log|land>");
        std::process::exit(1);
    }

    let command = &args[1];
    let remaining_args = &args[2..];

    let result = match command.as_str() {
        "new" => cmd_new(remaining_args),
        "switch" => cmd_switch(remaining_args), // Added switch command
        "submit" => cmd_submit(),
        "restack" => cmd_restack(),
        "amend" => cmd_amend(),
        "log" => cmd_log(),
        "land" => cmd_land(),
        _ => Err(err(&format!("Unknown command: {}", command))),
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
